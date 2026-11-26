# Session data framing and log format design

Status: implemented 2026-11-26. Decisions resolved; see §16 and §17.

This document proposes one framing scheme for every session data path (serial,
RTT, and DAP) and one saved log format built from the same frames. It supersedes
the framing and log-record sections of
[api-architecture-design.md](api-architecture-design.md) (§10.4, §11.2, §14.4),
the RTT data-message section of
[rtt-artifact-decoding-design.md](rtt-artifact-decoding-design.md) (§6.3), and
the DAP data-path framing and recording sections of
[dap-design.md](dap-design.md) (§7.3, §7.7, §8.1). DAP lifecycle policy
(exclusive subscriber, non-resumable, close-on-detach) is unchanged.

## 1. Summary

Today the live data plane has three different shapes:

- serial uses a binary frame with a sequence number and no stream byte;
- RTT uses text JSON messages (`rtt.records`, `rtt.raw`, `rtt.decode_error`) and
  never uses a binary frame;
- DAP uses a binary frame whose single byte means direction, not channel.

Client input is also inconsistent: serial and DAP send raw bytes with no frame,
RTT silently ignores binary input, and DAP input is logged with a different
record layout from serial input.

The saved log is a separate, ad-hoc format: a per-record `BLOG` header, a `u8`
record kind, a `u8` channel whose meaning depends on the session kind, no file
header, no CRC, and a sidecar `RECORD_DECODED` JSON record keyed by sequence.
DAP direction is stored twice (record kind and channel).

This design replaces all of that with one versioned binary frame:

```text
magic | version | flags | record type | direction | stream | seq |
timestamp | payload length | CRC32C | payload
```

The same frame is used:

- server to client on the session data WebSocket;
- client to server for session input;
- one record at a time in the append-only session log.

The session kind is metadata, not part of the frame. A raw payload is opaque to
the framing layer; serial bytes, RTT bytes, and DAP protocol bytes all travel the
same way. RTT decoded output is an explicit frame record type with a JSON
payload, not a text WebSocket message. Input is an explicit direction, not a
separate message shape.

The log becomes a stream of the same frames, beginning with a session-header
frame. It is self-describing (kind, session ID, creation time), checksummed, and
streamable. There is no on-disk compatibility requirement: there are no
deployments, so the wire and log formats change in place without a protocol
version bump.

## 2. Goals

- One frame envelope for serial, RTT, and DAP data in both directions.
- One codec in `boot-protocol` shared by server, CLI, and tests.
- No session-kind string switches in the framing layer.
- A saved log that is a stream of the same frames, self-describing and
  checksummed.
- Explicit direction and stream fields; no overloading one byte.
- One monotonic sequence per session for every persisted frame, including input
  and lifecycle marks.
- Explicit gap records; clients never infer loss from a sequence jump.
- RTT decoded output and raw fallback in one frame, without a sidecar keyed by
  sequence.
- A generic WebSocket handler for all session kinds, with kind-specific policy
  confined to limits and lifecycle.
- No change to the DAP wire seen by the debugger, the HTTP surface, leases,
  authentication, or artifact handling.

## 3. Non-goals

- Changing the DAP protocol or feeding a replayed transcript to a debugger.
- Changing asciicast semantics; export still renders from the log.
- Implementing log retention, quota enforcement, or segment rotation in this
  pass.
- Re-decoding historical output when an RTT decoder changes.
- Adding a global event bus or changing change-follow framing.
- Supporting mixed old/new clients. There are no deployed clients, so the
  server and CLI change in place.
- Compressing logs. Compression can be added later with a frame flag or a
  container change.

## 4. Current implementation

This section records what the code does today. Function names are the stable
reference; line numbers will drift.

### 4.1 Live data path

`boot-protocol::SessionWsMessage` is a text control enum. Binary frames are
encoded and decoded by `encode_session_frame(kind, seq, channel, data)` and
`decode_session_frame(kind, bytes)`:

```text
serial:  seq:u64 big-endian | payload
rtt:     seq:u64 big-endian | channel:u8 | payload
dap:     seq:u64 big-endian | direction:u8 | payload
```

The channel byte is included only when `kind == "rtt" || kind == "dap"`.
`SessionDataFrame.channel` is `Option<u8>`: serial has no byte at all, while
RTT and DAP have one byte that may be zero. The Rust `Option` therefore means
"this kind has no byte", not "the stream is absent".

Actual server behavior is narrower than the codec suggests:

| Kind | Server to client | Client to server | Notes |
| --- | --- | --- | --- |
| `serial` | binary `seq + payload` | raw bytes, no frame | Server assigns a sequence and logs input with channel 0. Input is not broadcast live. |
| `rtt` | text `rtt.records` / `rtt.raw` / `rtt.decode_error` | ignored when `mode=write` | The RTT binary branch of the codec is effectively unused. |
| `dap` | binary `seq + direction(0) + payload` | raw DAP bytes, no frame | Server assigns a sequence, logs input as `RECORD_INPUT` with channel 1, and forwards it to the worker. |

Evidence:

- `run.rs::send_session_event` sends `SessionEventData::Bytes` as a binary frame
  and every RTT event as text JSON.
- `run.rs::send_binary` is called with `kind = "dap"` and direction 0 for live
  DAP output.
- `run.rs::session_io_ws` only accepts client binary input for `serial` and only
  forwards input to the worker in that branch.
- `run.rs::dap_session_io_ws` accepts raw DAP bytes and calls
  `push_dap_input`.
- `run.rs::dap_replay_ws` sends both directions as binary frames with direction
  0 or 1.
- `SessionHandle::push_dap_output` ignores failure of the subscriber channel
  `try_send`, so a slow debugger socket drops output. `push_dap_input` ignores
  failure of the worker channel `try_send`, so a full worker queue drops input
  after the log and counters already recorded it.

Consequences:

- A client cannot write a generic session data loop; it must branch on kind.
- Adding a session kind means changing the codec and both ends.
- RTT input is silently dropped instead of rejected.
- DAP direction and RTT channel are the same wire field with different meanings.
- Input has no common framing, so it cannot be validated or replayed uniformly.

### 4.2 Saved log

`boot-api/src/runtime/session_log.rs` writes one file per session at
`<session_dir>/<session_id>/session.log` with directory mode `0700` and file mode
`0600`. Each record has this header:

```text
magic:         "BLOG" (4)
version:       u8 (1)
record kind:   u8     // output, input, gap, lifecycle, decoded
channel:       u8     // RTT channel; direction for DAP; 0 for serial
flags:         u16    // always zero today
seq:           u64
timestamp:     u64    // Unix milliseconds
length:        u32
payload:       [u8; length]
```

