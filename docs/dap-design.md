# DAP completion design

Status: proposed; no implementation started.

This document describes how to finish Debug Adapter Protocol (DAP) support in
`boot`. It supersedes DAP-specific details in
[api-architecture-design.md](api-architecture-design.md) and
[cli-architecture-design.md](cli-architecture-design.md) where they conflict.

## 1. Summary

DAP is already a session kind in the wire model, schema, and CLI, but there is
no backend. Completing it requires four pieces:

1. an in-process DAP engine that owns the probe and speaks Microsoft DAP;
2. runtime rules for an exclusive, non-resumable, close-on-detach data plane;
3. framed DAP messages on the session data plane, which the CLI bridge strips
   before presenting a normal DAP stream to the debugger;
4. a local TCP listener in `boot start-dap` that connects a debugger to the
   server session.

The same data-plane frames are written to the session log. After the session
ends, `replay-session` reads them through `/io`, parses the DAP messages, and
renders them. Replay is display-only; it never connects a second debugger.

The backend is a boot-owned native `boot-dap` crate built on `probe-rs`,
`probe-rs-debug`, and `svd-parser`. There is no vendored-engine fallback; if
the implementation has gaps, we fix the implementation. The external
`probe-rs dap-server` subprocess option is rejected in section 4.3.

## 2. Goals

- A debugger such as VSCode or CLion can connect to
  `boot start-dap <server>/<probe> --port 50000` and debug the selected probe.
- DAP remains a first-class session with an exclusive hardware lease.
- The server, not the debugger, selects the probe resource.
- DAP data-plane frames carry boot sequence/direction metadata; the CLI bridge
  strips that metadata for the debugger.
- The DAP data plane has exactly one live controller/writer and is not
  resumable. Read-only display subscribers are not part of this pass.
- Boot records the DAP data-plane frames and replays them through `/io` for CLI
  rendering after the session ends.
- A DAP client disconnect releases the probe promptly.
- Existing RTT, serial, change, auth, and source-offer behavior is unchanged.

## 3. Non-goals

- Adding a general server event bus.
- Making the DAP protocol stream resumable or attachable by a second debugger.
- Live observation of an in-progress DAP session.
- Feeding a replayed DAP transcript back into a debugger.
- A separate `/trace` endpoint or a separate observation storage format.
- Exporting DAP as asciicast.
- Running DAP without a server, or bypassing the server lease.
- Preserving an unreleased old DAP endpoint or wire format.
- Matching every probe-rs debugger edge case in the first release. Core DAP,
  REPL, and SVD/peripheral views are required; obscure extras can follow.

## 4. Backend decision

### 4.1 Implementation: a boot-owned native adapter

Add a workspace crate, provisionally `boot-dap`, that implements the DAP
adapter directly. It is server-only and is never a dependency of
`boot-protocol` or `boot-api-client`.

This is a larger implementation than adapting an existing engine, but it is
feasible and gives boot long-term control. The hard parts are available as
libraries:

- `probe-rs` provides probe discovery, target attach, core access, breakpoints,
  stepping, memory access, flashing, and RTT.
- `probe-rs-debug` provides DWARF loading, address-to-source lookup, stack
  unwinding, breakpoint verification, and variable expansion.
- `svd-parser` provides the SVD model needed for peripheral and register views.
- The remaining work is DAP framing, request dispatch, debugger state, and
  rendering.

The core DAP surface is small enough to own; REPL and SVD add bounded,
testable work on top of it:

- DAP framing and message types for the requests boot supports;
- initialize, launch, attach, disconnect, and restart;
- breakpoints, configurationDone, continue, pause, next, stepIn, stepOut;
- threads, stackTrace, scopes, variables, evaluate, setVariable, disassemble,
  readMemory, and writeMemory;
- targets, sources, modules, and loadedSources where the client expects them;
- RTT output and RTT input windows;
- REPL commands through `evaluate` with `context = "repl"` and `completions`;
- SVD/peripheral views through scopes, variables, and `evaluate`.

The implementation is a blocking state machine on a dedicated thread. It does
not need a private Tokio runtime. This is simpler than carrying an async
debugger inside boot.

Risks:

- DAP has many client-specific quirks. The adapter must be tested against VSCode
  and at least one second client before it is considered done.
- REPL and SVD support are part of the required scope, so this is not a
  "minimal core only" rewrite.
- The initial implementation will lag probe-rs's debugger in edge cases until
  the test corpus grows.

### 4.2 No vendored fallback

The native adapter is the implementation. If implementation or later testing
exposes gaps, we fix the adapter rather than replacing it with a vendored
probe-rs DAP engine.

Reasons:

- one owned code path, with no fork to sync against probe-rs releases;
- no additional MIT/Apache vendored boundary in the source offer;
- no split between the boot session model and an engine that assumes a local
  CLI process;
- the adapter surface and its tests stay under boot's control.

The adapter may use probe-rs libraries and upstream behavior as a reference,
but it does not vendor the probe-rs DAP server.

### 4.3 Rejected: external `probe-rs dap-server` subprocess

This is less code, but it is not compatible with the architecture:

- `probe-rs` is not shipped in boot releases.
- The child would open hardware outside the boot worker that owns the lease.
- The child selects its probe from the DAP launch request unless boot rewrites
  the request, which makes the proxy DAP-aware anyway.
- It reintroduces the pipe/proxy lifecycle that the session design removed.
- Cleanup on crash or cancellation is split across two processes.

The subprocess route is not a fallback. If the native adapter is abandoned,
revisit this section as a new architecture proposal rather than silently
spawning `probe-rs`.

## 5. Architecture

```text
DAP debugger (VSCode, CLion, ...)
        │ TCP 127.0.0.1:<port>
        ▼
boot-cli `start-dap` bridge
        │ WebSocket binary, raw DAP bytes
        ▼
boot-api HTTP/WS adapter
        │ session handle
        ▼
runtime session / subscriber
        │ bounded input and output channels
        ▼
DapWorker thread
        │ in-process DAP engine
        ▼
probe-rs -> probe

DAP data-plane frames ──────────────────────────────► session log
        │                                                │
        │ CLI strips live frames                         │ /io?after replay
        ▼                                                ▼
DAP debugger                                    boot replay-session renderer
```

The CLI bridge is local to the debugger. The server may be local or remote.
Live frames are framed for transport and stripped by the CLI before they reach
the debugger. The same frames are appended to the session log; after the
session ends, the CLI reads them through `/io` and renders them. There is no
separate trace file or live observation stream.

### 5.1 How DAP normally works

DAP is a JSON protocol over a byte stream. Each message is framed as
`Content-Length: <n>\r\n\r\n<json>`. Boot supports two transports:

