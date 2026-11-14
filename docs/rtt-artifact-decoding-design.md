# RTT Session Artifact and Server-Side Decoding — Design

Status: Implemented  
Date: 2026-11-14  
Scope: `boot-api`, `boot-protocol`, `boot-api-client`, `boot-cli`, documentation  
Related: [api-architecture-design.md](api-architecture-design.md),
[cli-command-structure.md](cli-command-structure.md)

## 1. Summary

An RTT session should optionally own a firmware artifact. The artifact is the
ELF that the target is running. The server uses it for two jobs that are
currently done by the CLI:

1. find the `_SEGGER_RTT` control block address;
2. decode defmt frames, including format strings and source locations.

The artifact is recorded on the session, not passed to every attach. A client
that attaches to an existing RTT session therefore gets decoded output without
having a matching ELF on its machine. A client that has a new build can upload
an artifact and make it the session's decoder, without reimplementing decoding
client-side.

The server also records the **last successfully flashed artifact** for each
stable probe. New RTT sessions inherit it unless the request supplies a
different artifact or explicitly opts out. This is what makes
`run-firmware --no-attach` followed later by `attach-probe` work with no
`--elf`, and it gives every client a server-owned default decoder for the
firmware that is actually running.

The server remains the single owner of hardware and the single producer of
session data. Artifacts no longer have a TTL and are never removed by a
background reaper. They live until a client explicitly deletes them. The server
tracks which sessions, probe last-flash records, and in-flight flash changes
reference an artifact so deletion can reject an artifact that is still in use.
Raw RTT bytes stay the canonical session log; decoded output is stored alongside
them so replay and export do not depend on the artifact still being present.