Observations:

- There is no file header. The file does not record the session ID, session
  kind, encoder version, or creation time. A reader must already know the kind;
  `SessionHandle::replay` uses `self.kind` to interpret records.
- `RECORD_DECODED` is a second JSON record with the same sequence as the raw
  `RECORD_OUTPUT` record. Replay loads the whole file, builds a `HashMap<seq,
  decoded>`, then iterates again. The rule "prefer decoded if present" is
  implicit and depends on the reader's merge logic.
- DAP direction is encoded twice: `RECORD_OUTPUT`/`RECORD_INPUT` and `channel`
  0/1. For RTT, `channel` is the RTT up-channel. For serial it is always 0.
- `RECORD_GAP` and `RECORD_LIFECYCLE` are declared but never written.
- The header has a `flags` field but no CRC field. The architecture document
  specified CRC32C; the implementation does not have it.
- `timestamp_ms` is `now_unix() * 1000`, so it has one-second resolution.
  Asciicast export computes intervals from it and loses sub-second timing.
- `SessionLogWriter::append` writes the header and payload separately and calls
  `flush` on every record. `File::flush` is not `fsync`; a crash can lose the
  tail. The reader stops silently at the first short or bad record.
- DAP is persisted, contrary to api-architecture-design.md §14.4, which says DAP
  is not persisted. The DAP design document later added DAP recording, so the
  behavior is intentional; the older statement is stale.
- `data.oldest_seq` is never updated. `data.latest_seq` is updated on output and
  DAP input but not on serial input. `data.bytes` counts DAP input but not
  serial input.
- The live ring silently evicts. `SessionWsMessage::Gap` exists and the CLI
  handles it, but the server never sends it. A subscriber that falls behind the
  ring simply receives a sequence jump and no warning.
- `restore_sessions` reads the whole log to find `latest_seq` and takes the
  maximum of the log and the database. There is no CRC check, no tail
  truncation, and no legacy detection.

## 5. Design principles

1. **One envelope, kind-neutral.** The framing layer knows direction, stream,
   sequence, time, record type, and bytes. It does not know serial, RTT, or DAP.
2. **Raw payloads are canonical.** The `DATA` record carries exactly the bytes
   the producer read or the client wrote. Derived views never replace canonical
   bytes on disk.
3. **Input is data.** Input uses the same frame and the same sequence space as
   output; it differs only in `direction`.
4. **Sequence is session-wide.** One monotonic sequence covers every persisted
   frame. A visible jump is not a gap; only a `GAP` record is a gap.
5. **Explicit derived views.** RTT decoded records are a `RTT_RECORDS` frame
   with a defined JSON payload, not a text message or a sidecar record.
6. **One codec.** WebSocket frames and log records are encoded by the same
   functions in `boot-protocol`.
7. **Self-describing storage.** The log starts with a session-header frame and
   every record is length-delimited and checksummed.
8. **Policy is separate from framing.** DAP exclusivity, ring sizes, overflow
   behavior, and authentication stay in the runtime/API layer.
9. **No compatibility shims.** Change both sides in place and do not carry a
   second wire or log format. The workspace is still green field, so
   `PROTOCOL_VERSION` stays at 1.

## 6. Frame model

### 6.1 Logical frame

```rust
pub struct SessionFrameHeader {
    pub flags: u8,
    pub record: FrameRecord,
    pub direction: FrameDirection,
    pub stream: u8,
    pub seq: u64,
    pub timestamp_ns: u64,
}

pub struct SessionFrame<'a> {
    pub header: SessionFrameHeader,
    pub payload: &'a [u8],
}
```

The runtime and ring may store an owned form:

```rust
pub struct OwnedSessionFrame {
    pub header: SessionFrameHeader,
    pub payload: Vec<u8>,
}
```

`payload_len` and `crc` are wire fields derived from the payload; they are not
part of the in-memory header. `decode_frame(bytes) -> Result<SessionFrame<'_>,
FrameError>` borrows the payload. `encode_frame(&SessionFrame) -> Vec<u8>`
returns a complete frame. `OwnedSessionFrame` converts to and from the borrowed
form without changing the wire bytes.

### 6.2 Sequence, timestamps, and cursors

- `seq` is a `u64`, assigned once per producer chunk, input event, lifecycle
  mark, or gap record. It starts at 1 and never repeats or resets.
- A raw/decoded pair for one RTT chunk shares one `seq`. They are distinguished
  by `record` and `flags`, not by sequence.
- The session-header frame uses `seq = 0` and is not part of the data sequence.
  Readers skip it.
- `timestamp_ns` is Unix time in nanoseconds, captured once per frame. A raw and
  decoded pair uses the same timestamp. The writer clamps a non-monotonic clock
  to the previous frame's timestamp so replay intervals stay non-negative.
- `latest_seq` is the highest allocated frame sequence, including input and
  marks. `oldest_seq` is the lowest sequence still replayable from the log (or
  the ring when the log is unavailable).
- A subscriber's cursor is the highest sequence it has consumed. A cursor can
  be advanced by a frame the subscriber did not display (input, marks), so a
  visible sequence jump is normal.
- **Clients must not infer a gap from a sequence jump.** The server sends an
  explicit `GAP` frame when data was lost. This is the only loss signal.

### 6.3 Direction and stream

`direction` describes who produced the bytes:

| Value | Name | Meaning |
| --- | --- | --- |
| 0 | `Output` | Device/adapter to client. Producer worker or DAP adapter. |
| 1 | `Input` | Client to device/adapter. Written by a subscriber. |

`stream` is a logical stream within the session:

| Kind | `stream` | Notes |
| --- | --- | --- |
| `serial` | 0 | One byte stream. |
| `rtt` | RTT up-channel number | Input is not supported; RTT frames are output-only. |
| `dap` | 0 | Direction distinguishes the two DAP directions. |

The two concepts are separate. In particular, DAP no longer uses `stream` to
mean direction, and RTT no longer uses `direction` to mean output.

Reserved: a future kind can define more than 256 streams only by changing the
frame format. 256 is sufficient for serial consoles, RTT channels, and DAP.

### 6.4 Record types

`record` describes the payload:

| Value | Name | Payload | Live | Log |
| --- | --- | --- | --- | --- |
| 0 | `DATA` | Opaque session bytes | Yes | Yes |
| 1 | `RTT_RECORDS` | JSON decoded RTT view | Yes, when a decoder is active | Yes, as a derived companion |
| 2 | `GAP` | 24-byte gap range | Yes | Yes |
| 3 | `MARK` | JSON lifecycle/export mark | No | Yes |
| 4 | `SESSION_HEADER` | JSON session metadata | No | First record only |
| 5..255 | reserved | Rejected by this frame version | No | No |