- TCP, the default: the debugger connects to `127.0.0.1:<port>`;
- stdio, with `--stdio`: the debugger launches `boot start-dap ... --stdio`
  as a child process and speaks DAP over the child's stdin/stdout.

In stdio mode stdout carries only DAP bytes. All CLI status, progress, warnings,
and errors go to stderr.

A typical sequence is:

1. The client sends `initialize` with its client ID and feature support.
2. The adapter replies with capabilities and sends an `initialized` event.
3. The client sends either `launch` or `attach`.
4. The client sends breakpoint requests, then `configurationDone`.
5. The adapter starts or continues the target and reports `stopped`,
   `continued`, and `output` events.
6. The client issues threads/stackTrace/scopes/variables/step/continue requests.
7. The client sends `disconnect` when the debug session ends.

`launch` and `attach` arguments are adapter-defined, not part of the DAP
standard. In a probe-rs-style adapter:

- `launch` means the adapter may program or reset the target, using `program`
  as the executable to flash.
- `attach` means the adapter attaches to an already-running target and normally
  does not program it.
- `program` is the ELF used for debug information, source/line lookup, and
  breakpoint verification. It is a path on the adapter host. The target itself
  does not receive the path.
- `chip`, `coreConfig`, reset strategy, protocol speed, SVD paths, and RTT
  settings are also adapter-defined configuration.
- After `configurationDone`, the debugger expects the adapter to run or halt
  the target according to its configuration.

The program path matters because the debugger sends source paths and line
numbers from the client. The adapter maps those to addresses using the ELF
debug info. If the server cannot read the client's `program` path, line
breakpoints and stack traces cannot be resolved correctly.

probe-rs has a non-standard `remote_server_mode`, but boot does not implement
it. Boot's remote file handling belongs in the `start-dap` bridge: it reads the
launch configuration, uploads the program and optional SVD/chip-description
files through the artifact API, and replaces the path fields with artifact IDs
in a boot-specific `bootArtifacts` object. The server adapter resolves those
IDs to server-side paths and ignores the original client paths.

This is the supported remote workflow:

- the CLI has the files and the server token;
- the server never needs to read a client path or accept inline base64 in the
  DAP launch message;
- the same mechanism works for local and remote servers;
- uploaded artifacts can be referenced by the session and cleaned up through
  the existing artifact/reference rules.

Uploading the program solves the debug-info path. It does not rewrite source
paths embedded in DWARF. The adapter returns those paths from the artifact and
the debugger resolves them locally, using its normal source mapping.

Probe-rs's own DAP implementation is local-first. The VSCode extension is the
DAP client; it launches `probe-rs dap-server` as the debug adapter on the
machine with the probe, or connects to a standalone instance over TCP. The
adapter is implemented in Rust in the `probe-rs-tools` binary crate, not in the
TypeScript extension. That model works because probe-rs assumes the adapter
process can open the probe and read the build artifacts directly. It is not the
right model for boot: boot already has a server that owns hardware and leases,
so the adapter belongs in the server and the CLI is only the local byte bridge.

## 6. `boot-dap` engine

### 6.1 Libraries and crate layout

`boot-dap` depends on `probe-rs`, `probe-rs-debug`, `svd-parser`, `serde`, and
`serde_json`. It does not depend on a third-party DAP engine. `probe-rs-debug`
is the debug-info/stack/variable library; it does not provide DAP framing or a
server.

A local `protocol` module owns the DAP framing and the subset of standard
messages boot supports. Custom REPL, RTT, and SVD behavior uses the standard
`evaluate`, `completions`, `scopes`, and `variables` requests plus
adapter-specific arguments, so the protocol layer stays small.

Suggested layout:

```text
boot-dap/src/
  lib.rs          # DapBackend, DapIo, errors
  codec.rs        # Content-Length framing and streaming parser
  messages.rs     # request/response/event types used by boot
  session.rs      # launch/attach, target state, configuration
  debug.rs        # breakpoints, stepping, stack frames, variables
  repl.rs         # evaluate(context = "repl") and completions
  svd.rs          # SVD parse/cache and peripheral variables
  rtt.rs          # DAP RTT output windows and input prompts
```

The crate exposes one blocking entry point:

```rust
pub struct DapIo {
    /// Requests and DAP bytes from the WebSocket subscriber.
    pub input: std::sync::mpsc::Receiver<Vec<u8>>,
    /// Bytes and output events sent to the WebSocket subscriber.
    pub output: tokio::sync::mpsc::Sender<Vec<u8>>,
}

pub trait DapArtifactResolver: Send + Sync {
    /// Resolve an uploaded artifact ID to a server-side path and register a
    /// session reference.
    fn resolve(&self, artifact_id: &str) -> Result<std::path::PathBuf, DapError>;
}

pub struct DapLaunch {
    /// Authoritative probe selector from the boot resource.
    pub probe_selector: String,
    /// Optional server-side ELF/artifact path.
    pub program_override: Option<std::path::PathBuf>,
    /// Resolves `bootArtifacts` IDs from the launch configuration.
    pub artifacts: std::sync::Arc<dyn DapArtifactResolver>,
}

pub trait DapBackend: Send {
    fn run(
        &mut self,
        io: DapIo,
        launch: DapLaunch,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), DapError>;
}
```

The input side uses a standard channel because the worker needs
`recv_timeout` to interleave requests with target polling. The output side uses
a Tokio channel so the API WebSocket handler can select on it directly. Both
are bounded. A test implementation can echo bytes or produce scripted DAP
responses.

The worker does not produce replay records. The runtime frames and logs the
data-plane bytes on the way in and out, so the same framing is used for the
debugger transport and for replay.

### 6.2 Thread model

`DapWorker` is one dedicated OS thread per DAP session, named
`boot-dap-<session-id>`, with a larger stack (4 MiB) because debug-info and
unwinding code can recurse.

This is the same pattern as the current RTT and serial session threads. It is
not the architecture's `ProbeWorker`: that worker is for finite probe
operations and resource lifetime, while a DAP session owns a lease and a
long-lived debugger state (breakpoints, stack frames, variable caches, RTT).
Reusing the probe worker would couple session-owned state to a cacheable
resource worker and make close, restart, and lease cleanup ambiguous. The lease
still guarantees that a DAP session and a probe change cannot use the probe at
the same time.

The worker is a blocking loop. It does not need a private Tokio runtime and it
never calls `tokio::spawn`. It performs these steps until cancellation or
disconnect:

1. wait for DAP bytes with a short timeout;
2. parse complete DAP messages;
3. handle requests using `probe-rs` and `probe-rs-debug`;
4. poll target/core status and emit `stopped`, `continued`, and `output`
   events;