This intentionally supersedes the statement in
[api-architecture-design.md §10.4](api-architecture-design.md#104-fanout-ring-and-framing)
that the server never decodes defmt. The rest of the runtime model is unchanged.

## 2. Current behavior

### 2.1 Attach flow

`boot attach-probe <server>/<probe> --elf <file>` currently:

1. reads the ELF on the client;
2. parses the defmt table and source locations locally;
3. extracts `_SEGGER_RTT` with `object`;
4. creates an RTT session with `options.rtt_addr`;
5. attaches to `/v1/sessions/{id}/io`;
6. strips the 9-byte RTT frame header and feeds payload bytes to a local
   `defmt_decoder::StreamDecoder`;
7. formats each decoded frame for the terminal.

The server only sees a hexadecimal `rtt_addr`. It stores raw RTT bytes, sends
raw bytes to subscribers, and has no knowledge of defmt.

`boot run-firmware` already uploads the ELF as an artifact for flashing, but it
does not associate that artifact with the RTT session it creates afterward, and
the server does not remember it as the probe's firmware. Artifacts also have a
one-hour TTL and a background cleanup pass today, which adds lifetime rules that
are unrelated to whether the artifact is still needed.

### 2.2 Relevant code

| Area | Current location |
| --- | --- |
| Client ELF parsing and defmt decoding | `boot-cli/src/probe.rs` (`attach_probe`, `extract_rtt_address`) |
| RTT hardware attach | `boot-api/src/runtime/session.rs` (`rtt_thread`) |
| Session data framing | `boot-api/src/run.rs` (`send_binary`) |
| Raw session log | `boot-api/src/runtime/session_log.rs` |
| Artifact store | `boot-api/src/runtime/artifact.rs` |
| Wire types | `boot-protocol/src/lib.rs` |

### 2.3 Problems

| # | Problem | Consequence |
| --- | --- | --- |
| P1 | Every attaching client needs the exact matching ELF. | A second engineer or a CI machine cannot follow the same RTT stream without finding the build. |
| P2 | Decoding is client-specific. | Two clients can show different names, levels, or locations for the same bytes. |
| P3 | The server already has the flashed artifact but forgets it after the change. | The client uploads and parses the same ELF again after `run-firmware`, and `--no-attach` gives later attaches no way to know the firmware. |
| P4 | RTT address lookup happens on the client. | The server only receives an opaque hex string and cannot validate or re-derive it. |
| P5 | Raw bytes are the only persisted form. | Replay and asciicast export cannot render defmt data without a client-side ELF. |
| P6 | The CLI carries `defmt-decoder`, `defmt-parser`, and `object`. | Every client must stay compatible with the firmware's defmt version and the decoder library. |
| P7 | A single client decoder is fed every RTT channel. | Interleaved channels can produce malformed frames or misleading output. |
| P8 | Artifact TTL and cleanup add a second lifetime independent of sessions and probe records. | A session or probe default can outlive the artifact that describes it, and the reaper must know every owner to avoid deleting an in-use file. |
| P9 | Artifact deletion has no in-use check. | Deleting an artifact can break a running decoder or a probe's inherited firmware. |

## 3. Goals and non-goals

### 3.1 Goals

- Record an optional firmware artifact on an RTT session.
- Record the last successfully flashed artifact on each stable probe and inherit
  it for new RTT sessions.
- Resolve the RTT control block address from the selected artifact at session
  creation.
- Decode defmt on the server and send structured, ready-to-render records.
- Let a client upload a new artifact and switch the session decoder while it is
  running.
- Make replay and export use the same decoded representation, without a local
  ELF.
- Keep raw RTT bytes as the canonical durable data.
- Remove artifact TTLs and background cleanup; artifacts live until explicitly
  deleted.
- Track artifact references so an in-use artifact cannot be deleted.
- Remove client-side defmt and ELF parsing from the attach path.
- Keep the change testable without hardware.

### 3.2 Non-goals

- Changing serial or DAP data framing.
- Decoding arbitrary binary RTT protocols or defining a general log format.
- Re-decoding already captured output when the artifact changes.
- Reattaching the live RTT producer when the artifact changes.
- Automatically re-flashing or rolling back a probe when its last artifact is
  missing or unusable.
- Automatic artifact expiry, TTL refresh, or background garbage collection.
  Storage is managed by explicit deletion (and an optional upload quota, if
  configured).
- Adding per-artifact ownership or multi-tenant ACLs. `control` remains the
  trusted hardware scope.
- Implementing DAP.
- Removing `probe-rs`'s fallback RTT scan for sessions with no artifact.
- Adding a global event stream or changing lease/session ownership semantics.

## 4. Decisions at a glance

| # | Decision | Rationale |
| --- | --- | --- |
| D1 | An RTT session may reference one artifact for address lookup and decoding. | Gives every subscriber a shared, server-owned interpretation. |
| D2 | The artifact is a top-level probe-session field, not part of `SessionOptions`. | Options describe immutable hardware setup; the decoder may change. |
| D3 | Artifact replacement uses dedicated `PUT`/`DELETE /v1/sessions/{id}/artifact`. | Clear set/clear semantics without an `Option<Option<T>>` patch. |
| D4 | RTT address precedence is explicit `rtt_addr`, then artifact `_SEGGER_RTT`, then `probe-rs` scan. | Preserves an escape hatch and existing behavior. |
| D5 | Decode on the server, once per producer chunk, with one decoder per RTT channel. | Avoids per-subscriber duplicate work and fixes channel interleaving. |
| D6 | Send structured text frames for RTT data; keep serial binary frames. | Clients no longer decode defmt; text is easier to evolve than a binary envelope. |
| D7 | Keep raw bytes as the canonical log and persist decoded companion records. | Replay/export stay stable even if the artifact is deleted or replaced. |
| D8 | Track references from non-terminal sessions, probe last-flash records, and in-flight flash changes so in-use artifacts cannot be deleted. | Protects active decoders and probe defaults without imposing a second lifetime. |
| D9 | Track a monotonic `decode_epoch` and never re-decode past output. | Artifact changes are visible and historical output remains truthful. |
| D10 | Artifact replacement changes decoding only, not the attached RTT control block. | Reattaching hardware is a separate, disruptive operation. |
| D11 | Replay and export prefer decoded records, with raw fallback. | One representation for live and retained output. |
| D12 | Remove defmt/ELF parsing from `boot-cli`; add it to `boot-api`. | Moves the cost and compatibility surface to the server. |
| D13 | Change `/v1` in place and keep `PROTOCOL_VERSION = 1`. | There are no deployments; the repo forbids compatibility shims. |
| D14 | Do not add artifact ownership in this pass. | `control` is already the hardware-trust scope; revisit for multi-tenant use. |
| D15 | Record the last successful flash artifact per stable probe. | Lets later attaches use the firmware that is actually running without re-uploading an ELF. |
| D16 | New RTT sessions inherit the probe artifact by default; explicit request artifacts and `inherit_artifact=false` override it. | Makes the common case flag-free while preserving an explicit escape hatch. |
| D17 | A failed or cancelled flash does not change the probe's last artifact; a successful non-ELF or RTT-less flash clears it. | The record must describe the current firmware, not a partially written or unusable image. |
| D18 | Remove artifact TTLs, `expires_at`, and the background cleanup pass. Artifacts live until explicitly deleted. | Removes a second, unrelated lifetime model; reference checks already protect in-use files. |

## 5. Model

### 5.1 Session artifact

An RTT session has an optional **decode state**:

```text
SessionDecode
  mode:            "defmt" | "raw"
  epoch:           u64
  artifact:        SessionArtifact | null
  rtt_addr:        string | null     # address actually used by the producer
  rtt_addr_source: "explicit" | "artifact" | "scan"
```

The artifact summary is:

```text
SessionArtifact
  id:          art_...
  size:        bytes
  sha256:      hex
  created_at:  RFC 3339
  defmt:       bool
  rtt_addr:    string | null        # _SEGGER_RTT found in this artifact
```

`mode` is `defmt` when an artifact with a defmt table is active. It is `raw`
when there is no artifact, the artifact has no defmt section, or decoding is
unavailable. `rtt_addr` is the address the producer attached to; it is stored
separately from `artifact.rtt_addr` because an artifact replacement does not
move the live control block.

### 5.2 Probe last flashed artifact

Each stable probe resource may record one **last flashed artifact**:

```text
ProbeArtifact
  id:          art_...
  sha256:      hex
  size:        bytes
  flashed_at:  RFC 3339
  change:      chg_... | null      # the flash change that produced it
  created_by:  principal id | null
  defmt:       bool
  rtt_addr:    string | null       # _SEGGER_RTT found in this artifact
```

Rules:

1. The record is updated when a `flash` change reaches status `done`. It is not
   updated at flash creation, because the write may fail.
2. A failed, cancelled, or interrupted flash leaves the previous record
   unchanged.
3. A successful flash whose artifact is not a readable ELF, or is an ELF with
   neither `_SEGGER_RTT` nor defmt data, clears the record. The old ELF no
   longer describes the running firmware, and an artifact with no usable RTT
   metadata would only hold storage for no benefit.
4. The record is keyed by the stable resource ID, so it survives server restarts
   and probe re-enumeration. It is not tied to the current USB generation.
5. The probe record keeps its artifact referenced. There is no TTL: the artifact
   lives until the record is replaced or cleared and the artifact is explicitly
   deleted.
6. `GET /v1/probes/{id}` and `GET /v1/probes` expose `last_artifact` as
   metadata. A probe can exist without one.
7. A flash change keeps a reference to its artifact while it is in flight, so
   the artifact cannot be deleted between request acceptance and the probe
   record taking over the reference.

The record describes what was last written through `boot`, not an absolute
claim about the target. If firmware was flashed by another tool, reset to a
bootloader, or replaced out of band, the record can be stale. The probe artifact
is a convenience and a default, not a hardware observation. A client can always
override it with an explicit artifact, and a new session can opt out.

### 5.3 Artifact references and deletion

Artifacts are immutable uploads identified by ID and SHA-256. They do not
expire. The only way to remove an artifact is `DELETE /v1/artifacts/{id}`.
Because a running session or a probe's remembered firmware may still need one,
the server tracks references and rejects deletion while any owner is using the
artifact.

Rules:

1. An artifact is referenced while it is used by:
   - a non-terminal session;
   - a probe's last-flash record;
   - an in-flight flash change.
2. There is no `expires_at`, TTL refresh, or background artifact reaper.
3. `DELETE /v1/artifacts/{id}` returns `409 artifact_in_use` while any owner
   references it.
4. A session reference is released when the session reaches `closed` or
   `failed`, or when the session is purged.
5. A `suspended` session keeps its reference because it may be resumed.
6. A probe reference is released when a newer successful flash replaces the
   record, a non-ELF or RTT-less flash clears it, or the record is explicitly
   cleared.
7. A flash-change reference is released when the change reaches a terminal
   state. On success, the probe record takes over the reference before the
   change reference is released.
8. Multiple owners may reference the same artifact. The artifact remains
   deletable only after the last owner releases it.
9. If an artifact is missing at recovery time, a session still resumes using the
   persisted address and falls back to raw output for new data. A probe record
   with a missing file is ignored for inheritance until it is cleared or
   replaced. Both cases record a warning rather than failing hardware work.

The recommended implementation is a small `artifact_refs` table keyed by
`(artifact_id, owner_kind, owner_id)` for sessions and flash changes, plus the
`probe_artifacts` record as the probe reference. Delete checks look at both.
`Artifact.in_use` is derived from those references. The current artifact is also
stored on the session for snapshot rendering.

### 5.4 RTT address resolution

At session creation the server selects the decoder artifact first:

1. An explicit `artifact` on the request.
2. Otherwise, the probe's last flashed artifact when
   `inherit_artifact` is true (the default).
3. Otherwise, no artifact.

It then resolves the control block address in this order:

1. `options.rtt_addr`, if the client supplied one.
2. `_SEGGER_RTT` from the selected artifact.
3. No address; the RTT worker uses `probe_rs::rtt::Rtt::attach` to scan.

The resolved address and its source are stored on the session. On recovery the
worker uses the stored address, so a resumed session does not depend on the
artifact still being readable. The probe artifact also provides the default
RTT address after a later `attach-probe` with no flags.

An artifact replacement also extracts `_SEGGER_RTT` for display and mismatch
detection, but it does **not** move the live producer or change the stored
producer address. If the new artifact's address differs from the address the
producer attached to, the server logs a warning (`rtt_addr_mismatch`) and still
applies the new decode table. Changing the hardware address requires creating a
new session, not swapping the decoder.

### 5.5 Decode state and epochs

Each artifact association, replacement, or clear increments `decode_epoch`,
starting at 1 for the first artifact. Epoch 0 means "no artifact / raw".

The epoch is:

- part of the session snapshot;
- included in every decoded or raw RTT data message;
- included in decoded log records;
- sent to live subscribers when the decoder changes.

Historical output is never re-decoded. Scrollback stays as it was captured.
A new artifact affects only output produced after the epoch bump. When a
decoder is replaced or cleared while it holds partial-frame bytes, those bytes
are flushed as `rtt.raw` before the reset so no producer input is lost.

### 5.6 Server-side decoding

The server builds a **decode profile** from an artifact:

```text
DecodeProfile
  artifact_id
  sha256
  rtt_addr: Option<u64>
  defmt_table: Option<defmt_decoder::Table>
  locations: Option<defmt_decoder::Locations>
```

Profile parsing:

- `object::File::parse` locates `_SEGGER_RTT`.
- `defmt_decoder::Table::parse` returns `None` when the ELF has no `.defmt`
  section. That is not an error; the artifact may still supply an RTT address.
- `table.get_locations` builds source locations.
- An unreadable or non-ELF artifact, or an ELF with neither `_SEGGER_RTT` nor a
  defmt table, is rejected synchronously with `400 invalid_artifact` when it is
  explicitly associated with a session. Flash uploads themselves remain opaque
  bytes; a successful flash with no usable metadata clears the probe record.

Decoding rules:

- One stream decoder per RTT channel. A frame from channel `n` never feeds
  channel `m`'s decoder.
- Partial frames are retained between producer chunks.
- One producer chunk may yield zero or more complete defmt frames.
- A chunk with frames produces one `rtt.records` message containing an ordered
  array of records.
- A chunk with no active defmt profile produces `rtt.raw` with the original
  bytes.
- `DecodeError::UnexpectedEof` means "wait for more bytes", not an error.
- `DecodeError::Malformed` emits any complete frames decoded before the error,
  resets that channel's decoder, emits a rate-limited `rtt.decode_error`, and
  forwards the remaining undecoded bytes as `rtt.raw` so no data is lost.
- `frame.display_timestamp()`, `frame.level()`, `frame.display_message()`, and
  the source location are converted to structured fields. The client does not
  need the defmt table or location map.

The decode profile is cached in memory by artifact ID or SHA-256 so switching
to an artifact already used by another session does not reparse the ELF.

### 5.7 Sequence and event model

The existing data sequence remains **one sequence per producer read chunk**.
That keeps `?after=`, gaps, ring eviction, and `oldest_seq`/`latest_seq`
semantics unchanged. A decoded event may contain several frames for one
sequence. A chunk that only contributes bytes to a partial frame still advances
the sequence and gets a ring entry; the entry advances the subscriber cursor but
is filtered from live output because it has no display records.

To keep replay consistent with live output, every chunk processed by an active
defmt decoder gets a decoded log record, even when it contains no complete
frame. That empty record is the marker that says "these raw bytes were decoder
input, not display output". Replay preference is therefore per sequence: if a
decoded record exists, use it (including its optional raw fallback); otherwise
use the raw output record. This prevents a partial frame from being printed as
raw bytes and then again as decoded text.

`data.bytes` continues to count raw output bytes, not the expanded decoded
representation. Storage accounting for decoded companion records is separate.
The live session ring remains byte-bounded; decoded events count against the
ring by their encoded size, and the existing RTT ring limit is kept.

## 6. Protocol changes

All types live in `boot-protocol`. The generated HTTP client follows the
OpenAPI changes automatically.

### 6.1 Session request

Add a top-level optional artifact to `ProbeSessionRequest`:

```rust
pub struct ProbeSessionRequest {
    pub kind: String,
    #[serde(default)]
    pub options: SessionOptions,
    /// Firmware artifact to use for RTT address lookup and defmt decoding.
    /// Overrides the probe's last flashed artifact.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Inherit the probe's last flashed artifact when `artifact` is absent.
    #[serde(default = "default_true")]
    pub inherit_artifact: bool,
    #[serde(default = "default_max_subscribers")]
    pub max_subscribers: u32,
    #[serde(default = "default_detach_timeout")]
    pub detach_timeout_seconds: u64,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u64,
}
```

`SessionOptions` keeps `rtt_addr` as an explicit override. No artifact field is
added there; serial sessions must not inherit RTT decode options.

### 6.2 Session snapshot

`Session` gains a `decode` object:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionDecode {
    pub mode: String,
    pub epoch: u64,
    pub artifact: Option<SessionArtifact>,
    pub rtt_addr: Option<String>,
    pub rtt_addr_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionArtifact {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: String,
    pub defmt: bool,
    pub rtt_addr: Option<String>,
}
```

`Session.decode` is part of `hello`, so a new subscriber knows immediately
whether output will be `defmt` or `raw` and which artifact is in use. Sessions
created without an artifact start with `mode = "raw"`, `epoch = 0`, and no
artifact.

`Artifact` also gains an `in_use: bool` field so `POST /v1/artifacts` and
`GET /v1/artifacts/{id}` can report whether deleting it would conflict with a
session, probe record, or in-flight flash change. `POST` returns `false`; `GET`
derives the value from `artifact_refs` and `probe_artifacts`.

`Probe` gains an optional last-flash record:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ProbeArtifact {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    pub flashed_at: String,
    pub change: Option<String>,
    pub created_by: Option<String>,
    pub defmt: bool,
    pub rtt_addr: Option<String>,
}

pub struct Probe {
    // ... existing fields ...
    pub last_artifact: Option<ProbeArtifact>,
}
```

`last_artifact` is metadata only. It never causes a hardware attach by itself;
it is used when a session requests inheritance. `GET /v1/probes` and
`GET /v1/probes/{id}` both include it.

### 6.3 RTT data messages

RTT data uses text frames. Serial keeps its existing binary frame:
`u64 seq (big-endian) + payload`.

New `SessionWsMessage` variants:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RttRecord {
    /// Defmt timestamp rendering, when the table has timestamp data.
    pub timestamp: Option<String>,
    /// Lowercase level: trace, debug, info, warn, error.
    pub level: Option<String>,
    pub module: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

pub enum SessionWsMessage {
    // ... existing variants ...

    #[serde(rename = "rtt.records")]
    RttRecords {
        seq: u64,
        channel: u8,
        epoch: u64,
        records: Vec<RttRecord>,
    },
    #[serde(rename = "rtt.raw")]
    RttRaw {
        seq: u64,
        channel: u8,
        epoch: u64,
        /// Base64; the bytes are not necessarily UTF-8.
        data: String,
    },
    #[serde(rename = "rtt.decode_error")]
    RttDecodeError {
        seq: u64,
        channel: u8,
        epoch: u64,
        message: String,
    },
    #[serde(rename = "session.decoder")]
    DecoderChanged { session: Session },
}
```

A typical decoded session produces:

```json
{"type":"rtt.records","seq":42,"channel":0,"epoch":1,
 "records":[
   {"timestamp":"12.345678","level":"info","module":"app",
    "file":"src/main.rs","line":42,"message":"sensor ready"}
 ]}
```

A raw-mode session, or a fallback chunk inside a decoded session, produces:

```json
{"type":"rtt.raw","seq":43,"channel":0,"epoch":0,
 "data":"aGVsbG8K"}
```

(The epoch in the example is `0` for a session with no artifact; a fallback
inside an artifact-backed session carries that session's current epoch.)

`session.decoder` is sent to every live subscriber when the artifact is set,
replaced, or cleared. It carries the full updated session, including
`decode.epoch` and `decode.mode`.

Text control messages (`ping`, `pong`, `error`, input records) are unchanged.
The old RTT binary framing is retired. There is no dual format: a client that
does not understand `rtt.records`/`rtt.raw` is unsupported.

## 7. HTTP surface

### 7.1 New and changed endpoints

| Method | Path | Scope | Purpose |
| --- | --- | --- | --- |
| `GET` | `/v1/probes` | read | Includes `last_artifact` per probe. |
| `GET` | `/v1/probes/{id}` | read | Includes `last_artifact` per probe. |
| `POST` | `/v1/probes/{id}/sessions` | control | Accepts optional `artifact` and `inherit_artifact`. |
| `PUT` | `/v1/sessions/{id}/artifact` | control | Set or replace the decoder artifact. |
| `DELETE` | `/v1/sessions/{id}/artifact` | control | Clear the decoder artifact and return to raw mode. |
| `DELETE` | `/v1/probes/{id}/artifact` | control | Forget the probe's last-flashed artifact and release its reference; returns empty success. |
| `GET` | `/v1/sessions/{id}` | read | Includes `decode` in the session. |

There is no `GET /v1/sessions/{id}/artifact`; the session snapshot already
contains the artifact summary. Session `PUT` and `DELETE` return the updated
`Session`. The probe delete returns an empty success.

### 7.2 Create an RTT session with inherited or explicit artifact

With no artifact field, the session inherits the probe's last flashed artifact:

```http
POST /v1/probes/probe_1a2b.../sessions HTTP/1.1
Authorization: Bearer boot_v1_...
Content-Type: application/json

{
  "kind": "rtt",
  "options": { "channels": [0] },
  "max_subscribers": 8,
  "detach_timeout_seconds": 60
}
```

The response's `decode.artifact` identifies the inherited artifact. To opt out
and use scan/raw output even when the probe has a recorded artifact, send:

```json
{ "kind": "rtt", "inherit_artifact": false, "options": { "channels": [0] } }
```

An explicit `artifact` always wins over inheritance:

```http
POST /v1/probes/probe_1a2b.../sessions HTTP/1.1
Authorization: Bearer boot_v1_...
Content-Type: application/json

{
  "kind": "rtt",
  "artifact": "art_01J9Z6...",
  "options": { "channels": [0] },
  "max_subscribers": 8,
  "detach_timeout_seconds": 60
}
```

The response shape is the same for inherited and explicit artifacts:

```json
{
  "id": "ses_01J9Z6...",
  "kind": "rtt",
  "resource_id": "probe_1a2b...",
  "state": "starting",
  "decode": {
    "mode": "defmt",
    "epoch": 1,
    "artifact": {
      "id": "art_01J9Z6...",
      "size": 491520,
      "sha256": "...",
      "created_at": "2026-11-13T12:00:00Z",
      "defmt": true,
      "rtt_addr": "0x20000000"
    },
    "rtt_addr": "0x20000000",
    "rtt_addr_source": "artifact"
  }
}
```

If the artifact has no `_SEGGER_RTT`, the session is still created and
`rtt_addr_source` is `scan` (unless an explicit address was supplied). An
explicit malformed artifact fails with `400 invalid_artifact` before a session
is created. An inherited malformed or missing artifact is ignored with a
warning, and the session starts in raw/scan mode.

### 7.3 Replace the decoder

```http
PUT /v1/sessions/ses_01J9Z6.../artifact HTTP/1.1
Authorization: Bearer boot_v1_...
Content-Type: application/json

{ "artifact": "art_01J9Z7..." }
```

- The new artifact is validated and referenced by the session.
- `decode.artifact` and `decode.mode` are updated.
- `decode.epoch` increments.
- `decode.rtt_addr` and `decode.rtt_addr_source` keep the producer's current
  address and source. `decode.artifact.rtt_addr` shows the new artifact's
  address.
- The session's reference to the old artifact is released.
- Live subscribers receive `session.decoder`.

Setting the same artifact ID is idempotent: no epoch bump, no reparse from the
client's perspective. Clearing an already-cleared decoder is also a no-op.

```http
DELETE /v1/sessions/ses_01J9Z6.../artifact HTTP/1.1
Authorization: Bearer boot_v1_...
```

Clearing sets `decode.mode = "raw"`, bumps the epoch, releases the session's
artifact reference, and notifies subscribers. Retained decoded records remain as
they were captured.

Forgetting the probe record is separate from clearing a session decoder:

```http
DELETE /v1/probes/probe_1a2b.../artifact HTTP/1.1
Authorization: Bearer boot_v1_...
```

This removes `Probe.last_artifact` and releases the probe's artifact reference.
Existing sessions keep their own reference; future sessions stop inheriting. The
endpoint is idempotent and returns an empty success. It does not require the
probe to be currently present; it operates on the stable resource ID's record.

### 7.4 Errors

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid_artifact` | 400 | Artifact is not a readable ELF, or supplies neither an RTT address nor a defmt table. |
| `artifact_in_use` | 409 | Delete was refused because a session, probe record, or flash change references the artifact. |
| `not_found` | 404 | The artifact or session does not exist. |
| `not_rtt_session` | 400 | An artifact was set on a serial session. |
| `session_closed` | 409 | Artifact changes are not allowed on a closed/failed session. |
| `rtt_addr_mismatch` | warning | The new artifact's `_SEGGER_RTT` differs from the live address; returned in the server log, not as a request failure. |

Artifacts never expire, so there is no `artifact_expired` error. A missing
artifact is `404 not_found`.

## 8. Runtime and persistence

### 8.1 Session runtime

`SessionHandle` gains a decode state guarded by its existing state lock or a
dedicated mutex:

- the current `DecodeProfile`;
- one decoder per active RTT channel;
- the current epoch;
- a rate-limited decode-error marker.

`defmt_decoder::StreamDecoder` borrows its `Table`, so the per-channel state is
wrapped in a small owning type (for example with `self_cell`) rather than
leaking tables or reparsing per chunk.

`push_output` changes from "store bytes" to:

1. allocate the producer sequence;
2. append a raw `RECORD_OUTPUT` record;
3. if a defmt profile is active, decode the chunk and append a
   `RECORD_DECODED` record containing the ordered `RttRecord` values and any
   raw fallback;
4. append the subscriber-visible event to the ring;
5. update session stats and notify subscribers.

The producer thread remains hardware-only. Decoding happens in the session
actor, not in the RTT worker, so a slow decode does not delay hardware reads.

### 8.2 Flash completion and the probe artifact

A flash change references its artifact when the change is created. The reference
is owned by the change until it reaches a terminal state.

When a flash change reaches `done`, the runtime:

1. loads or reuses the artifact's decode profile;
2. if it is a readable ELF with `_SEGGER_RTT` or defmt data, upserts
   `probe_artifacts` for the stable resource ID with the artifact ID, SHA, size,
   flash time, change ID, principal, defmt presence, and `_SEGGER_RTT`;
3. otherwise deletes any existing `probe_artifacts` row and logs that the probe
   has no decodable last artifact;
4. releases the change reference, while the probe record (if any) holds the new
   reference.

The update must not fail the flash change. A persistence or parse failure is
logged, the change remains `done`, and the probe record is left absent or
cleared rather than pointing at an image that was not successfully written.

The runtime keeps an in-memory map of probe artifacts, loaded at startup and
updated on flash completion or explicit clear. `Probe.last_artifact` is rendered
from that map, so `GET /v1/probes` does not perform one database query per
probe.

Session creation then selects the artifact:

- explicit request artifact: validate it synchronously; failure rejects the
  session;
- inherited probe artifact: if the row, artifact, or file is missing or
  unreadable, log a warning and create the session with no artifact
  (scan/raw);
- `inherit_artifact=false`: do not consult the probe record.

This distinction matters: an explicit request for a bad artifact is a client
error, while an unavailable inherited default is a degraded convenience.

### 8.3 Session log

Add a record kind:

```text
RECORD_OUTPUT  = 0   # raw bytes, canonical
RECORD_INPUT   = 1   # serial input
RECORD_GAP     = 2   # reserved
RECORD_LIFECYCLE = 3 # reserved
RECORD_DECODED = 4   # JSON {records, raw} for the same seq
```

`RECORD_DECODED` is a derived companion to `RECORD_OUTPUT` with the same
sequence and channel. Its payload is a small JSON object:

```json
{ "records": [ /* RttRecord values */ ], "raw": "base64-or-null" }
```

`records` holds complete frames produced from this chunk; it may be empty when
the chunk only contained a partial frame. `raw` holds undecoded bytes after a
malformed frame or raw fallback; it is normally null. Replay walks records and
prefers the decoded record for a sequence whenever one exists, even if its
record array is empty. If none exists it returns the raw record. Input records
remain separate and are only emitted with `include_input=true`.

This means:

- replay does not need the artifact;
- export can render defmt output as text;
- replacing or clearing the artifact does not rewrite history;
- raw bytes are still available for debugging and future decoders.

### 8.4 Database

Extend the existing `sessions` table (there are no deployments, so extend the
initial migration rather than adding a migration chain):

```sql
-- Add to the initial sessions table.
    artifact_id   TEXT,
    decode_json   TEXT NOT NULL DEFAULT '{}'  -- serialized SessionDecode
```

`decode_json` contains the full `SessionDecode`, including the artifact summary,
epoch, resolved producer address, and address source. Storing it as one small
JSON blob keeps the schema simple and lets the decode shape evolve with the
protocol. `artifact_id` is duplicated as a plain column for reference queries.
The artifact summary remains meaningful after the artifact is explicitly
deleted; `artifact_refs` is only the session/change reference.

Add a general reference table for sessions and in-flight changes:

```sql
CREATE TABLE artifact_refs (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE RESTRICT,
    owner_kind  TEXT NOT NULL CHECK (owner_kind IN ('session','change')),
    owner_id    TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (artifact_id, owner_kind, owner_id)
);

CREATE INDEX artifact_refs_by_artifact ON artifact_refs(artifact_id);
```

Session refs use `owner_id = sessions.id`; flash-change refs use
`owner_id = changes.id`. Session close/fail/purge and any terminal change
remove their refs. A probe does not use `artifact_refs`; its `probe_artifacts`
row is the reference.

Add the probe's last-flash record:

```sql
CREATE TABLE probe_artifacts (
    resource_id  TEXT PRIMARY KEY,
    artifact_id  TEXT NOT NULL REFERENCES artifacts(id) ON DELETE RESTRICT,
    sha256       TEXT NOT NULL,
    size         INTEGER NOT NULL,
    defmt        INTEGER NOT NULL DEFAULT 0,
    rtt_addr     TEXT,
    change_id    TEXT,
    created_by   TEXT,
    flashed_at   INTEGER NOT NULL
);

CREATE INDEX probe_artifacts_by_artifact ON probe_artifacts(artifact_id);
```

`probe_artifacts` is keyed by stable resource ID, so it survives inventory
churn. The row is also the probe's artifact reference. A successful flash
replaces it atomically with the new artifact and removes the change ref; a
non-ELF or RTT-less success deletes it and leaves the probe with no inherited
artifact.

`artifact::delete` rejects artifacts referenced by `artifact_refs` or
`probe_artifacts`. There is no expiry sweep and no `expired_artifacts` query.
`DELETE /v1/probes/{id}/artifact` deletes the probe row and releases the
reference.

The initial `artifacts` table drops `expires_at` and the
`artifacts_by_expiry` index. Artifact rows remain until the file and row are
explicitly deleted. The startup `artifact::cleanup` call and the cleanup tick in
the runtime maintenance task are removed; there is no `expired_artifacts`
store method.

```sql
CREATE TABLE artifacts (
    id          TEXT PRIMARY KEY,
    path        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
```

### 8.5 Recovery

On startup:

1. Load sessions and their decode fields.
2. Load `probe_artifacts` into the runtime map used for session inheritance.
3. Load artifact refs and reparse profiles for sessions that are not terminal.
4. Remove `artifact_refs` rows whose owner is a change, because no change
   resumes after a restart and any in-flight change is marked failed. Also
   prune refs for sessions that are already terminal.
5. If a referenced artifact file is missing, set the session's `decode.mode =
   "raw"`, keep the stored `rtt_addr`, append a decode warning, and continue.
   A probe record with a missing file is ignored for inheritance.
6. Resume RTT sessions with the persisted `rtt_addr` (or scan if none), exactly
   as today. The artifact is not needed to reattach.
7. New output after recovery uses the restored decoder when available, or raw
   fallback otherwise.

A closed/failed session has no reference but still has stored decode summary
and decoded records, so replay works. A probe may still have a last artifact
even when no session exists; it is kept until replaced, cleared, or the
artifact is explicitly deleted after the probe record no longer references it.

## 9. CLI UX

### 9.1 `attach-probe`

```text
boot attach-probe <server>/<probe> [--elf <path> | --artifact <id>]
                                  [--clear-artifact]
                                  [--follow | --new]
                                  [--read-only] [--session <id>]
```

Behavior:

| Flags | Session exists | No session |
| --- | --- | --- |
| none | Attach and use the session decoder. | Create with `inherit_artifact=true`; use the probe's last flashed artifact if present. |
| `--elf <path>` | Upload the ELF, set it as the session decoder, then attach. | Upload the ELF and create the session with it. |
| `--artifact <id>` | Set that artifact as the decoder, then attach. | Create the session with that artifact. |
| `--clear-artifact` | Clear the session decoder, then attach. | Create with `inherit_artifact=false` and no artifact. |
| `--follow` | Required to exist. With a decoder flag, update the decoder first. | Error. |
| `--new` | Error if an active session exists. | Create as requested. |

`--elf` means "use this firmware for address lookup and decoding", not "decode
locally". The CLI may compute SHA-256 locally and skip uploading when the
session already references the same SHA. A decoder flag applied to an existing
session changes output for every subscriber, because the decoder is session
state; the CLI should say so in the status line. `--clear-artifact` clears the
session or opts out of inheritance; it does not forget the probe's last-flash
record.

Output changes from a local encoding label to server decode state:

```text
✓ Attached to local/0483:374b:... RTT
  decoder: defmt (art_01J9Z6..., inherited last flash, sha256 9f2c1a7b...)
```

or:

```text
  decoder: raw (no artifact or probe has no last flash)
```

When a live decoder changes:

```text
Decoder changed to art_01J9Z7... (defmt)
```

Ingest rules:

- `rtt.records`: print `timestamp level module:line > message`, matching the
  current display format.
- `rtt.raw`: write `String::from_utf8_lossy` to stdout, matching current
  non-defmt behavior.
- `rtt.decode_error`: print a warning to stderr, rate-limited per channel.
- `session.decoder`: update the label; do not re-render scrollback.

### 9.2 `run-firmware`

`run-firmware` already has the artifact ID from its upload. It passes that ID
to the post-flash attach instead of re-reading the ELF. The attach output is
decoded without any client-side table.

On a successful flash the server records the artifact as the probe's last
flash. Therefore `run-firmware --no-attach` is now a useful workflow: a later
`attach-probe` with no flags inherits the artifact and decodes without any
client-side file. The record is not changed if the flash fails or is cancelled.

The current `--new` behavior after flashing is kept: if an active RTT session
exists, the command reports it. A developer who wants to change the decoder of
the existing session can run `attach-probe --elf <new build>`; that is the
explicit replacement path.

If the flashed artifact is not a readable ELF, or is an ELF with neither
`_SEGGER_RTT` nor defmt data, the server clears the probe's last-flash record
and `run-firmware` continues in raw mode with a warning instead of failing after
a successful flash. An explicit `attach-probe --elf` still returns
`invalid_artifact` when the file cannot supply any RTT metadata, because the
user asked to use that file as the decoder.

### 9.3 Replay, export, and list

- `replay-session` uses `rtt.records` and `rtt.raw` from the server. It needs no
  `--elf`, and the CLI no longer parses defmt.
- `export-session` uses `RECORD_DECODED` when present, so defmt sessions export
  as readable text instead of failing on binary data. Sessions with raw
  non-UTF-8 output keep the existing conflict behavior.
- `list-sessions` gains a decode column, for example `defmt:art_...` or `raw`.
- `list-probes` shows the last flashed artifact, for example
  `fw:art_01J9Z6... (defmt)`, so it is visible which decoder a flag-free attach
  will inherit.
- The CLI drops `defmt-decoder`, `defmt-parser`, and `object` dependencies once
  no other command uses them.

### 9.4 Example flows

```text
# Flash and attach. The artifact is uploaded once, recorded on the probe as the
# last flash, and recorded on the session.
boot run-firmware local/probe firmware.elf

# Another machine follows with no ELF and no decoder setup.
boot attach-probe local/probe

# Flash without attaching; a later attach still inherits the artifact.
boot run-firmware local/probe firmware.elf --no-attach
boot attach-probe local/probe

# Rebuild without resetting the target: upload the new ELF and switch the
# existing session decoder.
boot attach-probe local/probe --elf firmware-new.elf

# Reuse an artifact from a previous build.
boot attach-probe local/probe --artifact art_01J9Z6...

# Opt out of the inherited probe artifact and stream raw bytes.
boot attach-probe local/probe --clear-artifact

# Forget the probe's remembered firmware (API; a new successful flash replaces
# it automatically).
curl -X DELETE .../v1/probes/probe_1a2b.../artifact
```

## 10. Security, quotas, and operations

- Setting or clearing a session artifact, and clearing a probe's last-flash
  record, require `control`. `flash` already requires `control`, so a client
  cannot change the probe default without permission. `read` may see artifact
  metadata but cannot attach to data.
- Artifact contents are never returned by the session API; only ID, size, SHA,
  timestamps, defmt presence, and derived RTT address.
- The server validates that an associated artifact is a readable ELF. It does
  not execute or map it.
- Artifacts persist until explicitly deleted, so storage grows with uploads.
  Add an optional per-session, per-probe, or global artifact quota and include
  artifact bytes in `storage_exhausted` accounting. There is deliberately no
  automatic deletion. A probe's last-flash record is naturally bounded to one
  artifact per stable resource; a successful flash replaces it and releases the
  old reference.
- Decoding is cheaper than per-subscriber client decoding for multiple
  followers, but it still costs CPU and memory per session. Limit the number of
  active RTT channels (existing `channels` option) and keep the profile cache
  bounded.
- Decoded text can contain secrets exactly like raw RTT output. Existing
  session retention, file permissions, and purge rules apply to both raw and
  decoded records.
- No endpoint lets a client set a probe's last-flash record directly. Only a
  successful `flash` change updates it; `DELETE /v1/probes/{id}/artifact` can
  clear it. An attach-time decoder upload changes the session, not the probe
  default.
- `artifact_refs` and `probe_artifacts` must be updated transactionally with
  session/change close and flash completion so a crash cannot leave a permanent
  reference or delete an artifact still in use.
- If artifact ownership is added later, enforce it on association as well as
  deletion. Until then, any `control` principal may associate any artifact,
  which matches the current trusted-lab model.

## 11. Edge cases

| Case | Behavior |
| --- | --- |
| Artifact has no `_SEGGER_RTT` | Session uses explicit address or scan; artifact still decodes defmt if present. |
| Artifact has no defmt section | Session runs in `raw` mode; artifact still supplies an address. |
| Artifact is not an ELF | Association fails synchronously with `400 invalid_artifact`. |
| Artifact has no expiry | Remains until explicit `DELETE`; there is no reaper. |
| Artifact delete while referenced | `409 artifact_in_use`; release the reference by clearing/closing/purging the session or clearing the probe record first. |
| Artifact file missing at recovery | Session resumes from stored address and falls back to raw; probe record is ignored for inheritance; warning recorded. |
| Flash fails or is cancelled | Probe last-flash record is unchanged; the flash change reference is released. |
| Flash succeeds with a non-ELF or RTT-less artifact | Probe last-flash record is cleared and its old reference released; the running firmware remains undecodable until a new ELF is associated with a session. |
| New flash succeeds | Probe record is atomically replaced; the old artifact remains until all its references are released and it is explicitly deleted. |
| Explicit artifact on session create | It overrides the probe record; the probe record is unchanged. |
| `inherit_artifact=false` on session create | The probe record is unchanged; this session starts raw/scan-only. |
| Probe disappears and returns | The last-flash record is keyed by stable resource ID and is still available. |
| Two sessions inherit the same probe artifact | Both sessions reference it; replacing the probe record releases only the probe's reference, not the sessions'. |
| Probe record points at a missing artifact | Session creation ignores inheritance, warns, and creates a raw/scan session. |
| Server restarts mid-flash | The change is failed, its change ref is pruned, and the probe's previous record remains; firmware may have been partially written. |
| Artifact replaced with a different `_SEGGER_RTT` | Decode switches; live RTT attachment unchanged; `rtt_addr_mismatch` warning. |
| Malformed defmt frame | Decoder for that channel resets; `rtt.decode_error`; remaining undecoded bytes emitted as raw. |
| Partial frame across chunks | Decoder retains bytes; no live event until a complete frame or fallback, while the decoded log marker records the decoder input. |
| Multiple frames in one chunk | One `rtt.records` event with an ordered record array. |
| Multiple RTT channels | Independent decoders per channel; records carry the channel. |
| Artifact cleared while streaming | Epoch bumps; future chunks are `rtt.raw`; old decoded records remain. |
| Decoder change during replay | No effect; replay uses stored decoded records. |
| Session closed while artifact update is in flight | Update loses the race and returns `409 session_closed`; no reference is leaked. |
| Two sessions reference the same artifact | One artifact row, two refs; explicit delete requires both to be released. |
| Artifact never deleted | It remains on disk and in the database indefinitely; optional upload quota is the only storage guard. |
| Server restarts mid-stream | Raw and decoded records already written remain; new output continues after the persisted sequence. |

## 12. Alternatives considered

### 12.1 Keep decoding on the client but return the artifact ID

The server could record the artifact and tell clients which one to fetch, while
each client still parses and decodes. This avoids moving dependencies but keeps
P1, P2, and P4: every client still needs the exact ELF and a compatible
decoder, and output can diverge.

Rejected. The point of recording the artifact is a shared server interpretation.

### 12.2 Store only decoded text

The server could decode at ingestion and store only rendered text. This is
simple and makes replay trivial, but loses raw bytes, prevents future
re-decoding, and makes a bad decode permanent.

Rejected. Raw remains canonical; decoded is a companion record.

### 12.3 Decode per subscriber at fan-out

Each subscriber could own a server-side decoder and receive raw bytes internally.
That mirrors today's client behavior and avoids persisting decoded records, but
it duplicates CPU, complicates replay cursor alignment, and still needs the
artifact at replay time.

Rejected for the first implementation. Per-channel decoding once at ingestion is
simpler and gives all subscribers the same output.

### 12.4 Copy the ELF into the session directory

Copying would make the session self-contained without artifact references, but
it duplicates up to 64 MiB per session and bypasses the existing artifact store.
References plus a persisted decoded journal give the same replay guarantee with
less duplication.

Rejected.

### 12.5 Put the artifact in `SessionOptions`

Options are already persisted as immutable hardware configuration. The artifact
is mutable and is not a hardware option. Mixing them would make `PATCH` and
serial options confusing.

Rejected.

### 12.6 Keep RTT binary frames and add decode metadata

A binary envelope with a per-frame type could be more compact than JSON, but
RTT logs are low-volume and the text representation is easier for clients,
replay, and debugging. A future high-rate binary mode can be added separately.

Rejected for now.

### 12.7 Require an explicit `--use-last-flash` flag

The probe record could exist but only be used when the client asks for it.
That avoids surprising inheritance, but the common workflow
(`run-firmware --no-attach`, then `attach-probe`) would still need an extra
flag, which is the UX this design is trying to remove.

Rejected. Inheritance is the default; `--clear-artifact` and an explicit
artifact are the escape hatches.

### 12.8 Set the probe record from the attach command

An attach-time `--elf` could update the probe's last-flash record as well as the
session decoder. That would let a build not flashed through `boot` become the
probe default, but it would make an observation command mutate persistent
defaults and could disagree with the firmware actually running.

Rejected. Only a successful flash changes the probe record.

## 13. Compatibility and rollout

- There are no existing deployments. Change `/v1` in place and do not add
  aliases, dual framing, or compatibility migrations.
- Extend the initial database schema rather than adding a migration chain,
  including `probe_artifacts` and the generalized `artifact_refs`.
- `Probe` gains `last_artifact`; `Artifact` loses `expires_at` and gains
  `in_use`. Both crates are updated together.
- Keep `PROTOCOL_VERSION = 1`. A mixed old/new client is unsupported; the RTT
  data-plane change is intentional and both crates are updated together.
- `boot-api-client` is regenerated from OpenAPI for the new request and session
  fields. WebSocket types come from `boot-protocol`.
- Update:
  - `docs/api-architecture-design.md` §10.4 and §11.2;
  - `docs/cli-command-structure.md` for the attach flags;
  - `docs/authentication.md` if the artifact/session scope wording changes.

## 14. Testing

No test requires real hardware.

### 14.1 Unit tests

- Parse an ELF fixture with `_SEGGER_RTT` and defmt data; assert the address,
  table presence, and locations.
- Parse an ELF without defmt; assert raw mode and address extraction.
- Reject a non-ELF artifact with `invalid_artifact`.
- Decode split frames across chunks, multiple frames per chunk, and malformed
  input with decoder reset.
- Verify per-channel decoders do not cross-feed.
- Verify `decode_epoch` increments on set, replace, and clear, and does not
  increment for a no-op.
- Verify raw output is logged even when decoding succeeds.
- Verify replay prefers `RECORD_DECODED` and falls back to `RECORD_OUTPUT`.
- Verify export renders decoded defmt text.
- Verify artifact references: delete conflict, release on close/purge, multiple
  session references, flash-change reference release, probe reference release,
  and missing file at restore.
- Verify artifacts have no expiry path: no cleanup task, no `expired_artifacts`
  query, and an unreferenced artifact remains until explicit deletion.
- Verify successful flash writes `probe_artifacts`; failed/cancelled flash does
  not; non-ELF success clears it.
- Verify a new successful flash replaces the probe record and releases the old
  reference; the old artifact remains until explicitly deleted.
- Verify inherited artifact selection: explicit > probe > none, and
  `inherit_artifact=false` skips the probe record.
- Verify a missing probe artifact is ignored for inheritance but an explicit
  missing artifact is an error.

### 14.2 HTTP tests

- `POST /v1/probes/{id}/sessions` with an explicit artifact returns `decode`
  with the derived address and defmt mode.
- `POST` with no artifact inherits the probe record; `inherit_artifact=false`
  does not.
- An explicit artifact overrides the probe record.
- A missing explicit artifact returns `not_found`; a missing inherited artifact
  falls back to raw/scan.
- `PUT`/`DELETE /v1/sessions/{id}/artifact` update the session and notify
  subscribers.
- `DELETE /v1/probes/{id}/artifact` clears `Probe.last_artifact` and releases
  the probe reference.
- `GET /v1/probes` and `GET /v1/probes/{id}` include `last_artifact`.
- `GET /v1/artifacts/{id}` reports `in_use` correctly for session, probe, and
  change references.
- Artifact operations on a serial session return `not_rtt_session`.
- Artifact operations on a closed session return `session_closed`.
- `DELETE /v1/artifacts/{id}` succeeds after the last reference is released and
  is never triggered automatically by time.
- Concurrent update/close does not leak a reference.

### 14.3 CLI tests

- `attach-probe` parses `--elf`, `--artifact`, `--clear-artifact`, and the
  existing attach flags.
- `attach-probe` with no decoder flag creates a session with
  `inherit_artifact=true`.
- `--clear-artifact` on create sends `inherit_artifact=false`; on an existing
  session it sends `DELETE /v1/sessions/{id}/artifact`.
- `--elf` with no session creates the session with the uploaded artifact.
- `--elf` with an existing session updates the decoder before attaching.
- `run-firmware` passes its uploaded artifact to the post-flash session and the
  server records it as the probe's last flash.
- `run-firmware --no-attach` followed by `attach-probe` with no flags uses the
  inherited artifact.
- Replay and export do not require a local ELF and print decoded records.
- The CLI no longer depends on defmt/ELF parsing crates.

### 14.4 Manual smoke

One hardware session with a real defmt target:

1. flash and attach with `run-firmware`;
2. attach from a second client with no ELF and confirm identical output;
3. flash with `--no-attach`, then attach with no flags and confirm the probe's
   last artifact is inherited and decoding works;
4. upload a rebuilt ELF and confirm new output uses the new decoder;
5. flash a non-ELF or failing image and confirm the probe record is cleared or
   unchanged as appropriate;
6. replay and export the session after closing it and confirm text output;
7. confirm the artifact remains readable while referenced and can be deleted
   after the session is purged and the probe record is cleared, with no expiry
   automatically removing it first.

## 15. Implementation plan

1. **Protocol and schema.** Add `SessionDecode`, `SessionArtifact`,
   `ProbeArtifact`, `Probe.last_artifact`, request and message fields,
   `RECORD_DECODED`, the session decode columns, `artifact_refs`, and
   `probe_artifacts`.
2. **Artifact profile and references.** Move ELF/defmt parsing into `boot-api`,
   add the profile cache, address extraction, delete conflict, and reference
   tracking across sessions, changes, and probes. Remove the TTL field,
   `expired_artifacts`, and the cleanup task.
3. **Probe last-flash record.** Update it on successful flash, clear it on
   non-ELF success, keep it on failure/cancel, expose it on `Probe`, and load it
   at startup.
4. **Session create/resume.** Select explicit/inherited/no artifact, resolve the
   address, persist decode state, restart decoding on recovery, and fall back to
   raw when an inherited artifact is unavailable.
5. **Decode pipeline.** Add per-channel decoders, decoded log records, raw
   fallback, decode errors, and the new RTT WebSocket messages.
6. **Artifact update API.** Add session `PUT`/`DELETE`, probe `DELETE`, and
   `session.decoder` notification, with no reattach.
7. **Replay, export, and CLI.** Switch RTT replay/export to decoded records,
   update attach/run-firmware/list-probes/list-sessions, and remove client-side
   defmt/ELF parsing.
8. **Documentation and tests.** Update the architecture docs, add fixtures, and
   run `cargo test --workspace` and `cargo fmt --check`.

## 16. Open questions for review

1. Should an explicit `--elf`/`--artifact` on `attach-probe` change the decoder
   for every subscriber, or should changing a shared session decoder require a
   separate command? The design allows it, with a clear status line, because
   `control` is the trusted hardware scope and the session is already shared.
2. Should persistent artifact storage be counted against session data quota, a
   separate artifact quota, per-probe quota, or all three? The design removes
   automatic cleanup, so an optional upload/usage quota is the only storage
   guard and leaves the accounting bucket open.
3. Should the CLI expose "forget the probe's last flashed artifact" as a
   command, or leave it to `DELETE /v1/probes/{id}/artifact` until a user
   workflow requires it? No new verb is added in this pass.

## 17. Decision log

| Date | Decision |
| --- | --- |
| 2026-11-13 | Proposal created: session-owned artifact, server-side address lookup and defmt decode, artifact references, decoded companion log records, new RTT text frames. |
| 2026-11-13 | Artifact is mutable session decode state, not an immutable `SessionOptions` field. |
| 2026-11-13 | Artifact replacement changes decoding only; it never moves the live RTT control block. |
| 2026-11-13 | RTT data moves to structured text frames; serial remains binary. |
| 2026-11-13 | Replay/export prefer decoded records and keep raw bytes canonical. |
| 2026-11-13 | Add a probe-level last-flashed artifact, updated only on successful flash, inherited by default for new RTT sessions. |
| 2026-11-13 | A non-ELF flash clears the probe record; a failed/cancelled flash leaves it unchanged. |
| 2026-11-13 | Probe, session, and in-flight flash artifacts are reference-counted; probe artifacts persist until replaced or cleared. |
| 2026-11-14 | Removed artifact TTLs, `expires_at`, and background cleanup. Artifacts live until explicit deletion; references only guard in-use deletes. |
| 2026-11-14 | Implemented: session and probe artifact endpoints, server-side defmt decoding, inherited probe artifacts, decoded replay/export, persistent artifacts, and CLI attach changes. |