`DATA` is the only record type that carries canonical session bytes. In the
log, a reader that understands the session kind can ignore everything else and
still recover the full byte stream. On the live path, a decoded RTT session
sends the derived `RTT_RECORDS` frame instead of the raw companion; the raw
bytes remain in the log.

`RTT_RECORDS` is a derived view. It never replaces the canonical `DATA` frame on
disk. Live clients receive the derived view when a decoder is active, because
that is what they render; the raw companion stays in the log for future
decoders and raw export.

`GAP` is a core record so every kind reports loss the same way. `MARK` covers
lifecycle for export and recovery; it is not broadcast live because lifecycle
is already a typed text control message.

### 6.5 Flags

| Bit | Name | Meaning |
| --- | --- | --- |
| 0 | `SUPERSEDED` | A canonical `DATA` frame whose `RTT_RECORDS` companion exists for the same `seq`. Display readers skip it; raw/export readers keep it. |
| 1..7 | reserved | Must be zero on write, ignored on read. |

A `DATA` frame without `SUPERSEDED` is the display frame for its sequence. A
`DATA` frame with `SUPERSEDED` is retained only for canonical bytes. This makes
the "prefer decoded" rule explicit and independent of record ordering.

### 6.6 RTT payloads

`RTT_RECORDS` payload is a JSON object:

```json
{
  "v": 1,
  "epoch": 2,
  "records": [
    {
      "timestamp": "12.345678",
      "level": "info",
      "module": "app",
      "file": "src/main.rs",
      "line": 42,
      "message": "sensor ready"
    }
  ],
  "raw_fallback": "aGVsbG8K",
  "error": null
}
```

- `epoch` is the session decode epoch at ingestion. It is part of the payload,
  not the frame header, because it is meaningful only for RTT. Raw `DATA`
  frames do not carry it; raw display does not need it, and `MARK(session.decoder)`
  records decoder changes for diagnostics.
- `records` may be empty. An empty record is still written when a chunk only
  contributed bytes to a partial defmt frame; it is the marker that says "these
  bytes were decoder input, not display output".
- `raw_fallback` is base64 for undecoded bytes emitted after a malformed frame.
  It is not the canonical chunk; the `DATA` frame is.
- `error` is a rate-limited decode error for this chunk, or null.
- `v` is the RTT payload version, independent of the frame version.

The CLI renders `records` first, then `raw_fallback`, then the error warning.
This preserves the current output order while removing the three separate text
messages that today share one sequence.

### 6.7 Gap payload

`GAP` payload is fixed-width, big-endian:

```text
from_seq:      u64
to_seq:        u64
dropped_bytes: u64
```

Range `[from_seq, to_seq]` is inclusive. The range covers sequence numbers, not
guaranteed display frames; hidden input or mark sequences may fall inside it if
they were never retained. `dropped_bytes` is best-effort and may be 0 when the
exact byte count is unavailable. The range is the authoritative information.

### 6.8 Mark payload

`MARK` payload is JSON:

```json
{
  "v": 1,
  "event": "session.active",
  "detail": {}
}
```

Events:

| Event | When | Export |
| --- | --- | --- |
| `session.active` | Worker reached active, or resumed | marker |
| `session.suspended` | Worker lost the resource | marker |
| `session.resumed` | Resume succeeded | marker |
| `session.failed` | Terminal failure | marker with error |
| `session.closed` | Graceful close | exit event |
| `session.decoder` | RTT decoder set, replaced, or cleared | marker; detail carries `decode` |

`detail` is an open JSON object. For `session.failed`, it carries
`{"error":"..."}`. For `session.decoder`, it carries the updated
`SessionDecode` object. Marks are not sent to live subscribers; `hello`,
`session.active`, `session.suspended`, `session.resumed`, `session.failed`,
`session.ended`, and `session.decoder` remain text control messages.

### 6.9 Session header payload

`SESSION_HEADER` is the first record in a log and is never sent on the wire
(`hello` carries the same information live):

```json
{
  "v": 1,
  "protocol": 1,
  "session_id": "ses_01J9Z6...",
  "kind": "rtt",
  "created_at_ns": 1764000000000000000
}
```

`v` is the session-header payload version. `protocol` is the boot protocol
version. The writer validates that an existing file's header matches the session
ID and kind before appending.

## 7. Wire format

### 7.1 Frame header

Every frame, on the wire and on disk, starts with this 36-byte header:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | magic `BFRM` (`0x42 0x46 0x52 0x4D`) |
| 4 | 1 | format version (`1`) |
| 5 | 1 | flags |
| 6 | 1 | record type |
| 7 | 1 | direction |
| 8 | 1 | stream |
| 9 | 3 | reserved (zero) |
| 12 | 8 | seq (`u64`, big-endian) |
| 20 | 8 | timestamp_ns (`u64`, big-endian) |
| 28 | 4 | payload length (`u32`, big-endian) |
| 32 | 4 | CRC32C |
| 36 | N | payload |

Rules:

- All integers are big-endian, matching the existing sequence byte order.
- `payload_len` is the payload length, not including the 36-byte header.
- `MAX_FRAME_PAYLOAD` is 64 MiB. The native DAP reader allows a 32 MiB body
  plus a 16 KiB header, so 64 MiB comfortably covers a maximum DAP frame. The
  server may enforce a lower limit for kinds that never need large frames, but
  the codec rejects anything above the hard limit.
- CRC32C (Castagnoli) covers the first 32 bytes of the header (magic through
  `payload_len`) followed by the payload. The CRC field itself is excluded.
- `reserved` bytes must be zero on write, and a reader must reject non-zero
  reserved bytes. This keeps the format forward-safe: a future version cannot
  silently reinterpret bits an old reader ignored.
- An unknown `record` value is a protocol error. Future frame versions add new
  core values; kind-specific payload structure lives inside `DATA` and is
  defined by the session kind (DAP already has its own `Content-Length`
  framing).

The header is deliberately fixed-size so a log reader can validate a record
without materializing it and a WebSocket reader can reject malformed input
before copying a large payload.

### 7.2 Why a header on WebSocket frames

The WebSocket message boundary already provides a length, so `payload_len` and
CRC are redundant on the wire. Keeping the same header is the point: one codec,
one test suite, one mental model, and a client can feed a received binary
message directly to the log parser. The overhead is 36 bytes per message. Serial
and RTT reads are capped at 1 KiB today; DAP messages are larger. The overhead
is negligible compared with the simplification.