5. write output bytes with `try_send`; the runtime frames and logs them.

This keeps hardware access on the worker thread and makes cancellation
deterministic. The runtime releases the lease only after the worker reports
that it exited (or after a bounded shutdown timeout, see section 11).

### 6.3 DAP lifecycle and configuration

The adapter implements the normal handshake described in section 5.1:

- `initialize` replies with capabilities and emits `initialized`;
- `launch` and `attach` accept the probe-rs-style configuration;
- breakpoint requests and `configurationDone` are applied before running;
- `disconnect` closes the debug session and returns the core cleanly.

The boot resource is authoritative:

- The session's `resource.selector` is injected into the launch/attach
  configuration, replacing any `probe` value sent by the debugger.
- If the debugger asks for a different probe, boot logs it and uses the
  session's probe anyway. It must never open a different probe.
- `chip`, `cwd`, core configuration, reset options, SVD paths, and RTT
  settings come from the DAP client unless boot has an explicit override.

Program and file selection is explicit, in this order:

1. an explicit session artifact from `start-dap --elf` or `--artifact`;
2. a bridge-uploaded `bootArtifacts.program` from the launch/attach request;
3. the probe's last flashed artifact, but only when the user passed
   `--inherit-artifact` and the client supplied no program;
4. otherwise fail the launch with a clear error.

The server never reads the DAP client's `program` path directly. That path is a
client-side file handled by the `start-dap` bridge. By the time the request
reaches the adapter, it either references a `bootArtifacts` ID, an existing
`art_...` artifact, or no program at all.

The default is no artifact inheritance. That is deliberate: the debugger's
`program` is the ELF it will use for source lines and breakpoints, and silently
substituting the last flashed image can make the client's source view disagree
with the target. `--inherit-artifact` is the explicit opt-in for the
"debug what I just flashed" workflow.

The `start-dap` bridge is responsible for the upload path. When it sees a
`launch` or `attach` request, it:

- resolves `program`/`coreConfigs[].programBinary`, `svdFile`, and
  `chipDescriptionPath` relative to the request's `cwd` when necessary;
- uploads those files through `POST /v1/artifacts`;
- replaces them with IDs in a boot-specific `bootArtifacts` object, preserving
  the rest of the launch configuration;
- leaves an existing `bootArtifacts` object or an `art_...` value alone;
- fails the request with a DAP error if a path cannot be read.

There is no server-side fallback to the original path. The only supported path
is the CLI bridge.

Example rewritten config:

```json
{
  "bootArtifacts": {
    "program": "art_01J...",
    "svd": "art_01J...",
    "coreConfigs": [
      { "program": "art_01J...", "svd": "art_01J..." }
    ]
  }
}
```

The adapter resolves artifact IDs through a `DapArtifactResolver` supplied by
the runtime. Resolution returns a server-side path and registers an artifact
reference on the DAP session; missing or expired artifacts fail the launch with
a clear error. The session records references so the artifacts cannot be
deleted while the debug session is alive.

The adapter does not require DWARF source paths to exist on the server; it
loads debug info from the program artifact and returns source paths for the
client to resolve.

If an upload fails, the bridge returns a DAP error response for the
launch/attach request and does not start the debug session. Upload progress and
errors go to stderr, not the DAP stream.

### 6.4 Request surface and required extras

The first implementation must support the normal probe-rs debug flow:

- initialize, launch, attach, disconnect, restart;
- breakpoints, instruction breakpoints, and configurationDone;
- continue, pause, next, stepIn, stepOut;
- threads, stackTrace, scopes, variables, evaluate, setVariable;
- disassemble, readMemory, and writeMemory;
- targets, sources, modules, and loadedSources;
- output events and stopped/continued events.

### 6.4.1 REPL

REPL support is required, not optional. It is exposed through `evaluate` with
`context = "repl"` and `completions`. The initial command set matches the
probe-rs debugger surface:

- `backtrace`
- `breakpoint`
- `cpu`
- `info`
- `inspect`
- `registers`
- `rtt`

`completions` must expose command names and their arguments. Commands that
require a halted core reject execution while the target is running.

### 6.4.2 SVD and peripherals

SVD/peripheral support is required, not optional. The adapter:

- parses the configured SVD with `svd-parser`;
- caches peripheral, register, and field metadata;
- exposes a `Peripherals` root variable through `scopes`/`variables`;
- reads register values lazily from target memory;
- supports `evaluate` and `setVariable` for peripheral and register names.

The SVD path and cache follow the launch configuration. If no SVD is supplied,
peripheral scope is omitted rather than failing the session.

## 7. Runtime changes

### 7.1 Creating a DAP session

`Runtime::create_probe_session` stops rejecting `kind == "dap"`.

Validation:

- resource kind must be a probe;
- `max_subscribers` must be exactly `1`; values above 1 return `invalid_request`;
- `detach_timeout_seconds` is forced to `0` and has no effect;
- the data-plane `SessionLogWriter` is opened; the log holds both directions;
- `replayable` starts as `true`; if the log cannot be opened or a later write
  fails, it becomes `false` and the live session continues;
- `data` starts empty and is updated from the framed data-plane records;
- no `SessionRing` is allocated; live output uses the subscriber channel and
  the log is the replay source;
- `SessionDecode.mode` stays `raw`; an explicit DAP artifact is recorded in
  `SessionDecode.artifact` for persistence/reference purposes only. It is
  loaded from artifact metadata directly and is not required to have RTT or
  defmt data;
- an explicit artifact adds an artifact reference. DAP artifact choice is
  create-time only; the existing artifact set/clear endpoints remain RTT-only.

Creation order remains: validate, acquire lease, create handle, persist, spawn
worker. If worker startup fails, the session moves to `failed` and the lease is
released.

Backend selection is injected through the runtime configuration (for example a
`DapBackendFactory` on `RuntimeConfig`/`RuntimeInner`). Production installs the
`boot-dap` engine; tests install a fake that can echo bytes or script responses.
The factory is server-only and is never serialized.

The session starts in `starting`. The worker reports `active` when it is ready
to accept DAP bytes. A DAP session with no subscriber for 30 seconds is closed
with `last_error = "dap_attach_timeout"` so an abandoned API creation cannot
hold the probe indefinitely.

### 7.2 Attaching

Add a DAP-specific attach path in `SessionHandle`:

- only `mode=write` is accepted; `mode=read` returns `invalid_request`;
- if any subscriber already exists, return `dap_exclusive` (409);
- otherwise create the single subscriber and return both the subscriber and
  the DAP output receiver.

`max_subscribers = 1` is defense in depth; the exclusive attach check is the
primary rule.