### 7.3 Server to client

`GET /v1/sessions/{id}/io` keeps its current shape:

1. upgrade;
2. first server message is the text `hello`;
3. then binary `SessionFrame` messages for data, decoded views, and gaps;
4. lifecycle and errors remain text control messages;
5. for a closed/suspended session, replay follows `hello`, then
   `session.ended`, then a normal close.

The server sends at most one frame per producer chunk or input event. For RTT
with an active decoder it sends the `RTT_RECORDS` frame (the display frame); it
does not send the `SUPERSEDED` raw companion live. For serial and raw RTT it
sends `DATA` frames. For DAP it sends `DATA` frames with `direction = Output`.

A `RTT_RECORDS` frame is not sent if the session is in raw mode. A decode error
is carried inside the `RTT_RECORDS` payload, not as a separate message.

`hello` keeps its current fields (`data_seq`, `oldest_seq`, `latest_seq`). Its
semantics are clarified:

- `data_seq` is the cursor the server used for this connection, after any
  clamping in §9.4;
- `oldest_seq` is the first sequence still replayable;
- `latest_seq` is the highest allocated sequence, including input and marks.

### 7.4 Client to server input

Input uses the same frame with `direction = Input`:

- `record = DATA`;
- `stream` must be 0 for serial and DAP; RTT rejects input with a text `error`;
- `seq = 0` and `timestamp_ns = 0`; the server assigns both after validation;
- `flags = 0`;
- payload is the serial bytes or the complete DAP transport frame.

The server:

1. validates magic, version, CRC, length, record type, direction, and stream;
2. rejects input for kinds that do not support it (`rtt`) with a text
   `error` (`input_not_supported`) and stays attached;
3. tries to enqueue the payload on the session's bounded input queue;
4. on success, allocates `seq` and `timestamp_ns`, appends an input frame to the
   log, and updates `input_bytes` and `latest_seq`;
5. on queue overflow, sends a text `error` (`input_overflow` for serial,
   `dap_input_overflow` for DAP) and does not log the frame. DAP may close with
   `4408` as its current policy requires.

Input frames are never broadcast to live subscribers. `include_input=true`
applies to retained replay, not to new input while attached; DAP replay always
requests it because both sides of the conversation are needed for a useful
transcript.

A malformed input frame closes the socket with WebSocket protocol error `1002`.
A write from a `mode=read` subscriber still gets the existing
`write_forbidden` text error and remains attached.

### 7.5 WebSocket message limits

The existing 64 KiB per-message limit for serial/RTT is a server-side chunking
policy, not a frame rule. The frame codec allows up to 64 MiB so a maximum DAP
message can be sent as one buffer. The server continues to split serial/RTT
reads into small chunks and never batches them into a single large frame.

### 7.6 No binary frame before hello

A client binary frame before `hello` is a protocol error and closes with `1002`.
This matches the existing architecture rule and avoids ambiguous cursor state.

## 8. Saved log format

### 8.1 File layout

For now, one file per session:

```text
<session_dir>/<session_id>/session.log
```

Directory mode `0700`, file mode `0600`. The file is an append-only stream of
frames with no padding between records:

```text
SESSION_HEADER frame (seq 0)
DATA/RTT_RECORDS/GAP/MARK frames, in sequence order
```

Every record is a complete frame using the header in §7.1, including
`payload_len` and CRC32C. There is no separate per-record `BLOG` magic and no
separate file magic. The file is self-describing from the first frame.

Segment rotation is intentionally deferred. The format is already segment-ready:
rotation only needs to close a file at a frame boundary, start the next file
with a `SESSION_HEADER`, and read files in order. The existing but unused
`session_segments` table is removed in this pass; a future rotation design can
reintroduce a segment index with a concrete need and no change to frame bytes.

### 8.2 Write path

`SessionLogWriter::open(root, session_id, kind, created_at_ns)`:

1. creates the session directory if needed and applies modes;
2. opens `session.log` in append mode;
3. if the file is empty, writes the `SESSION_HEADER` frame. The runtime then
   writes a `MARK(session.decoder)` when an RTT decoder is already active, so a
   standalone log records the initial artifact and epoch;
4. if the file is non-empty, reads and validates the first frame:
   - magic/version must match;
   - record must be `SESSION_HEADER`;
   - `session_id` and `kind` must match the caller;
   - otherwise the log is unreadable and is handled by §8.6.

`append(header, payload)`:

1. encodes the complete frame into one buffer;
2. calls `write_all` once;
3. does not call `fsync` per frame;
4. returns an error to the caller on I/O failure.

On the first write failure the runtime marks `replayable = false`, closes the
writer, keeps the live session running, and sends `session.degraded` with reason
`persistence_failed`. It does not append after a failed write and does not
pretend the log is complete. Recovery truncates any partial tail before the
session can append again.

`close()` writes a `MARK(session.closed)` frame when possible and drops the file
handle. A crash can lose the last few bytes; recovery handles it.

### 8.3 Read path

`SessionLogReader` is a streaming iterator over frames:

- opens the file and validates the first frame as `SESSION_HEADER`;
- reads a 36-byte header, validates magic, version, reserved bytes, record type,
  direction, and `payload_len` (stream is validated by the session layer, which
  knows the kind);
- reads the payload;
- validates CRC32C;
- yields the frame;
- on EOF at a header boundary, ends normally;
- on a short header or payload, ends with `Truncated { offset }`;
- on a bad magic/version/CRC, ends with `Corrupt { offset }`.

`replay(after, include_input)` uses the reader and processes frames one at a
time instead of loading the raw file into a single buffer. It applies the
display rule in §8.5 and currently returns the selected events as a vector; a
future API can hand them to the caller one at a time. `scan` uses the same
reader to find the last valid frame and the last good offset. File I/O runs on a
blocking task when it is driven from an async handler, so a large replay does
not occupy an async worker.

The reader does not treat `SESSION_HEADER` as data. It validates it and skips
it. A file with a valid header but no data yields an empty replay.

### 8.4 Recovery

On startup, for each session record:

1. open the log and validate the header;
2. scan to the last valid frame;
3. use `max(log latest, database latest)` for `latest_seq`, preserving
   monotonicity, and recompute `input_bytes` by summing readable input `DATA`
   payloads (the database does not store it);
4. if the tail is truncated, truncate the file to the last good offset and
   continue appending from there;
5. if the log is corrupt before the tail, quarantine it by renaming it to
   `session.log.corrupt-<unix>` and start a new file with a `SESSION_HEADER`;
6. after quarantine, write a `GAP` frame covering the lost sequence range and
   continue after the database high-water mark;