Because DAP has only one live controller/writer, an attach must not leak if the
WebSocket upgrade fails. On upgrade failure, detach the subscriber and close the
session.

Attaching to a `closed` or `failed` DAP session with `mode=read` replays the
logged data-plane frames through `/io?after=...`. This is a display replay, not
a debugger attach: no hardware is opened, no input is accepted, and the server
sends `session.ended` after the records. If the log is unavailable, it sends
`hello` and `session.ended` without data.

### 7.3 Data path

The DAP data plane uses boot session frames:

```text
seq:u64 (big-endian) | direction:u8 | payload
```

Direction `0` is adapter-to-client; direction `1` is client-to-adapter. The
header is transport metadata. The CLI bridge strips it from live output before
writing to the debugger's TCP stream, so the debugger sees a normal DAP byte
stream.

Channels:

```text
WS binary  -> input channel  -> DapWorker
DapWorker  -> output channel -> frame + log -> WS binary
```

Recommended bounds:

- input: 64 messages and 1 MiB;
- output: 256 messages and 1 MiB.

On input overflow, send a WS `error` with `code = "dap_input_overflow"` and
close with `4408`. On output overflow, fail the session with
`last_error = "dap_output_overflow"` and close with `4408`. The worker must
never block on a slow subscriber.

Recording:

- the runtime assigns one monotonic data sequence for both directions;
- output records are appended to the session log before/with delivery;
- input records are appended when the WebSocket receives them;
- the log stores direction and payload, so replay can interleave the
  conversation in order;
- a log write failure marks the session `replayable = false`; live debugging
  continues;
- existing session storage quotas and purge apply.

`session_io_ws` gets a dedicated DAP branch, or a separate
`dap_session_io_ws`:

- active session: exactly one write subscriber; output is framed as
  `seq + direction(0) + payload`; incoming binary is raw DAP payload, logged
  with direction 1, and sent to the worker;
- closed/failed session with `mode=read`: replay the recorded frames from
  `after`, including both directions, then send `session.ended`;
- text frames carry `hello`, lifecycle, and errors only;
- on active-session socket close, close the session immediately rather than
  scheduling the generic detach grace period.

DAP does not use `SessionRing`; the session log is the replay source. Lifecycle
updates still use the existing session state and broadcast channel.

Read-only live display subscribers are technically possible: one controller
plus N readers. They are not part of this pass because they reintroduce live
fanout, separate subscriber limits, slow-reader policy, and both-direction
delivery. The retained-log replay covers the “boot should display what
happened” use case without those moving parts.

### 7.4 Why a replay is display-only

DAP bytes are a conversation, not a resumable debugger stream. A recording
contains requests from one client, responses with request sequence numbers,
variable references, source paths, breakpoint state, memory contents, and
events that depend on the target's exact state. Feeding that transcript to a
new debugger would be invalid: the new client would see responses to requests
it never sent, and the target would already be in a different state.

It can also be unsafe. DAP includes `writeMemory`, flash/launch actions,
`continue`, and reset-like operations. Replaying those bytes into a debugger
could mutate the target unexpectedly.

The live debugger state — selected frame, variable caches, breakpoints, RTT
windows, core status — lives in the adapter and on the target. It is not
reconstructable from bytes, and probe-rs does not provide a snapshot format for
it. Server restart therefore marks the session failed rather than resuming it.

Boot records the data-plane frames in the session log. `replay-session` reads
them through `/io` and parses/renders them in the CLI. That is a display replay:
it never connects to a debugger and never sends recorded input back to hardware.

### 7.5 Detach and close

DAP never uses `schedule_close_if_idle`.

Close sequence:

1. set state `closing`;
2. cancel the worker token;
3. drop the input sender;
4. wait for the worker to report `ended`;
5. flush and close the session log writer and persist the final data metadata;
6. release the lease;
7. set state `closed`, persist, and keep metadata.

If the worker does not exit within a bounded grace period (5 seconds), mark the
session `failed` with `dap_shutdown_timeout`, release the lease, and log a
warning. This mirrors the failure mode of the existing worker threads.

A DAP client disconnect also closes the CLI bridge's TCP connection.

### 7.6 Recovery and management

- Startup recovery marks an unfinished DAP session `failed` with
  `server_restart`, keeps the recorded session log, and leaves `replayable`
  true if the log is readable. The session is not resumed.
- `POST /v1/sessions/{id}/resume` returns `not_resumable`.
- `PATCH /v1/sessions/{id}` rejects DAP policy changes with `invalid_request`.
  DAP is fixed at one live controller/writer and has no detach grace.
- `GET /v1/sessions/{id}/export` continues to return 409 for DAP.
- `replay-session` on a DAP session reads the logged data-plane frames through
  `/io?after=...` and renders them.
- `DELETE /v1/sessions/{id}?purge=true` removes the session log.

### 7.7 Data-plane recording

The session log is the DAP recording. There is no separate trace file or trace
level.

- `RECORD_OUTPUT` stores adapter-to-client payloads.
- `RECORD_INPUT` stores client-to-adapter payloads.
- the log record's channel is the direction (0 or 1).
- records use the session's monotonic data sequence, so replay preserves the
  conversation order.
- payloads are the raw DAP bytes after the transport frame header; the CLI
  runs the DAP `Content-Length` parser when rendering.

The session's `data` counters include both directions so cursors and
`replay-session --from` work. `replayable` is true while the log is usable. If
the log cannot be opened or a write fails, DAP sets `replayable = false`, keeps
the live debugger running, and logs the persistence failure. Existing session
storage quotas and purge apply.

Recording only adapter-to-client messages would lose `launch`,
`setBreakpoints`, `evaluate`, and other requests. Both directions are required
for a useful replay.

### 7.8 Replaying retained DAP sessions

There is no live observation stream. A DAP session has the same single data
subscriber as before. The recorded frames are read through `/io` after the
session reaches a terminal state.

`replay-session` on a DAP session:

1. loads the session metadata;
2. requires the session to be `closed` or `failed`;
3. connects to `GET /v1/sessions/{id}/io?mode=read&after=<seq>`;
4. receives both directions as framed binary records;
5. parses the DAP messages and renders them in the CLI.

If the session is still active, replay returns `session_active`. The operator
must wait for the debugger to disconnect, or for the server to mark the session
failed after a restart. A failed session's log is retained and readable.

`replayable` is true for DAP when the log is available. The session is still not
resumable: replay is display-only and never connects a debugger or sends
recorded input to hardware.

RTT and serial are unchanged: retained data replays through `/io?after=...`.

## 8. Protocol and API changes

### 8.1 Wire framing

DAP binary frames use boot session framing:

```text
seq:u64 (big-endian) | direction:u8 | payload
```

Direction `0` is adapter-to-client; direction `1` is client-to-adapter. The
payload is raw DAP bytes. The header is transport metadata, not part of DAP.

Update `boot-protocol`:

- `encode_session_frame("dap", seq, direction, payload)` emits the 9-byte
  header followed by the payload;
- `decode_session_frame("dap", bytes)` returns `seq`, `channel = Some(direction)`,
  and the payload;
- keep `session_frame_is_sequenced(kind)` so callers do not repeat string
  checks.

Live path:

- server-to-client frames carry direction 0;
- the CLI bridge strips the 9-byte header and writes the payload to the
  debugger's TCP stream;
- client-to-server frames are raw DAP bytes; the server assigns a sequence and
  direction 1 when recording.

Replay path:

- `/io?mode=read&after=...` on a closed/failed session sends both directions as
  framed binary records in sequence order;
- `replay-session` parses the frames and DAP `Content-Length` messages; it does
  not strip them into a debugger stream.

Text WebSocket frames remain for `hello`, lifecycle, and errors. The only
client that connects to `/io` for live DAP is the CLI bridge, so the framing
rule is contained there. Direct API clients must strip the header themselves.

No new protocol endpoint or request field is required. `Session.trace` and
`ProbeSessionRequest.trace` are not added; DAP replay uses the existing session
log and `/io`.

### 8.2 HTTP semantics

- `POST /v1/probes/{id}/sessions` with `kind = "dap"` creates a DAP session.
- `max_subscribers` must be 1 or the request is `400 invalid_request`.
- `GET /v1/sessions/{id}/io` attaches the single live write subscriber. A
  second live attach returns `409 dap_exclusive`.
- `GET /v1/sessions/{id}/io?mode=read&after=<seq>` on a `closed` or `failed`
  session replays the recorded frames. This is display-only; no hardware is
  opened. An active session with `mode=read` is rejected.
- `GET /v1/sessions/{id}` and `GET /v1/sessions` expose `replayable` and the
  data counters; there is no separate trace field.
- `PATCH` returns `400 invalid_request` for DAP policy changes.
- `POST /resume` returns `409 not_resumable`.
- `GET /export` returns `409` for DAP.
- `DELETE /v1/sessions/{id}?purge=true` removes the session log.

Existing error mapping already covers `dap_exclusive`; add `dap_input_overflow`
and `dap_output_overflow` as WebSocket error codes.

### 8.3 Authentication

DAP attach requires `control` at the HTTP layer, as it does today.

The session WebSocket must also revalidate the token periodically, as promised
by the authentication design. Add a 30-second revalidation timer to the session
socket loop. On failure, send a WS error and close with `4401`, then close the
DAP session and release the lease.

### 8.4 DAP replay through `/io`

DAP replay uses the existing session data plane instead of a separate endpoint:

- `GET /v1/sessions/{id}/io?mode=read&after=<seq>` on a `closed` or `failed`
  DAP session;
- binary frames are `seq + direction + payload`, with both directions included;
- the CLI parses the frames and DAP messages; it never forwards them to a
  debugger;
- `hello` reports the session snapshot, `replayable`, and the data cursor;
- the server sends `session.ended` after the replay;
- an active DAP session rejects `mode=read`; a second live attach still returns
  `dap_exclusive`;
- a purged or unreadable log has `replayable = false`; replay returns `hello`
  then `session.ended` with no records.

`after` is the last data sequence already rendered. `include_input` is ignored
for DAP because both directions are always included. There is no separate
`/trace` endpoint and no WebSocket observer mode.

The split between `/io` replay and live data is deliberate:

- `/io` already owns session cursors, sequencing, logging, retention, and
  replay for RTT/serial;
- DAP uses the same data-plane path, with framing that the CLI bridge strips
  for live debuggers;
- replay is display-only, so it can use the same `/io` read side without
  allowing a second debugger.

A generic `/replay` wrapper would add a branch without removing the underlying
path. The rule is: all session replay uses `/io`; DAP differs only in how the
CLI interprets and renders the frames.

## 9. CLI changes

### 9.1 Command and plan

`start-dap` becomes a real bridge:

```text
boot start-dap <server>/<probe> [--stdio | --port <port>]
                 [--elf <path> | --artifact <id>] [--inherit-artifact]
```

Without `--stdio`, `start-dap` listens on `127.0.0.1:<port>`; the port defaults
to `50000`. With `--stdio`, it speaks DAP over stdin/stdout and `--port` is not
allowed.

`--elf` uploads the file with the existing artifact endpoint and uses the
returned ID. `--artifact` selects an existing artifact. `--inherit-artifact`
allows the probe's last flashed artifact to be used only when the DAP client
does not supply a `program`. The default is no inheritance.

The plan should show the upload as its own task, then a `StartDapBridge` task:

```rust
enum DapBridgeTransport {
    Tcp { port: u16 },
    Stdio,
}

Action::StartDapBridge {
    server: Key<ServerConfig>,
    resource: Key<ResourceHandle>,
    transport: DapBridgeTransport,
    artifact: ArtifactRef,
}
```

Once the session is created — on accept in TCP mode, at startup in stdio mode —
`start-dap` writes the session ID and replay hint to stderr:

```text
DAP session ses_01J... started
Replay after disconnect: boot replay-session local/ses_01J...
```

A typical stdio launch configuration is:

```json
{
  "runtimeExecutable": "boot",
  "runtimeArgs": ["start-dap", "local/probe", "--stdio"]
}
```

Stdout is reserved for DAP bytes in stdio mode. UI status, progress, warnings,
and errors must go to stderr.

The existing `Action::AttachSession` DAP arm is removed; DAP is not an
interactive stdout stream.

### 9.2 Bridge lifecycle

1. Load the server and resolve the probe before opening a transport.
2. Open the transport:
   - **TCP**: bind `127.0.0.1:<port>` and print the address to stderr.
   - **stdio**: do not bind or listen; use process stdin/stdout.
3. Create the DAP session:
   - **TCP**: create a fresh session per accepted connection, so the probe is
     not held while idle.
   - **stdio**: create exactly one session at startup. The debugger launched
     this process, so there is no idle period to protect.
   Session parameters are the same in both modes: `acquire = create`,
   `max_subscribers = 1`, `detach_timeout_seconds = 0`,
   `inherit_artifact = false`, and the chosen artifact.
4. Attach `/io` in `write` mode and pump bytes until the client or session
   ends.
5. Close the session with `DELETE`. Closing keeps the session log for replay;
   only an explicit purge removes it.
6. In TCP mode continue accepting. In stdio mode exit after disconnect.

If session creation fails, TCP closes that client connection and continues
listening. Stdio prints the error to stderr and exits non-zero.

#### Session creation timing