7. if the log is missing but the database says data existed, keep the session
   metadata, mark `replayable = false`, and do not invent a sequence. For a
   resumable session, start a new log at `latest_seq + 1` and write a `GAP`
   frame for the missing range; for a terminal session, leave it unreadable.

A resumed serial/RTT session continues the same sequence. It never restarts at
zero. A DAP session is not resumable, but its log remains readable.

### 8.5 Display selection: raw vs decoded

For an RTT chunk with an active decoder, the log contains two frames with the
same `seq`:

1. `RTT_RECORDS` with `flags = 0`, written first;
2. `DATA` with `flags = SUPERSEDED`, written second.

For a chunk without a decoder, or a non-RTT kind, the log contains one `DATA`
frame with `flags = 0`.

Display selection:

- `RTT_RECORDS` is the display frame for its sequence.
- `DATA` with `SUPERSEDED` is skipped for display but retained for raw export
  and future decoding.
- `DATA` without `SUPERSEDED` is the display frame.
- A `DATA` frame with `SUPERSEDED` that is the only frame for its sequence (for
  example, a truncated log) is displayed as raw rather than silently dropped.

Because the decoded frame is written first, a sequential reader can emit it and
skip the following superseded raw frame without buffering. The `SUPERSEDED`
flag makes the rule explicit even if ordering changes later.

Raw export reads all `DATA` frames with `direction = Output`, including
superseded ones. Asciicast export uses display selection so defmt output is
text, not binary.

### 8.6 Unreadable logs

There is no legacy log format to migrate; the workspace is still green field.
A file that does not start with a valid `SESSION_HEADER` is corrupt or foreign,
not an older format:

- if the session is closed or failed, leave the file in place, set
  `replayable = false`, and log a warning;
- if the session may resume, rename the file to `session.log.corrupt-<unix>`,
  start a new log at `database latest_seq + 1`, write a `GAP` frame for the
  range that is not readable, and continue;
- `DELETE /v1/sessions/{id}/data` removes the log and any
  `session.log.corrupt-*` files in the session directory.

This preserves unreadable bytes for manual inspection without pretending they
are valid session data.

## 9. Runtime changes

### 9.1 Session event model

Replace `SessionEventData` with a frame-shaped event:

```rust
pub struct SessionEvent {
    pub seq: u64,
    pub timestamp_ns: u64,
    pub stream: u8,
    pub direction: FrameDirection,
    pub record: FrameRecord,
    pub flags: u8,
    pub payload: Vec<u8>,
}
```

`SessionHandle` keeps a ring of these. The ring holds display frames only:
`DATA` output without `SUPERSEDED`, `RTT_RECORDS`, `GAP`, and (if a future kind
needs it) kind-specific display records. Input frames and raw companions are
written to the log and are not put in the ring.

The runtime no longer matches on session kind to decide a frame shape. It
matches on kind only for:

- hardware/producer selection;
- RTT decode;
- DAP exclusivity and close-on-detach;
- ring size and overflow policy;
- input support.

### 9.2 Producer paths

All producers feed the same `push_frame` path:

```text
serial worker ─┐
rtt worker ────┼─► session task ─► allocate seq/time ─► log ─► ring ─► subscribers
dap worker ────┘
```

`push_output(kind, stream, data)`:

1. allocate `seq` from the session sequence and `timestamp_ns` once;
2. if the kind is RTT and a decoder is active:
   - build the `RTT_RECORDS` payload;
   - append `RTT_RECORDS` to the log with `flags = 0`;
   - append `DATA` to the log with `flags = SUPERSEDED`;
   - push `RTT_RECORDS` to the ring;
3. otherwise:
   - append `DATA` to the log with `flags = 0`;
   - push `DATA` to the ring;
4. update `latest_seq`, `bytes` (by the canonical output length, even when a
   decoded companion exists), and `buffered_bytes`;
5. notify subscribers.

`push_input(stream, data)`:

1. enqueue the payload on the bounded input queue;
2. allocate `seq` and `timestamp_ns`;
3. append a `DATA` frame with `direction = Input` and `flags = 0` to the log;
4. update `latest_seq` and `input_bytes`;
5. do not push to the ring and do not notify display subscribers.

DAP input uses the same `push_input` with `stream = 0`; serial input uses it
with `stream = 0`. RTT input is rejected before this point. The separate
`input_tx` and `dap_input_tx` fields become one bounded input queue with
per-kind capacity (serial 256 messages, DAP 64 messages); `try_send` failure is
reported to the client instead of silently dropping the frame.

DAP output uses `push_output` with `stream = 0` and `direction = Output`. The
dedicated `dap_output_tx`/`dap_output_rx` fields are removed. The worker still
uses a bounded channel to the session task; the session task pushes to the ring
and log. The DAP worker's channel and the ring are both bounded, so a slow
debugger cannot block hardware.

### 9.3 Sequence allocation and locking

The session keeps one `u64` sequence high-water mark. It is advanced under the
same lock that protects the ring/log append, or under a dedicated sequence
mutex. The invariant is: no two frames get the same sequence except the
intentional raw/decoded pair, and no frame is logged with a sequence lower than
a previously logged frame.

The raw/decoded pair is the only exception: it is constructed and appended as a
unit under the log lock, with one sequence and one timestamp. A reader therefore
never observes a decoded frame whose raw companion has a different sequence.

### 9.4 Ring, gaps, and overflow

`SessionRing` changes:

- stores `SessionEvent` with `stream` and `direction`;
- tracks `first_seq` and `bytes`;
- always retains at least the newest frame, even if that frame alone exceeds
  the byte cap;
- on eviction, tracks the last evicted display sequence. Gap
  `dropped_bytes` is best-effort and may be zero; the sequence range is
  authoritative, so no eviction-byte history is kept.

Ring caps:

| Kind | Cap |
| --- | --- |
| `serial` | 256 KiB |
| `rtt` | 1 MiB |
| `dap` | 4 MiB |

When a subscriber's cursor is older than `ring.first_seq`:

- for serial and RTT, send a `GAP` frame for `[cursor + 1, ring.first_seq - 1]`
  and continue from `ring.first_seq`;
- for DAP, fail the session with `last_error = "dap_output_overflow"` and close
  the subscriber with `4408`. DAP cannot resynchronize after a gap, so a gap
  frame would be misleading. The debugger sees a terminated adapter.

On attach, the server first streams the log for the requested cursor, then
switches to the ring. If the cursor is older than the log's first sequence, send
one `GAP` for the missing prefix, then continue. If the cursor is newer than
`latest_seq`, clamp it to `latest_seq`; the subscriber starts at the live tail
and receives no replay. Clamping prevents a bad client cursor from silently
skipping future frames whose sequence is still below the requested value.

The server never infers a gap from a ring read that simply skipped hidden input
or mark sequences. Input and marks are not in the ring, so any sequence jump
they cause is expected.

### 9.5 WebSocket handlers

`run.rs::session_io_ws` becomes the only data-plane loop:

- attach and subscriber limits are unchanged;
- DAP policy stays: one live subscriber, write mode required while live,
  read mode only for closed/failed replay, close the session on disconnect;
- `hello` is sent as text;
- replay uses the unified log reader and sends frames;
- live frames come from the ring;
- client binary frames are decoded once; input frames go to `push_input` and
  other directions/types are rejected;
- `session.active`, `session.suspended`, `session.resumed`, `session.failed`,
  `session.ended`, `session.decoder`, `error`, `ping`, and `pong` remain text.

`dap_session_io_ws` and `dap_replay_ws` are removed. The DAP-specific behavior
is a policy branch inside the generic handler, not a second framing path. The
generic closed-session replay path already sends `session.ended`; DAP uses the
same path with `include_input = true`.

`send_session_event` is replaced by `send_frame`, which encodes a
`SessionFrame` and sends one binary message. It does not branch on kind.

### 9.6 Session counters

`SessionData` becomes:

```rust
pub struct SessionData {
    pub oldest_seq: u64,
    pub latest_seq: u64,
    pub buffered_bytes: u64,
    pub bytes: u64,        // canonical output DATA bytes
    pub input_bytes: u64,  // canonical input DATA bytes
}
```

- `bytes` counts only `DATA` output payloads. It does not count decoded,
  fallback, input, marks, or framing overhead.
- `input_bytes` counts only `DATA` input payloads.
- `buffered_bytes` is the ring's encoded size or payload size; it is a live
  buffer metric, not durable storage.
- `oldest_seq` is the first replayable sequence (log first, or ring first when
  the log is unavailable).
- `latest_seq` is the highest allocated frame sequence.

`ServerStatus.storage_bytes` sums `bytes + input_bytes` for sessions. This is
still an accounting estimate, not a filesystem measurement.

### 9.7 Export

`export_session` reads the unified log:

- `DATA` output without `SUPERSEDED` → `o` event (UTF-8 checked);
- `RTT_RECORDS` → rendered record text, plus `raw_fallback` as `o`;
- `DATA` input → `i` event only when `include_input=true`;
- `GAP` → `m` marker with the range;
- `MARK(session.suspended/resumed/failed/decoder)` → `m` marker;
- `MARK(session.closed)` → `x` event;
- marks and gaps that do not map to asciicast are skipped.

The existing 409 behavior for non-UTF-8 output is unchanged. Defmt sessions
export as text because `RTT_RECORDS` is the display frame. DAP remains
non-exportable as asciicast and still returns 409, because a DAP transcript is
not a terminal stream.

## 10. CLI changes

### 10.1 Shared session follow

`boot-cli/src/follow/mod.rs`:

- import `decode_frame` and `SessionFrame`;
- for a binary message, decode once and dispatch on `record`:
  - `DATA` with `direction = Output` → `SessionEvent::Raw { stream, data }`;
  - `RTT_RECORDS` → parse the JSON and emit `SessionEvent::RttRecords` plus
    raw fallback and decode error;
  - `GAP` → `SessionEvent::Gap`;
  - `MARK` → ignore in human replay except `session.decoder`, which updates the
    decoder label; export and recovery consume all marks;
  - `DATA` with `direction = Input` → `SessionEvent::Input` when replaying with
    `include_input`;
- remove the `kind` string passed to the decoder;
- encode stdin chunks as input frames before sending;
- keep reconnect, cursor, and lifecycle handling unchanged.

### 10.2 DAP replay and bridge

`boot-cli/src/follow/dap.rs` (replay rendering):

- decode `SessionFrame` instead of the kind-specific frame;
- feed both directions to the DAP message parser so requests and responses
  interleave correctly;
- render both directions, matching today's transcript;
- handle `GAP` as a status warning (a DAP transcript with a gap is incomplete).

`boot-cli/src/dap_bridge.rs`:

- when reading from the debugger, wrap each complete DAP transport frame in a
  `DATA` input frame;
- when reading from the server, decode the frame, ignore non-output directions
  for the debugger, and write only the `DATA` payload;
- on `GAP`, report a status warning and continue if the session is still live;
  DAP live sessions fail rather than gap, so this is a replay-only case.

The DAP `Content-Length` parser and `FrameReader` stay; they now operate on the
frame payload, not on the WebSocket message.

### 10.3 UI sink

`SessionSink` changes from `raw(channel, data)` to `raw(stream, data)` (the
parameter name is corrected) and keeps `rtt_records`, `input`, and `gap`. The
sink still receives `SessionKind` for display policy (RTT raw output uses
`from_utf8_lossy`, serial writes raw bytes), but it no longer uses the kind to
parse frames. `DATA` is opaque; frame dispatch already happened in
`follow_session`.

`SessionKind` is still used for status labels, RTT rendering, and DAP
exclusivity, not for framing.

## 11. Protocol and API changes

### 11.1 `boot-protocol`

Add a `session_frame` module (or a section in `lib.rs`):

```rust
pub const SESSION_FRAME_VERSION: u8 = 1;
pub const SESSION_FRAME_MAX_PAYLOAD: usize = 64 * 1024 * 1024;

pub enum FrameDirection { Output = 0, Input = 1 }
pub enum FrameRecord {
    Data = 0,
    RttRecords = 1,
    Gap = 2,
    Mark = 3,
    SessionHeader = 4,
}

pub const FRAME_FLAG_SUPERSEDED: u8 = 0x01;

pub struct SessionFrameHeader { /* fields from §6.1 */ }

pub fn encode_frame(
    header: &SessionFrameHeader,
    payload: &[u8],
) -> Result<Vec<u8>, FrameError>;
pub fn decode_frame(bytes: &[u8]) -> Result<SessionFrame<'_>, FrameError>;
```

`FrameError` must distinguish:

- `TooShort`;
- `BadMagic`;
- `UnsupportedVersion`;
- `ReservedBytesSet`;
- `UnknownRecord`;
- `InvalidDirection`;
- `PayloadTooLarge`;
- `LengthMismatch`;
- `BadCrc`.

Stream validity is session-kind policy, not frame validity: the codec accepts
any `u8`, and the session layer rejects a stream the kind does not have (for
example, nonzero for serial or DAP).

Remove:

- `SessionDataFrame`;
- `SessionFrameError` (replaced by `FrameError`);
- `encode_session_frame`;
- `decode_session_frame`;
- the data variants of `SessionWsMessage`: `Gap`, `Input`, `RttRecords`,
  `RttRaw`, `RttDecodeError`.

Keep:

- `RttRecord` and `display_line`;
- lifecycle, error, ping, pong, and hello variants;
- `PROTOCOL_VERSION = 1`, unchanged. The workspace is green field, so there is
  no version bump for a breaking wire change.

`SessionData` gains `input_bytes` with serde default 0 for tolerant decoding of
the generated client. Since there are no deployments, a default is for schema
ergonomics, not compatibility.

### 11.2 API surface

No route changes. The data plane changes shape, not endpoints:

- `GET /v1/sessions/{id}/io` now carries frames;
- `GET /v1/sessions/{id}/export` reads the same log and preserves its query
  parameters;
- `GET /v1/sessions/{id}` and `GET /v1/sessions` include `input_bytes`;
- `GET /v1/server` keeps protocol version 1 and sums both byte counters.

`boot-api-client` is regenerated for the `SessionData` field. WebSocket types
come from `boot-protocol` and are updated with it.

## 12. Compatibility and rollout

- `PROTOCOL_VERSION` stays at 1. `SessionWsMessage::Hello.v` and
  `Hello.protocol` stay at 1. The workspace is still green field, so the
  breaking wire change does not need a version bump.
- There is no dual framing and no compatibility reader. Server and CLI are
  updated together, and there is no legacy log format to migrate.
- There are no deployed servers; edit the initial migration in place instead of
  adding a migration chain. The `sessions` table is unchanged except for
  whatever the runtime needs for counters (none if `input_bytes` is derived
  from the log at startup).
- Remove the unused `session_segments` table and `SegmentRecord` store methods
  from the initial migration and store layer in this pass. Rotation would
  reintroduce a segment index with a concrete design.
- Remove the unused `session_marks` table. It is redundant with `MARK` records
  in the log; keeping both would create two mark stores. This is a cleanup, not
  a new migration chain.
- The DAP design document's 9-byte frame and "DAP does not use SessionRing"
  statements are superseded. DAP keeps its exclusive, non-resumable,
  close-on-detach policy and its fail-on-overflow behavior.
- The RTT design document's text data messages are superseded. The decode
  epoch, artifact selection, and `RECORD_DECODED` intent move into the
  `RTT_RECORDS` payload.
- Asciicast remains an export format, not storage.

## 13. Alternatives considered

### 13.1 Keep kind-specific framing

Simplest and already implemented. Rejected because the current split causes
three codecs, silent RTT input, ambiguous channel/direction, and a separate log
merge rule. Every new session kind would repeat the problem.

### 13.2 Use JSON text envelopes with base64 payloads

Uniform and easy to inspect. Rejected because base64 adds 33% overhead, DAP
latency suffers, and binary payloads become opaque strings. The data plane is
byte-oriented; a binary header is the right shape.

### 13.3 Use CBOR or protobuf for the envelope

Extensible and compact. Rejected because it adds a dependency and a schema
toolchain for a 36-byte header. A fixed header is simpler to audit and test.

### 13.4 Keep RTT as text messages and wrap serial/DAP only

Less churn for the CLI. Rejected because RTT is a session type too; the
standardization goal is explicitly all session types. Keeping RTT text would
preserve the need for kind-aware client loops and a second replay path.

### 13.5 Store only decoded RTT text

Smaller logs and trivial replay. Rejected because it loses canonical bytes,
prevents future decoders, and makes a bad decode permanent. The `SUPERSEDED`
flag keeps both without ambiguity.

### 13.6 Use a sidecar decoded record keyed by sequence

This is today's `RECORD_DECODED`. Rejected because it requires a whole-file map,
depends on an implicit merge rule, and has no ordering guarantee. A `RTT_RECORDS`
frame with an explicit `SUPERSEDED` raw companion is self-describing and
streamable.

### 13.7 Separate log per direction or per stream

Makes each file homogeneous but complicates ordering, cursor replay, and gap
reporting. Rejected; one sequence space is simpler and preserves interleaving.

### 13.8 Make DAP use a dedicated bypass path

Keeps DAP's current direct channel and avoids ring policy. Rejected because it
preserves a second framing path. The frame codec is shared; DAP differs only in
its ring cap and fail-on-overflow policy.

### 13.9 Use asciicast as the log

Already rejected in api-architecture-design.md §14.11: it loses sequence
numbers, gaps, channels, binary payloads, and DAP framing.

## 14. Testing

No test requires real hardware.

### 14.1 Frame codec

- Round-trip every record type, direction, stream, and flag combination with
  representative payload lengths: empty, small, a DAP-sized message, and an
  over-limit payload that must be rejected.
- Assert the exact 36-byte header layout and big-endian integers.
- Reject wrong magic, version, reserved bits, unknown record, invalid direction,
  oversized payload, length mismatch, and bad CRC.
- Decode a frame with a payload that contains the magic bytes; length and CRC
  prevent false synchronization.
- Verify CRC covers header fields and payload.

### 14.2 Log

- Write a header and frames; read them back in order.
- Reject a file with no header or a mismatched session ID/kind.
- Truncate a partial tail and continue appending.
- Quarantine a corrupt mid-file and start a new log with a `GAP`.
- Continue sequence after restart; never reuse a sequence.
- Verify timestamps are nanosecond resolution and non-decreasing.
- Verify display selection: `RTT_RECORDS` first, `SUPERSEDED` raw skipped,
  raw-only frames displayed, orphaned superseded raw displayed.
- Verify `latest_seq` and `oldest_seq` after restart and purge.
- Verify an unreadable header is quarantined, a `GAP` is written, and
  `replayable=false` is set when appropriate.
- Verify permissions 0700/0600.

### 14.3 Runtime and data plane

- Serial output → one `DATA` output frame; ring and log match.
- Serial input → one `DATA` input frame; not in the ring; `input_bytes` grows;
  replay with `include_input` shows it.
- RTT raw → `DATA` output.
- RTT decoded → `RTT_RECORDS` display frame plus `SUPERSEDED` `DATA`; replay
  prefers the decoded frame; raw export sees the canonical bytes.
- RTT partial frame → empty `RTT_RECORDS` marker; no raw display duplication.
- RTT decode error → one `RTT_RECORDS` payload with `error`; CLI shows one
  warning.
- DAP output and input → `DATA` frames with the correct direction; replay
  interleaves them.
- DAP live overflow → session fails, no gap is sent, subscriber closes 4408.
- Serial/RTT ring eviction → one `GAP` with the correct range; no inferred gap
  from a hidden input sequence.