In TCP mode, opening the DAP session immediately when `start-dap` runs has some
appeal:

- the lease is acquired before the debugger connects, so probe contention is
  reported immediately;
- the session ID and state are available for display before a client arrives;
- there is no session-creation latency at the moment the debugger connects.

The cost is that an idle `start-dap` holds the probe exclusively. It blocks RTT,
serial, flashing, and other debug sessions even when no debugger is connected.
It also creates an awkward lifetime: after the first client disconnects, the
bridge must either reuse a session that no longer has valid debugger state or
throw it away and create another one.

Creating on accept avoids that:

- an idle bridge does not hold the lease;
- the probe remains available for RTT, serial, or another debug session while
  the listener waits;
- every debugger connection gets fresh adapter state, matching DAP's
  non-resumable model;
- the only cost is a short session-creation step after the TCP connection
  arrives, which is small compared with the DAP initialize/launch handshake.

Use lazy creation on accept in TCP mode. Stdio mode is inherently one session
started under the debugger's process control, so it creates the session
immediately and exits after disconnect. If a future TCP workflow needs a
prewarmed session, add an explicit `--prewarm` option rather than making idle
lease ownership the default.

A new TCP connection creates a new DAP session because DAP is not resumable and
a previous debugger may have left state behind. Only one client is served at a
time; additional connections wait in the listener backlog.

On Ctrl+C, close the active session and exit cleanly. If the server reports
`session_active`, print a clear message and close that TCP connection.

### 9.3 Bridge data path

Add a DAP bridge helper in `boot-cli` that shares the WebSocket message and
lifecycle handling with the existing follow function but writes the stripped
DAP payload to an async TCP sink.

It must:

- use the session `hello` and lifecycle messages for status;
- decode the boot DAP frame header (`seq + direction`);
- accept only direction 0 on the live path and write the payload to the
  debugger stream;
- forward debugger bytes unchanged as WebSocket binary input;
- parse DAP `Content-Length` at least far enough to inspect `launch`/`attach`;
- forward all messages that do not need rewriting byte-for-byte;
- not reconnect automatically;
- return a clean outcome when the session ends or the debugger disconnects.

Launch rewriting is the one DAP-aware operation on the live path:

1. buffer the complete `launch` or `attach` request;
2. resolve `program`/`coreConfigs[].programBinary`, `svdFile`, and
   `chipDescriptionPath` relative to `cwd`;
3. upload those files through `POST /v1/artifacts`, showing progress on stderr;
4. replace uploaded paths with IDs in `bootArtifacts`;
5. preserve the request `seq`, command, and all other arguments;
6. forward the rewritten request to the server.

If a path cannot be read or any upload fails, synthesize a DAP error response
for that request, delete the artifacts uploaded for that request if they are
unreferenced, and do not start the session. If the request already has
`bootArtifacts` or an `art_...` value, leave that file alone and skip its
upload.

The bridge does not parse stack, variable, or breakpoint payloads. Everything
after launch/attach is a byte stream.

The bridge has two concurrent directions:

- debugger input -> WebSocket binary (raw DAP bytes);
- WebSocket binary -> frame decode -> debugger output.

In TCP mode the debugger stream is the TCP socket. In stdio mode it is process
stdin/stdout: stdout carries only DAP bytes, stdin is the DAP input, and the
normal interactive CLI input pump is not used.

If either direction ends, close the session. In TCP mode continue listening; in
stdio mode exit. Report errors to stderr; never mix DAP bytes with CLI status
output.

The replay path is different: `replay-session` decodes the same frames but
keeps both directions and feeds the renderer instead of TCP.

### 9.4 Remote paths and artifacts

The bridge is the only supported file-transfer path, for both local and remote
servers. It reads the launch program and optional SVD/chip-description paths,
uploads them as artifacts, and forwards `bootArtifacts` IDs in place of the
paths. The server never reads the original client paths. Boot does not implement
probe-rs's non-standard `remote_server_mode`; the bridge does not decode inline
remote-mode file payloads.

`--elf` and `--artifact` are explicit pre-session overrides. They win over the
launch configuration. `--inherit-artifact` is only used when the client did not
supply a program. See section 6.3 for the full precedence.

Uploaded artifacts are referenced by the DAP session when the adapter resolves
them. They cannot be deleted while the session holds the reference and become
eligible for normal artifact cleanup after the session closes. If the same file
is uploaded repeatedly, server-side SHA-256 deduplication can avoid duplicates
in a later pass; it is not required for correctness.

Source paths embedded in DWARF are not uploaded. The adapter returns them from
the program artifact and the debugger resolves them using its normal source
mapping.

The probe selector is always overridden server-side. The client's `probe` field
is advisory only.

### 9.5 Replaying a DAP session

There is no live follow command. Extend the existing `replay-session` command:

```text
boot replay-session <server>/<session-id> [--from <seq>]
```

For a DAP session it:

1. loads session metadata;
2. requires the session to be `closed` or `failed` (otherwise reports
   `session_active`);
3. connects to `GET /v1/sessions/{id}/io?mode=read&after=<seq>`;
4. decodes both directions, parses the DAP messages, and renders them.

For DAP, `--from` is a data sequence and `--include-input` is ignored because
both directions are always included. The renderer is compact and
event-oriented, for example:

```text
session ses_01J... started
launch chip=nRF52840_xxAA program=target/debug/firmware
breakpoints: 2 requested, 2 verified
stopped: breakpoint thread 1 at src/main.rs:42 in main
output stdout: hello from the target
continued thread 1
session ended: disconnected
```

The renderer parses the framed messages and correlates requests and responses
using DAP `seq`/`request_seq`. For example, it prints a `stopped` event, then
uses the following `stackTrace` response to show the top frame. It renders
`setBreakpoints` responses, `output` events, and thread/continue events without
needing any server-side event mapping.

RTT and serial sessions keep their existing `replay-session` behavior through
`/io?after=...`. `list-sessions` should show the session kind and
`replayable`, so users can discover what is replayable.

## 10. Security

- DAP is available only through an authenticated `control` session.
- The local bridge binds loopback only in TCP mode. Any local process that can
  connect can drive the probe for the lifetime of that client; this is the same
  local trust boundary as a debugger attaching to a local probe. Stdio mode has
  no listening socket; only the launching debugger process can drive it.
- Token revocation closes the WebSocket and the DAP session within the
  revalidation interval.
- DAP replay requires `control`. The session log is mode `0600` under a `0700`
  session directory and is removed on purge.
- The log contains both DAP directions, including source paths, launch
  configuration, target output, variable values, and memory contents. It is not
  exposed to `read` scope.