- Input queue overflow → `input_overflow` error, frame not logged.
- Malformed client frame → close 1002.
- Read-only input → `write_forbidden` and stay attached.

### 14.4 HTTP and CLI

- `/io` sends `hello` then binary frames for a closed serial, RTT, and DAP
  session.
- Closed DAP replay uses the generic handler and includes input.
- `export-session` renders RTT decoded text, serial text, input events, gaps,
  and close markers.
- `follow-session` renders serial, RTT raw, RTT decoded, and gaps.
- `replay-session --from` works across the unified frames.
- `start-dap` bridge strips output frames and wraps input frames; the debugger
  sees an unchanged DAP byte stream.
- `attach-serial --write` sends framed stdin input.
- `list-sessions` and `server` report `bytes` and `input_bytes` without
  counting decoded or framing overhead.

### 14.5 Manual smoke

1. Open a serial session, type input, close, and replay with and without
   `--include-input`.
2. Open an RTT session with a defmt artifact, confirm live records, stop the
   server, restart, resume, and replay without the artifact.
3. Start a DAP bridge with a real debugger, set a breakpoint, disconnect, and
   replay the transcript.
4. Fill a slow serial subscriber past the ring cap and confirm it receives one
   gap and continues.
5. Kill the server mid-write and confirm recovery truncates only the partial
   tail and keeps the sequence monotonic.

## 15. Implementation plan

1. **Protocol frame module.** Add `SessionFrameHeader`, `FrameRecord`,
   `FrameDirection`, `encode_frame`, `decode_frame`, `FrameError`, and the CRC
   dependency in `boot-protocol`. Keep `PROTOCOL_VERSION = 1`. Remove the old
   framing functions and data `SessionWsMessage` variants.
2. **Log writer/reader.** Replace `session_log.rs` with the frame stream,
   session-header frame, CRC, streaming reader, and truncation/corruption
   recovery. Update `latest_seq`, `restore_sessions`, `close_session(purge)`,
   and `SessionLogWriter` open/append.
3. **Runtime event model.** Replace `SessionEventData` with the frame-shaped
   event. Implement one `push_output`/`push_input`, the raw/decoded companion
   pair, sequence/timestamp allocation, counters, ring changes, gap emission,
   and DAP fail-on-overflow.
4. **API/WebSocket.** Collapse `dap_session_io_ws`/`dap_replay_ws` into the
   generic `session_io_ws`. Send and receive frames. Update `export_session`.
   Update `SessionData` and server status.
5. **CLI.** Update `follow/mod.rs`, `follow/dap.rs`, `dap_bridge.rs`,
   `execute.rs`, and `ui/mod.rs` to the frame API. Encode stdin input frames.
6. **Cleanup.** Remove the old `RECORD_*` constants, the unused
   `session_marks` table, and the unused `session_segments` table plus
   `SegmentRecord` store methods. Rotation keeps its own future design.
7. **Docs.** Update api-architecture-design.md §10.4, §11.2, §14.4; remove or
   redirect the RTT text-message section; update the DAP data-path sections.
   Keep this document as the canonical framing reference.
8. **Verification.** `cargo fmt --check`, `cargo test --workspace`,
   `cargo clippy --workspace --all-targets`, and `cargo deny check licenses` for
   the CRC dependency.

## 16. Resolved decisions

| # | Question | Decision |
| --- | --- | --- |
| 1 | Live RTT raw companion | Display only. Live subscribers receive `RTT_RECORDS`; the raw `DATA` companion stays in the log for raw export and future decoding. |
| 2 | Log segmentation | One `session.log` for now. Rotation is deferred; the frame format is segment-ready. |
| 3 | Clock resolution | Nanoseconds. `timestamp_ns` is `u64`, valid until 2554 and aligned with `SystemTime`. |
| 4 | `input_bytes` | Add it to `SessionData`. `bytes` remains canonical output only; `input_bytes` covers canonical input. |
| 5 | Marks in replay | Human replay ignores marks except `session.decoder`, which updates the decoder label. Export and recovery use all marks. |
| 6 | Legacy logs | None. The workspace is green field; there is no legacy log format, no v1 reader, and no migration path. Unreadable files are corrupt/foreign and follow §8.6. |
| 7 | DAP ring | Use the shared ring with a 4 MiB cap and fail-on-eviction. The dedicated DAP output channel is removed. |
| 8 | Gap byte count | `dropped_bytes` is best-effort. The sequence range is authoritative; do not keep an eviction history just for exact counts. |

## 17. Decision log

| Date | Decision |
| --- | --- |
| 2026-11-25 | Proposal created: one versioned binary frame for serial, RTT, and DAP; input is a direction; RTT decoded output is a frame record type; the log is a stream of the same frames. |
| 2026-11-25 | One sequence covers all persisted frames; hidden input and marks may make the visible sequence non-contiguous; only `GAP` means loss. |
| 2026-11-25 | Canonical raw bytes are always logged; RTT decoded output is a `RTT_RECORDS` companion and the raw frame is marked `SUPERSEDED`. |
| 2026-11-25 | The saved log starts with a `SESSION_HEADER` frame and uses the same 36-byte record header with CRC32C. |
| 2026-11-25 | The wire and log formats change in place; no compatibility shim. |
| 2026-11-25 | DAP keeps exclusive, non-resumable, close-on-detach policy and fails on overflow instead of emitting a gap. |
| 2026-11-25 | The generic WebSocket loop handles all kinds; DAP-specific framing handlers are removed. |
| 2026-11-26 | `PROTOCOL_VERSION` stays at 1. The workspace is green field, so the breaking wire change does not need a version bump. |
| 2026-11-26 | Live RTT subscribers receive the display frame only; the raw companion is log/export-only. |
| 2026-11-26 | One `session.log` for now; segment rotation is deferred. |
| 2026-11-26 | Timestamps are Unix nanoseconds. |
| 2026-11-26 | Add `input_bytes` to `SessionData`. |
| 2026-11-26 | Human replay ignores marks except `session.decoder`; export and recovery use all marks. |
| 2026-11-26 | No legacy log handling or migration; unreadable logs are corrupt/foreign. |
| 2026-11-26 | DAP live output uses the shared ring and fails on eviction; the dedicated output channel is removed. |
| 2026-11-26 | Gap `dropped_bytes` is best-effort; the sequence range is authoritative. |
| 2026-11-26 | Implemented: frame codec, CRC-checked frame log, unified runtime event path, generic `/io` handler, input frames, gap handling, DAP shared ring, CLI frame decoding, export, and removal of the unused segment/mark tables. |