- DAP artifact uploads use the normal artifact API and the CLI's `control`
  token. Uploaded files are stored as artifacts, subject to the artifact size
  limit and session references.
- `bootArtifacts` IDs are resolved only through the server artifact store; a
  client cannot inject an arbitrary server path through them.
- The DAP data plane is written to the session log; the log is the replay
  source.
- A DAP session cannot escape the lease or select a different probe.
- Server-side artifact paths are opened only by the worker and only for
  artifacts referenced by the session.

## 11. Failure handling

| Failure | Result |
| --- | --- |
| Probe is leased by another change/session | `409 resource_busy` |
| DAP backend cannot start | Session `failed` with `dap_backend_unavailable` |
| DAP launch configuration is invalid | DAP error to client; session fails cleanly |
| launch-file upload fails | DAP error response; debugger session is not started |
| launch-file path cannot be read | DAP error response; debugger session is not started |
| `bootArtifacts` ID missing or expired | launch fails with a clear error |
| Client disconnects | Session closes, lease released |
| Stdio stdin/stdout closes | Session closes; bridge exits |
| Worker hangs during close | `failed` with `dap_shutdown_timeout` after grace |
| Input or output queue overflows | WS close `4408`; session fails or closes |
| Server restarts | Session `failed` with `server_restart`; log retained and replayable |
| Token revoked | WS close `4401`; session closes |
| DAP client requests another probe | Ignored; session probe remains authoritative |
| Session log write fails | `replayable = false`; live debugger continues |
| Log missing or purged | Replay returns no data and ends |
| Replay requested while session is active | Replay reports `session_active` |

## 12. Testing

Unit and integration tests must not require hardware.

Protocol and runtime tests:

- DAP frame encode/decode preserves sequence, direction, and payload;
- DAP create rejects `max_subscribers != 1`;
- DAP attach accepts one write subscriber and second live attach is
  `dap_exclusive`;
- active DAP `mode=read` is rejected; closed/failed DAP `mode=read` replays;
- DAP patch/resume/export are rejected; replay is allowed through `/io`;
- DAP session has a session log, no ring, and updates data counters and
  `replayable`;
- restore marks an unfinished DAP session failed, keeps the log, and leaves it
  replayable if readable;
- detach closes the session immediately and releases the lease;
- output overflow closes with `4408` without blocking the worker.

Native engine tests:

- codec handles messages split across chunks and multiple messages in one chunk;
- malformed/oversized headers are rejected without panicking;
- program precedence follows artifact, client program, opt-in inheritance, error;
- launch vs attach applies the correct program/flash behavior;
- REPL command parsing, completions, and halted-core checks;
- SVD parsing produces stable peripheral/register variable trees;
- peripheral value reads use target memory lazily and fail cleanly when absent;
- `bootArtifacts` IDs resolve through the artifact resolver, and missing IDs
  fail the launch with a clear error;
- resolving a DAP artifact registers a session reference;
- DAP output/event ordering is monotonic within a session.

API tests with a fake `DapBackend`:

- create, attach, send raw bytes, receive framed output;
- hello carries `replayable: true` and data counters;
- second live attach returns `dap_exclusive`;
- client close transitions to `closed`;
- token revocation closes the socket.

Recording and replay tests:

- output records use direction 0 and input records direction 1;
- records preserve conversation order with a monotonic data sequence;
- the log survives close and restore, and a failed session remains readable;
- replay sends both directions and honors `after`;
- replay on an active session reports `session_active`;
- a log write failure sets `replayable = false` and the live session continues;
- purge removes the session log and clears the data counters.

CLI tests:

- `start-dap` plan has an upload task when `--elf` is used and a bridge task;
- `--stdio` and `--port` are mutually exclusive; TCP defaults to port 50000;
- stdout in stdio mode contains only DAP frames; status and upload progress go
  to stderr;
- stdio mode serves one session and exits after disconnect;
- `--inherit-artifact` is passed only when requested;
- bridge binds loopback, accepts one client, and creates one session per
  connection;
- the live bridge strips the 9-byte DAP frame header and writes only payload to
  TCP;
- client input is forwarded as raw DAP bytes;
- launch/attach rewriting uploads `program`, `coreConfigs[].programBinary`,
  `svdFile`, and `chipDescriptionPath` and preserves `seq` and other arguments;
- relative paths resolve against the launch `cwd`;
- missing local paths fail with a DAP error; existing `bootArtifacts`/`art_...`
  values are left alone;
- upload failure returns a DAP error response and does not start the session;
- `replay-session` on a closed DAP session decodes both directions and renders
  lifecycle, stop, output, and continue events;
- the renderer correlates a `stackTrace` response to the preceding `stopped`
  event and renders the top frame;
- `replay-session` on an active DAP session reports `session_active`;
- RTT/serial `replay-session` still uses `/io?after=...`.

Manual hardware smoke test:

- start the server, flash a target, run `boot start-dap`, point VSCode at
  `127.0.0.1:50000`, hit a breakpoint, inspect variables, step, continue, and
  disconnect;
- repeat using `runtimeExecutable: boot` with `start-dap local/probe --stdio`;
  verify stdout carries only DAP and the process exits after disconnect;
- after disconnect, run `boot replay-session` and verify the replay shows the
  launch, breakpoints, stop, top frame, output, continue, and termination;
- verify the lease is released and a second debug session works;
- repeat with a remote server using normal local launch paths; verify the bridge
  uploads the program/SVD, the adapter resolves `bootArtifacts`, and debugging
  works without probe-rs remote mode;
- verify unplugging the probe fails the session cleanly and leaves a readable
  session log.

## 13. Code touchpoints

| Area | Files | Change |
| --- | --- | --- |
| Protocol | `boot-protocol/src/lib.rs` | DAP frame encode/decode with sequence + direction |
| Runtime | `boot-api/src/runtime/session.rs`, `boot-api/src/runtime/mod.rs` | DAP create/attach/close, worker wiring, channels, backend factory, bidirectional session logging/replay, artifact resolver and refs |
| Storage | `boot-api/src/runtime/session_log.rs` | Existing session log writer/reader; both directions for DAP |
| API adapter | `boot-api/src/run.rs` | DAP `/io` live/replay branch, frame/log handling, attach/error mapping, export guard, auth revalidation |
| Backend | new `boot-dap/` crate | Native DAP adapter over `probe-rs`/`probe-rs-debug`; `bootArtifacts` resolution; no vendored engine |
| CLI command | `boot-cli/src/command/probe.rs` | `--stdio`, `--elf`, `--artifact`, `--inherit-artifact` for `start-dap` |
| CLI plan | `boot-cli/src/workflow/mod.rs`, `boot-cli/src/plan.rs` | `StartDapBridge` transport + replay actions; remove the DAP `AttachSession` path |
| CLI executor | `boot-cli/src/execute.rs` | TCP listener/stdio pump, session lifecycle, frame stripping, launch-file upload/rewrite, DAP replay renderer |
| CLI replay | `boot-cli/src/follow/mod.rs`, new `boot-cli/src/dap_replay.rs` | Live frame stripping and framed-message renderer for retained sessions |
| CLI ports | `boot-cli/src/ports/mod.rs` | Upload launch files and pass artifact/replay parameters |
| CLI session views | `boot-cli/src/command/session.rs`, `boot-cli/src/ui/mod.rs` | `list-sessions` replayability and human DAP rendering |
| Dependencies | `Cargo.toml`, `boot-api/Cargo.toml` | Add optional `boot-dap`; remove unused `os_pipe`; move `probe-rs-debug` to the engine crate |
| Docs and licensing | `NOTICE`, `AGENTS.md`, `docs/*.md` | Architecture updates and command behavior; no vendored attribution required |

## 14. Documentation and release work

Update:

- this document's status and decision log;
- `docs/api-architecture-design.md` DAP section: engine, framed data plane,
  lifecycle, no resume, bidirectional session log, and `/io` replay;
- `docs/cli-architecture-design.md`: `start-dap` bridge, TCP/stdio
  transports, frame stripping, plan action, and DAP replay rendering;
- `docs/cli-command-structure.md`: remove "not implemented" from `start-dap`
  and document DAP `replay-session`;
- `docs/authentication.md`: replace the stale `/v1/probes/{selector}/dap`
  endpoint table entries, document DAP `/io` replay, and document session
  socket revalidation;
- `AGENTS.md`: add `boot-dap`, the DAP worker, and the DAP frame/log rule.

Release checks:

- `cargo test --workspace`;
- `cargo fmt --check`;
- `cargo deny check licenses`;
- the generated source offer still matches the binary.

## 15. Implementation phases

| Phase | Work | Exit criteria |
| --- | --- | --- |
| 1 | DAP frame encode/decode and CLI live strip helper | DAP frames carry sequence/direction; the proxy presents a normal DAP stream |
| 2 | Runtime DAP lifecycle with a fake backend | Exclusive attach, close-on-detach, bidirectional log, `/io` replay, recovery, errors |
| 3 | Native adapter core: codec, initialize, launch/attach, threads, stackTrace, scopes, variables, breakpoints, stepping, disconnect | VSCode can break, inspect, step, continue, and disconnect against a real probe |
| 4 | Complete native DAP surface: remaining requests, RTT, REPL, SVD/peripherals | Core requests plus the required REPL and SVD behavior work |
| 5 | `start-dap` TCP bridge, launch-file upload, `bootArtifacts` rewrite | Debugger connects, files upload, debug flow works, lease releases |
| 6 | Bidirectional DAP logging, `/io` replay, DAP `replay-session` | Retained sessions render lifecycle, stops, output, and errors |
| 7 | Artifact overrides/ref cleanup, stdio transport, auth revalidation | `--elf`/`--artifact`/`--inherit-artifact` work; stdout-safe stdio mode works |
| 8 | Docs, full test pass, manual hardware matrix | All checks green; native adapter only |

Phases 1 and 2 can land before the engine. Phase 3 is the first full native
adapter pass; later phases expand the request surface and transport options.

## 16. Open decisions

Approval of this document means approving the following defaults. Anything not
approved should be changed before phase 1.

1. **Native adapter only.** The boot-owned `boot-dap` adapter is the
   implementation. There is no vendored or subprocess fallback; gaps are fixed
   in the adapter.
2. **Bridge lifetime.** TCP mode remains listening after a client disconnects
   and creates a fresh DAP session per accepted connection; it does not prewarm
   an idle session. Stdio mode serves exactly one session and exits after
   disconnect.
3. **Artifact precedence.** Explicit `--elf`/`--artifact` wins, then a
   bridge-uploaded `bootArtifacts.program`, then the probe's last artifact only
   with `--inherit-artifact`, otherwise fail. The server never reads a
   client-side program path directly.
4. **Remote debugging.** The bridge uploads launch program/SVD/chip files and
   rewrites them to artifact IDs. Boot does not implement probe-rs
   `remote_server_mode`.
5. **Feature scope.** Core DAP debugging, REPL, and SVD/peripheral support are
   all required.
6. **Auth revalidation.** Session sockets revalidate tokens every 30 seconds.
7. **DAP data-plane framing.** DAP binary frames carry a sequence and
   direction. The CLI bridge strips the header for live debuggers; replay
   decodes both directions for rendering.
8. **Recording.** DAP records both data-plane directions in the existing
   session log. There is no separate trace file, trace level, or `/trace`
   endpoint. Replay is `control`-only and only for retained sessions.
9. **Replay only.** There is no live observation stream, `/observe` endpoint, or
   observe subscriber role. `replay-session` reads retained DAP frames through
   `/io`; RTT/serial use the same `/io?after=` path.
10. **Stdio transport.** `start-dap --stdio` serves one DAP session over
    stdin/stdout. Stdout is DAP-only; status and errors go to stderr. The
    process exits after disconnect.

## 17. Decision log

| Date | Decision |
| --- | --- |
| 2026-11-19 | Proposed in-process DAP support, raw DAP data plane, exclusive close-on-detach sessions, and a per-connection local TCP bridge. |
| 2026-11-20 | The boot-owned native adapter is the implementation; keep the `DapBackend` trait as the server boundary. |
| 2026-11-20 | Create the DAP session lazily when the debugger connects, not when `start-dap` starts listening. |
| 2026-11-21 | Add boot sequence/direction frames to the DAP data plane; the CLI strips them for the live debugger. |
| 2026-11-23 | No vendored-engine fallback. Gaps in the native adapter are fixed in the adapter. |
| 2026-11-23 | DAP has one live controller/writer. Read-only live display subscribers are out of scope; display is post-session replay through `/io`. |
| 2026-11-25 | The `start-dap` bridge uploads launch program/SVD/chip files as artifacts and rewrites path fields to `bootArtifacts` IDs. |
| 2026-11-25 | Do not implement probe-rs `remote_server_mode`; it is non-standard. |
| 2026-11-25 | Add `start-dap --stdio` as a single-session, stdout-only DAP transport alternative to TCP. |
| 2026-11-21 | Log both DAP directions in the existing session log and replay them through `/io` after the session ends. |
| 2026-11-21 | Remove the separate trace file, `/trace` endpoint, live observation, `follow-session`, and observer role. |
| 2026-11-20 | Require REPL and SVD/peripheral support; make artifact inheritance explicit via `--inherit-artifact`. |
