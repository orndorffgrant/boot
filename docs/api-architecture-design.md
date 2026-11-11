# boot API Internal Architecture — Design

Status: Draft, iteration 10  
Date: 2026-11-09  
Scope: `boot-api`, `boot-api-client`, `boot-cli`, proposed `boot-protocol` crate  
Related: [authentication.md](authentication.md), [release-and-update-design.md](release-and-update-design.md)

## 1. Summary

`boot-api` is currently a thin Dropshot adapter over `probe-rs` and
`serialport`. Each request opens hardware directly, performs one blocking
operation, and returns. WebSocket handlers own their own probe or serial session
for the lifetime of one socket. There is no shared resource model, no
concurrency control, no operation progress, and no way for a client to observe
state without making another request.

This design replaces that with three cooperating models:

1. **A resource model.** Probes and serial ports have stable IDs and a single
   owner for their lifetime.
2. **An actor runtime.** Each hardware resource is owned by exactly one actor.
   Long-lived work runs in supervised actors, not inside HTTP handlers.
3. **A change/session model inspired by snapd.**
   - Finite work such as flashing is a **change** with tasks, progress,
     cancellation, and a terminal result.
   - Streaming work such as RTT, serial, and DAP is a **session** with a lease,
     a lifecycle, buffering, and reconnect support.
   - A session has one producer and zero or more **subscribers**, so several
     clients can follow the same serial console or RTT stream without taking
     ownership of the hardware.

Clients follow a single change over `GET /v1/changes/{id}/follow` instead of
subscribing to a server-wide event stream. Session bytes (RTT, serial, DAP) use
per-session WebSocket connections, so change progress and hardware traffic never
block each other. A general event stream is deferred until a concrete need
appears.

Session metadata and session data are persisted. On restart, serial and RTT
sessions attempt to reattach to a still-present resource, and closed sessions
remain replayable. Change history is persisted for audit and inspection, but a
change that was running during a restart is marked interrupted, never resumed.

The wire surface stays `/v1`. There are no existing deployments, so the API
changes in place rather than introducing a v2 or compatibility adapters.

### 1.1 Proposed decisions at a glance

| # | Decision | Rationale |
| --- | --- | --- |
| D1 | Use a small in-house actor runtime over Tokio channels | Hardware types are `!Send` and block; a framework does not fit cleanly |
| D2 | One dedicated OS thread per open probe/serial resource | Isolates blocking hardware state and avoids blocking-pool starvation |
| D3 | Make flash/reset asynchronous changes with `202` | Progress, cancellation, idempotency, and disconnect survival |
| D4 | Make RTT/serial/DAP first-class sessions with leases and multi-client follow | Limits, reconnect, multi-client follow, and clean cleanup |
| D5 | Follow one change over `GET /v1/changes/{id}/follow` | Simpler than a general event stream; reconnect and replay are scoped to the change |
| D6 | Keep session data WebSockets separate from change-follow WebSockets | Prevents head-of-line blocking and couples backpressure correctly |
| D7 | Replace base64 flash bodies with streamed artifacts | Removes 33% overhead and the 10 MB body limit |
| D8 | Evolve `/v1` in place; no v2 | No deployments, and the resource-scoped v1 shape is the desired one |
| D9 | Persist session metadata and full session data; persist change history but never resume in-flight changes | Sessions can outlive a server restart; flashing cannot safely resume mid-operation |
| D10 | Add a `boot-protocol` crate for wire types | Server and client share typed WebSocket messages without depending on the runtime |
| D11 | Give each session one producer and many independent subscribers | Several clients can follow serial/RTT; slow followers do not block hardware or each other |
| D12 | Do not adopt Oxide's Steno saga library for change execution | Boot operations are single-resource, non-compensable, and not resumable; borrow the concepts, not the dependency |
| D13 | Do not make sessions personally owned | A session is a team resource: `created_by` is audit metadata, and `control`/`admin` govern management |
| D14 | Do not give sessions a `sharing` field | Session metadata is visible to `read`; data attach and management require `control`/`admin`. Read-only monitoring can become a dedicated scope later |
| D15 | Defer a general server event stream | Per-change and per-session sockets cover the workflows; a global stream can be added later without changing creation endpoints |
| D16 | Store session metadata and change history in the existing API SQLite DB; store session payloads in append-only segment files | Reuses the existing `boot.sqlite3`; keeps high-volume streams out of SQLite |
| D17 | Auto-resume serial and RTT sessions when their resource is still present; never auto-resume DAP | Serial/RTT can reattach to a running target; DAP debugger state cannot |
| D18 | Use one production `Store` struct with narrow capability traits | One pool/migration set without a single god trait; auth and runtime tests can fake only what they use |
| D19 | Keep the native segment log canonical and export asciicast v3 for compatibility | Asciicast is open and replayable, but loses sequence, gaps, channels, and binary payload semantics |
| D20 | Always allow multiple control subscribers to write serial | Simple and matches the trusted-operator model; `mode=read` is a client choice, writes are serialized per message |
| D21 | Cancel running changes at the next safe point when their token is revoked | Revocation should stop future work without force-killing a driver call |
| D22 | Persist serial input as well as output | Enables full replay/export; input may contain secrets, so retention and purge apply |
| D23 | Send task and change updates individually over change-follow | Avoid repeating full task arrays; `hello` provides the snapshot and `seq` provides replay |

All of these are proposals for iteration, not irreversible commitments. Section
20 records the resolved decisions and the remaining implementation defaults.

## 2. Why change

### 2.1 Current shape

```text
HTTP request ──► Dropshot handler ──► open probe/serial ──► blocking work ──► response
WebSocket upgrade ──► handler ──► open device ──► read/write loop ──► close
```

Everything a request needs is created inside the handler and dropped when the
handler returns. Authentication is already centralized through the `Auth<S>`
extractor; nothing else is.

### 2.2 Problems this design must solve

| # | Problem | Consequence |
| --- | --- | --- |
| P1 | No resource ownership or locking | Two requests can open the same probe or serial port concurrently and corrupt or fail each other's sessions. |
| P2 | Probe IDs are `"TODO"`; serial IDs are the last path component | IDs are unstable or ambiguous across reboots and USB re-enumeration. |
| P3 | Flash is one synchronous HTTP request with a base64 body | 10 MB body limit, 33% encoding overhead, no progress, no cancellation, no idempotency, client timeouts. |
| P4 | Session state lives only in the WebSocket closure | No listing, limits, multi-client follow, reconnect, or graceful cleanup. A dropped socket kills the session with no recovery. |
| P5 | Serial writes reopen the port for every message | The writer can race the reader, settings can diverge, and throughput is poor. |
| P6 | `GET .../target` attaches to the target | A read-looking request can halt or disturb a running device. |
| P7 | Each WebSocket handler duplicates auth revalidation, select loops, and cleanup | Revocation, keepalive, and shutdown behavior drift between RTT, serial, and DAP. |
| P8 | DAP is proxied through OS pipes and `spawn_blocking`, then aborted | Resource cleanup is unreliable and the backend is still unimplemented. |
| P9 | No way to follow a running operation | Clients poll or block; progress is invisible; multi-client state is unknowable. |
| P10 | No central limits or observability | One client can exhaust probes, sessions, or the blocking pool. |
| P11 | `probe-rs` and `serialport` types are not consistently `Send` | Handlers use ad hoc `spawn_blocking` and aborts, which can starve the blocking pool. |
| P12 | A live stream belongs to one WebSocket | A second engineer cannot follow the same serial console or RTT log; opening another handle conflicts with the first. |
| P13 | Sessions and their output disappear on restart | A running target loses its console/RTT session, and past output cannot be replayed. |

## 3. Goals and non-goals

### 3.1 Goals

- **Single ownership.** At most one actor mutates a given probe or serial port.
- **Stable identity.** Resource IDs survive re-enumeration, reboots, and CLI
  reconnects.
- **Async work.** Flashing and resetting return immediately with a change ID;
  progress, cancellation, and the final result are observable.
- **First-class streaming.** RTT, serial, and DAP have session IDs, leases,
  multi-client access, and a defined lifecycle.
- **Shared observation.** Several clients can follow the same serial console or
  RTT/debug stream. Following is independent: one slow or disconnected follower
  does not stop the producer or the other followers.
- **Push, not polling.** A client can follow a specific change over a WebSocket
  that reports progress, logs, and the terminal result. Session data has its own
  per-session WebSocket.
- **Uniform policy.** Auth, scopes, quotas, timeout, shutdown, and logging are
  applied in the runtime, not reimplemented per handler.
- **Testable without hardware.** Hardware access sits behind traits that can be
  replaced with deterministic fakes.
- **Preserve the CLI experience.** `probe-run`, `probe-attach`, `serial-shell`,
  and `start-probe-dap` keep their workflow; progress and reconnect improve.
- **Session recovery and replay.** A serial/RTT session survives a restart when
  its resource is still present; closed sessions remain replayable.
- **Incremental delivery.** Each migration phase is shippable on its own.

### 3.2 Non-goals

- Multi-user accounts, an identity provider, or per-resource ACLs beyond the
  existing `read`/`control`/`admin` scopes.
- A browser UI. A future browser client needs a ticket or cookie-based
  WebSocket auth design.
- Distributed operation or multiple servers coordinating over one resource.
- Resuming an in-flight change or a DAP session after a server restart.
- Guaranteeing zero data loss during a server restart; hardware downtime produces
  an explicit gap.
- Completing the DAP backend as part of this architecture change.
- Replacing SQLite or the existing token model.
- Adopting a general-purpose actor framework.
- Prometheus/OpenTelemetry integration in the first implementation. Metrics are
  exposed in the server status response; an exporter can come later.

## 4. Influences

### 4.1 snapd

snapd's REST API is the main product-level influence. Its important ideas:

| snapd idea | What we borrow | What we change |
| --- | --- | --- |
| Long-running operations are changes with an ID | `POST /v1/probes/{id}/flash` returns `202 Accepted` with a change ID and `Location` | We use standard HTTP status and `Location` instead of snapd's `{type: "async"}` envelope |
| Changes contain tasks with status, progress, timing, logs, and data | Same change/task shape, adapted for flash stages | We use lowercase stable machine values |
| `Do → Doing → Done`, plus `Wait`, `Error`, `Abort` | `queued → waiting → doing → done`, plus `cancelling`, `cancelled`, `failed` | No `Undo`/`Hold` initially; firmware rollback is out of scope |
| `Wait` means an external event is required | `waiting`, with `data.waiting_for` naming the lease or device | Same concept |
| `POST /v1/changes/{id}` with `{"action":"abort"}` | Same abort semantics | Idempotent for terminal changes |
| `GET /v1/changes?select=all\|in-progress\|ready` | Same select filters | Ownership filtering added |
| `GET /v1/system-info` | `GET /v1/server` for version, limits, and counts | Kept small |
| Notices plus optional long-poll `timeout` | Nothing; this is the polling path we replace | `GET /v1/changes/{id}/follow` pushes updates for one change; no global notice stream |
| Single daemon owns authoritative state | Runtime owns resources, changes, and sessions | Same; change history and session data are persisted, execution is not resumable |

snapd does not use WebSockets for change notification in its current documented
API; it exposes change state through `GET` and supports long-polling for notices.
This design keeps the change/task model but makes notification push-based.

### 4.2 Actor model

We borrow the classical actor ideas that fit a hardware daemon:

- **Isolation over synchronization.** An actor owns its state; other components
  send messages. No `Arc<Mutex<Hardware>>` shared across handlers.
- **Single ownership of a resource.** The actor that owns a probe is the only
  code that touches that probe.
- **Mailboxes and handles.** A handle is a cloneable channel; the actor task
  owns the receiver and state.
- **Bounded mailboxes and backpressure.** Unbounded queues are a bug by default.
- **Supervision.** Top-level actors are monitored and restarted with backoff.
  Child work actors fail independently and report to their owner/client.
- **Let it crash, selectively.** Control-plane actors can be restarted because
  their state is reconstructible. In-flight changes and sessions are terminal
  when their actor dies; they are not silently restarted.
- **No actor cycles through bounded mailboxes.** Cross-actor data flow uses
  pull-based rings or separate directional channels to avoid deadlock.

We do **not** adopt location transparency, distributed actors, or a framework.
The runtime is a small internal module over Tokio channels. See
[Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/) for the basic handle
and mailbox pattern, including its warnings about bounded-channel cycles.

### 4.3 Why not an actor framework?

`probe-rs` session types are not reliably `Send`, and hardware calls block. The
critical actor implementation is therefore a dedicated OS thread per open
resource, which most Rust actor frameworks do not model cleanly. A small
in-house runtime gives us:

- explicit thread actors for `!Send` hardware state;
- explicit async actors for control-plane state;
- no framework migration risk;
- no dependency on a framework's supervision or mailbox semantics.

If the runtime grows beyond a few hundred lines of primitives, revisit this
decision.

### 4.4 Oxide steno (distributed sagas)

Oxide's [steno](https://github.com/oxidecomputer/steno) is a prototype
distributed-saga executor and the library behind the "sagas" abstraction in
Oxide's control-plane API. A saga is a DAG of idempotent **actions**, each with
an optional **undo action**. Execution is coordinated by a saga execution
coordinator (SEC), recorded in a persistent saga log through a `SecStore`, and
can be recovered after a crash. In Omicron, each Nexus instance is an SEC and
CockroachDB stores the log. [RFD 289](https://rfd.shared.oxide.computer/rfd/0289)
describes the operational consequences: in-flight sagas are tied to a code
version, are drained during upgrades, and are not normally recovered by a
different version.

The ideas worth borrowing:

| Idea | Relevance to boot |
| --- | --- |
| Named actions with explicit dependencies | Useful if a future change has parallel or conditional stages |
| Per-action status, progress, and logs | Already reflected in the change/task model |
| Idempotent actions | Required for safe retries of create/upload/abort requests |
| Immutable action outputs | A useful discipline for change `data` |
| Explicit failure and unwind policy | We keep the policy simple: fail the change, release the lease, never auto-undo hardware |
| Durable saga log | We persist change history, not resumable execution state; a restart is terminal |

We should **not** adopt steno as a dependency for this design:

- **Boot changes are single-resource and mostly linear.** Flash is a pipeline on
  one probe, not a transaction across multiple services. The actor + change state
  machine already gives us sequencing, progress, and cancellation.
- **Undo actions do not map to hardware.** A failed flash cannot be atomically
  undone. Reflashing a known-good artifact is an explicit recovery operation,
  not a compensating action that the framework should run automatically.
- **Steno's execution model conflicts with ours.** We persist change history and
  treat a restart as terminal; we do not try to resume actions. Adopting steno
  would bring its saga log, action-output versioning, upgrade draining, and
  orphan-saga problems with it.
- **Abort is still a future feature in steno.** Boot needs cooperative abort now.
- **The crate is explicitly prototype-quality.** It is impressively used in
  Omicron, but its README calls out limited automated testing and unresolved
  design questions. It is not a drop-in fit for a small daemon.

Revisit this decision if one of these becomes real:

1. a provisioning workflow spans multiple hosts or services and needs
   compensating actions;
2. changes must survive a process restart and resume execution;
3. a change becomes a DAG of parallel hardware operations rather than a linear
   pipeline.

Until then, use the vocabulary and status discipline, not the library.

## 5. Core model

### 5.1 Concepts

| Concept | Definition | Owned by | Lifetime |
| --- | --- | --- | --- |
| **Resource** | A physical probe or serial port discovered by the server | Inventory actor / device worker | Stable ID; generation changes on re-enumeration |
| **Lease** | Exclusive permission to use a resource for a change or session | Lease manager | Acquired and released; RAII guard |
| **Change** | Finite, cancellable work such as flash, reset, or inspect | Change manager + change actor | Until terminal, then retained briefly |
| **Task** | One stage of a change with its own status/progress/log | Change actor | Same as the change |
| **Session** | Long-lived streaming access to a resource: RTT, serial, or DAP. One producer/lease, zero or more data subscribers | Session manager + session actor | Serial/RTT metadata/data retained for replay; DAP metadata only |
| **Subscriber** | One client attached to a session's data plane, with its own cursor, principal, and read/write mode | Session actor / subscriber actor | Until its socket closes or it is kicked |
| **Artifact** | An uploaded firmware file, identified by ID and SHA-256 | Artifact store | TTL or explicit delete |
| **Change update** | A snapshot or log entry pushed to the followers of one change | Change actor | Per-change bounded replay history |
| **Principal** | The token identity performing an operation | Auth store | Token lifetime |

### 5.2 Identity

All IDs are opaque, URL-safe, and prefixed so logs and errors are readable.

| ID | Format | Stability |
| --- | --- | --- |
| Resource | `probe_<16 hex>` / `port_<16 hex>` | Stable hash of USB identity; fallback identity documented below |
| Change | `chg_<ULID>` | Unique, time-sortable |
| Task | `tsk_<ULID>` | Unique within a change |
| Session | `ses_<ULID>` | Unique, time-sortable |
| Subscriber | `sub_<ULID>` | Unique per attach; clients resume by data cursor, not by subscriber ID |
| Artifact | `art_<ULID>` | Unique, time-sortable |

**Probe identity.** Hash of `(vendor_id, product_id, serial_number, interface)`
when a serial number exists. Without a serial number, use
`(vendor_id, product_id, bus, address, interface)` and mark the resource
`identity_stable: false` in the inventory. The server logs a warning and the CLI
can display it. The legacy `DebugProbeSelector` string is still accepted as a
selector alias, but it is never the primary ID.

**Serial identity.** Hash of `(vendor_id, product_id, serial_number, interface)`
when USB metadata exists; otherwise hash of the normalized device path. The
display path (`/dev/ttyACM0`) is metadata, not identity.

**Generation.** Every resource also has a `generation: u64`, incremented each
time the physical device disappears and reappears. Handles carry the generation.
A request against an old generation fails with `410 Gone` and
`code: "resource_replaced"` instead of silently operating on a different device.

### 5.3 State machines

**Resource worker**

```text
missing ──► present ──► opening ──► ready ──► busy
               ▲            │          ▲         │
               │            ▼          │         ▼
               └──────── faulted ◄─────┴───── closing
```

- `present`: enumerated, not open.
- `opening`: worker thread is opening/attaching.
- `ready`: open and idle; can be leased.
- `busy`: leased to a change or session.
- `faulted`: the worker or hardware failed; backoff before retrying.
- `missing`: no longer enumerated; any lease/session is revoked.

**Change**

```text
queued ──► waiting ──► doing ──► done
   │          │          │
   │          │          ├──► cancelling ──► cancelled
   │          │          └──► failed
   └──────────┴──────────────► failed/cancelled
```

- `queued`: accepted; change actor is starting.
- `waiting`: blocked on a lease, a device to reappear, or an external event.
- `doing`: actively running a task.
- `done`: terminal success.
- `failed`: terminal error.
- `cancelling`: cancellation requested; waiting for a safe point.
- `cancelled`: terminal cancellation.

**Session**

```text
creating ──► starting ──► active ──► closing ──► closed
               │             │  ▲
               │             ▼  │
               │         suspended ──► resuming
               │             │
               └─────────────┴──► failed
```

- `creating`: lease is being acquired.
- `starting`: hardware is being opened, attached, or scanned for RTT.
- `active`: hardware is streaming. `subscribers` may be zero; when it is zero,
  a detach deadline is running.
- `suspended`: persisted but not currently attached to hardware. Common causes are
  a server restart, a missing device, or a failed reattach. The session's data is
  replayable, and it can be resumed manually or by recovery.
- `resuming`: recovery is reacquiring the lease and reopening the hardware.
- `closing`/`closed`: explicit close, last-subscriber grace expiry, or lifetime
  limit. Closed sessions retain their data for replay.
- `failed`: terminal hardware/startup failure. Data is retained for replay.

A session is not "attached" as a whole. Each subscriber has its own attach
lifetime and cursor; the session remains `active` while at least one subscriber
exists, or while the zero-subscriber grace timer has not expired. A `suspended`
or `closed` session can still serve replay subscribers.

### 5.4 Invariants

1. A resource has at most one lease and therefore at most one active change or
   session at a time.
2. Only a device worker thread touches hardware state. No handler holds a
   `probe-rs` session or serial handle.
3. Every finite hardware operation has a change ID; every streaming operation
   has a session ID.
4. A session has exactly one producer but any number of subscribers. A
   subscriber's cursor, slowness, or disconnect never affects the producer or
   another subscriber.
5. Data-plane bytes never pass through change-follow sockets.
6. Every queue is bounded and has an explicit overflow policy.
7. No blocking hardware call runs on a Tokio worker thread.
8. Authentication and scope checks happen before a lease is acquired and on
   every WebSocket command or attach.
9. Change-follow and session sockets apply principal/scope checks before sending
   data.
10. Leases are released on every exit path, including panic and client
    disconnect, through an RAII guard and an idempotent release message. A
    session's lease is held until the session closes, not until one subscriber
    leaves.
11. Resource IDs are opaque; no client can supply a filesystem path as a
    resource identifier.
12. Session data is appended to a durable log with a monotonic sequence number.
    The in-memory ring is only the live tail; replay can read older retained
    chunks from disk.
13. A change is never resumed after a server restart. It is marked failed with
    `server_restart`, and its history remains inspectable.

## 6. Architecture

### 6.1 Layering

```text
                    CLI / other clients
             HTTP commands        WebSocket change/data
                    │                     │
                    ▼                     ▼
┌──────────────────────────────────────────────────────────┐
│ boot-api adapter                                         │
│  Dropshot HTTP handlers      WebSocket connection actors  │
│  Auth<S> extractor           protocol encode/decode       │
└───────────────────────────┬──────────────────────────────┘
                            │ RuntimeHandle (cloneable)
┌───────────────────────────▼──────────────────────────────┐
│ runtime                                                  │
│  ChangeManager  SessionManager  LeaseManager  Supervisor │
│  ArtifactStore  InventoryManager  RecoveryManager         │
└───────────────┬─────────────────────────────┬────────────┘
                │ commands                    │ updates / data
┌───────────────▼──────────────┐  ┌───────────▼────────────┐
│ ProbeWorker (OS thread)      │  │ SerialWorker (OS thread)│
│ owns probe-rs session state  │  │ owns serialport handle  │
└───────────────┬──────────────┘  └───────────┬────────────┘
                ▼                             ▼
             USB probe                    serial device

                    boot.sqlite3 + session log files
```

The **adapter layer** is replaceable. It knows Dropshot, HTTP, and WebSocket
framing. It calls runtime handles and never touches hardware.

The **runtime layer** owns policy: leases, changes, sessions, followers,
supervision, quotas, and shutdown. It does not know Dropshot.

The **hardware layer** owns `probe-rs`, `serialport`, and DAP integration. It is
only reachable through device worker handles.

### 6.2 Actor inventory

| Actor | Runtime | Owns | Spawn policy | Failure policy |
| --- | --- | --- | --- | --- |
| `Supervisor` | Async | Join handles, restart budgets | At startup | Fatal; the process exits if it cannot continue |
| `InventoryManager` + watcher | Async + thread | Resource catalog, enumeration thread | At startup | Permanent; restart re-enumerates |
| `LeaseManager` | Async | Lease table, wait queues | At startup | Permanent; restart fails active leases |
| `ChangeManager` | Async | Persisted change history, retention, idempotency keys | At startup | Permanent; reloads history and fails in-flight changes |
| `ChangeActor` | Async | One change, tasks, and follower broadcast | Per change | Temporary; followers see `failed` |
| `ChangeFollowerActor` | Async | One change-follow socket and cursor | Per follow | Temporary; only that follower closes |
| `SessionManager` | Async | Persisted session catalog, quotas, access policy, recovery | At startup | Permanent; reloads sessions and re-runs recovery |
| `SessionActor` | Async | One session's state, live ring, subscriber cursors, durable log writer | Per session | Temporary; all subscriber sockets close, lease released |
| `SessionLogWriter` | Async + file I/O | Append-only segment writes and manifest updates | Per session | Transient; session reports `persistence_degraded` |
| `RecoveryManager` | Async | Startup resume attempts and manual resume requests | At startup | Permanent; retries only `suspended` sessions |
| `SubscriberActor` | Async | One data-plane socket, cursor, read/write mode | Per attach | Temporary; only that subscriber is removed |
| `ProbeWorker` | Thread | One probe's `probe-rs` state | On demand, per open resource | Transient; fault resource, close children |
| `SerialWorker` | Thread | One serial port handle | On demand, per open resource | Transient; fault resource, close children |
| `DapWorker` | Thread | One DAP backend session | On demand | Transient; close session |
| `ArtifactStore` | Async + reaper | Uploaded files and metadata | At startup | Permanent; restart scans the spool directory |

### 6.3 Async actors and thread actors

**Async actors** are Tokio tasks. They use bounded `tokio::sync::mpsc` mailboxes
and `oneshot` replies. They are used for everything that is I/O-bound but
non-blocking: catalogs, leases, changes, sessions, followers, and sockets.

**Thread actors** are dedicated OS threads. They are used when state is `!Send`
or when a call blocks for a long time:

- one thread per open probe worker;
- one thread per open serial worker;
- one inventory enumeration thread;
- optionally one thread per DAP backend.

A thread actor uses:

- `tokio::sync::mpsc::Receiver::blocking_recv()` for commands;
- `oneshot::Sender::send()` for replies (non-blocking);
- `mpsc::Sender::blocking_send()` for data chunks where producer
  backpressure is appropriate (for example, bounded write input). It is never
  used to make the producer wait for a slow subscriber;
- an `AtomicBool` or `CancellationToken` for cancellation checks between
  blocking calls;
- `std::thread` names such as `boot-probe-<id>` and `boot-serial-<id>`.

The thread actor never calls `tokio::spawn`, never awaits a timer, and never runs
on the Tokio blocking pool. This avoids blocking-pool starvation and gives each
device a deterministic lifecycle. Worker stacks default to 2 MiB and are
configurable if a target backend needs more.

### 6.4 Handles, mailboxes, and replies

Sketch:

```rust
pub struct Handle<C> {
    tx: mpsc::Sender<C>,
}

pub struct Command<C, R> {
    pub request_id: RequestId,
    pub cancel: CancellationToken,
    pub reply: oneshot::Sender<Result<R, RuntimeError>>,
    pub payload: C,
}
```

Rules:

- Mailboxes are bounded (control messages: 32; connection output: 256; data
  chunks: by bytes).
- A sender awaits capacity with a timeout. On timeout the adapter returns `503`
  or closes the socket with `4408`.
- Actor handles are cloneable and safe to store in `ApiState`.
- Replies use `oneshot`; a dropped receiver means the caller went away, which is
  normal, not an error.
- Every command carries a `RequestId` used in logs and tracing spans.
- Actors never call back into an actor that may be waiting on them through a
  bounded mailbox. Data flows use pull-based rings or separate directional
  channels.

### 6.5 Supervision and restart

The supervisor owns a restart budget per critical actor: exponential backoff
starting at 1 second, capped at 30 seconds, with at most five restarts per
minute before the runtime enters `degraded`.

| Actor class | Restart strategy |
| --- | --- |
| LeaseManager | Permanent. Restart loses the lease table; active sessions are suspended and recovered; in-flight changes are failed. |
| ChangeManager | Permanent. Restart reloads persisted history; in-flight changes are failed with `server_restart`. |
| SessionManager | Permanent. Restart reloads persisted sessions, closes data sockets, and re-runs recovery for suspended sessions. |
| RecoveryManager | Permanent. Restart resumes recovery attempts; sessions stay `suspended` until a lease is available. |
| ProbeWorker, SerialWorker | Transient. Restart only after an abnormal exit; active children are failed first. A new worker opens the device lazily. |
| SessionLogWriter | Transient. On failure the session enters `persistence_degraded`; live data may continue with a gap, or the session suspends. |
| ChangeActor, ChangeFollowerActor, SessionActor, SubscriberActor | Temporary. Never restarted. The client observes a terminal failure and may retry. |

A hardware thread panic is caught by the thread wrapper, logged with the resource
ID, reported to the supervisor, and converted into a `faulted` resource state.
Any lease or session on that resource is revoked. The worker may be respawned
after backoff, but active work is not transparently resumed.

### 6.6 Shutdown

On `SIGTERM`/`SIGINT` or a systemd stop:

1. Mark the runtime `draining`; `/v1/health` returns `503` with
   `status: "draining"`, and new changes/sessions get `503`.
2. Stop the inventory watcher.
3. Give in-flight changes a grace period to finish (default 10 seconds).
4. Cancel remaining changes at a safe point and persist them as failed with
   `server_shutdown`.
5. Mark live sessions `suspended` with reason `server_shutdown`, flush their
   durable logs, and close change-follow and session sockets with `1012`.
6. Release all leases and stop device workers. Join threads with a timeout.
7. Exit; if a hardware thread is stuck in a driver call after the timeout, log
   it and let the process exit reclaim the thread. Recovery on the next start
   handles the suspended sessions.

### 6.7 Backpressure rules

1. No unbounded queue. Per-change follower history and session rings are bounded
   with explicit eviction.
2. Control mailboxes are small and bounded; overload is a `503`, not memory
   growth.
3. Change followers get a bounded connection queue. If it overflows, the server
   closes with `4408 SlowConsumer`; the client reconnects with its last sequence
   and replays or receives a fresh snapshot.
4. Session data uses one bounded live ring per session plus the durable log, with
   a pull-based writer per subscriber. Each subscriber has an independent cursor.
   A slow subscriber never throttles the hardware producer or the other
   subscribers: it reads from the ring, then the log, and only gets a `gap` (or
   `4409` under `replay=strict`) when its cursor is outside retention. The
   producer always reads at hardware speed.
5. Change progress is coalesced to at most 10 updates per second; the terminal
   state is always emitted.

### 6.8 Proposed code layout

```text
boot-protocol/src/                 # serde + schemars only; no tokio, no dropshot
  lib.rs
  ids.rs                           # ResourceId, ChangeId, SessionId, ...
  resource.rs                      # probe/serial inventory DTOs
  change.rs                        # Change, Task, progress, statuses
  session.rs                       # Session, subscriber, options, state
  change_ws.rs                     # change-follow messages
  session_ws.rs                    # data-plane messages
  error.rs                         # stable error codes and bodies
  ws.rs                            # shared WebSocket envelope helpers

boot-api/src/
  lib.rs                           # BootApi trait; re-export protocol DTOs
  openapi.rs                       # security scheme and spec post-processing
  run.rs                           # bootstrap: runtime, shared DB, Dropshot
  auth.rs                          # existing tokens, scopes, Auth<S> extractor
  api/
    mod.rs
    state.rs                       # ApiState { auth, runtime, config }
    http/
      mod.rs
      resources.rs
      changes.rs
      sessions.rs
      artifacts.rs
      tokens.rs
      server.rs
    ws/
      mod.rs
      change_follow.rs             # one change-follow connection
      session_io.rs                # one subscriber connection
      codec.rs                     # Dropshot WS <-> protocol messages
  runtime/                         # compiled only with the `run` feature
    mod.rs                         # Runtime, RuntimeHandle, RuntimeConfig
    actor.rs                       # handle, mailbox, reply, cancellation
    supervisor.rs
    lease.rs
    inventory.rs
    change.rs
    change_follow.rs               # follower registry + replay buffer
    session.rs                     # SessionActor: ring + subscriber registry
    session_log/                   # durable session data
      mod.rs                       # writer, reader, framing
      format.rs                    # record header + CRC
    store/                         # persistence
      mod.rs                       # Store struct + capability traits
      sqlite.rs                    # shared boot.sqlite3 pool
      sessions.rs
      changes.rs
    export.rs                      # asciicast v3 export
    recovery.rs                    # startup/manual resume
    subscriber.rs                  # SubscriberActor: cursor + socket writer
    artifact.rs
  hardware/                        # compiled only with the `run` feature
    mod.rs                         # ProbeOps, SerialOps, DapOps
    probe.rs                       # ProbeWorker thread actor
    serial.rs                      # SerialWorker thread actor
    dap.rs                         # DapWorker thread actor
    backends/
      probe_rs.rs
      serialport.rs
      dap.rs
      fake.rs                      # test-only backends
```

`boot-api-client/src/` keeps the generated `include!` code and adds a
hand-written `ws.rs` that uses `boot-protocol` for typed change-follow and
session-data frames. `boot-cli/src/follow.rs` owns reconnect/backoff and progress
rendering, so each command does not reimplement WebSocket handling.

The `boot-protocol` split matters because `boot-api-client` currently depends on
`boot-api` only at build time. Runtime WebSocket message types cannot come from
that build dependency. A small protocol crate gives both sides the same types
without pulling `probe-rs` or Dropshot into the client.

## 7. Inventory and hardware ownership

### 7.1 Inventory watcher

- One thread enumerates probes and serial ports every 1 second by default.
- It diffs by stable ID, updates the resource catalog, and bumps the generation
  when a device disappears and reappears.
- It never opens or attaches to a probe. `GET /v1/probes` is safe and
  cannot disturb a target.
- The serial filter remains USB-only by default, preserving today's behavior;
  a config option can include virtual ports.
- Enumeration errors do not kill the watcher; they mark the inventory
  `degraded: true` and are retried on the next tick.
- A future Linux `udev` watcher can replace polling behind the same inventory
  interface. Polling is the first implementation because it is portable and
  simple.
- There is no inventory push stream initially. Clients re-list when they need a
  fresh view; a global event stream is deferred (section 11.6).

### 7.2 Probe worker lifecycle

Workers are created lazily:

1. A change or session acquires a lease on a `present` probe.
2. The inventory manager spawns a `ProbeWorker` thread with the probe's selector
   and generation.
3. The worker opens the probe and attaches the target when the operation needs
   it.
4. On completion, the worker stays idle for `idle_resource_ttl` (default 60
   seconds), then closes the probe and exits. The catalog keeps the resource
   entry.
5. If the probe disappears, the worker is closed immediately and the generation
   increments when it returns.

This keeps thread count proportional to active resources, not to all discovered
resources.

### 7.3 Serial worker lifecycle

- Open on demand when a serial session is created.
- One handle is used for both reads and writes; the current "reopen for each
  write" behavior is removed.
- Reads go to the session ring at hardware speed; subscribers pull
  independently. Writes arrive through a bounded input channel.
- Port settings come from the session options and are applied at open. Changing
  settings requires a new session.
- Close when the session closes or the last-subscriber grace expires.
- If the device disappears, close the session with `session.resource_gone`.

### 7.4 DAP

DAP is modeled as a session kind with a raw binary data plane. It is exclusive:
exactly one `control` subscriber is allowed. It does not support resume; on
detach the session closes and the lease is released. A second attach gets
`409 dap_exclusive`. The backend lives in a `DapWorker` thread that owns the
`probe-rs-debug` state and forwards opaque bytes. This removes the current
OS-pipe/`spawn_blocking` proxy.

DAP support is an implementation phase, not a prerequisite for the runtime.

### 7.5 Hardware abstraction

Hardware access sits behind traits implemented by real and fake backends. The
traits are used only inside a worker thread, so they do not need `Send`.

```rust
trait ProbeOps {
    fn list(&mut self) -> Result<Vec<ProbeInfo>, HardwareError>;
    fn open(&mut self, selector: &Selector) -> Result<(), HardwareError>;
    fn attach(&mut self, target: TargetSelector) -> Result<TargetInfo, HardwareError>;
    fn flash(&mut self, artifact: &Path, options: &FlashOptions,
             progress: &ProgressSink, cancel: &CancelFlag) -> Result<(), HardwareError>;
    fn reset(&mut self) -> Result<(), HardwareError>;
    fn attach_rtt(&mut self, options: &RttOptions) -> Result<RttHandle, HardwareError>;
    fn read_rtt(&mut self, buffer: &mut [u8]) -> Result<usize, HardwareError>;
    fn start_dap(&mut self, io: DapIo) -> Result<(), HardwareError>;
}
```

`FakeProbeOps` and `FakeSerialOps` drive the same actor code with scripted
results, delays, cancellations, and failures. Integration tests use the fake
backends through the real HTTP/WebSocket adapters.

## 8. Leases

### 8.1 Semantics

A lease is exclusive permission to use one resource. It carries:

```text
lease_id, resource_id, generation, mode (exclusive),
requester (principal id), holder (change id or session id),
acquired_at, purpose
```

There is no shared/read lease initially. Subscribers do not acquire leases; the
session holds the single exclusive lease while it is active. A future
monitor-only *session* could add a shared lease, but hardware access would still
serialize inside the worker.

### 8.2 Acquisition

- A `ChangeActor` requests a lease when it reaches the lease-acquisition step.
  If the resource is busy, the change enters `waiting` and joins a FIFO queue.
  The change request itself still returns `202`; waiting is visible through
  change-follow updates.
- A session-create request requests a lease with `wait_ms` (default `5000`,
  maximum `30000`). If the lease is not available within the timeout, the
  response is `409` with the current holder. A session is not created in a
  pending state; this keeps interactive semantics simple. A client that wants
  fail-fast passes `wait_ms=0`.
- Queue entries are cancelled when the change is aborted or the request is
  cancelled. Cancelled entries do not hold a position.
- There is no priority or preemption. An admin can abort the holder or close the
  holding session.
- Multi-resource operations are not needed today. If added, acquire in sorted
  resource-ID order to avoid deadlocks.

### 8.3 Release and failure

- `LeaseGuard` is an RAII object held by the change or session actor.
- Dropping it sends an idempotent `Release { lease_id }` via `try_send`. If the
  manager is gone, the runtime is already failing.
- If a device disappears, the lease manager revokes the lease and notifies the
  holder; the holder must stop within its cancellation policy.
- If the holder actor panics, the guard drop releases the lease. The supervisor
  also reconciles leases against live actors during restart.
- A lease is never persisted. A server restart clears all leases; persisted
  sessions are suspended and recovered, and in-flight changes are failed.

### 8.4 Conflict errors

```json
{
  "code": "resource_busy",
  "message": "probe is in use",
  "resource": "probe_1a2b3c4d5e6f7890",
  "holder": { "type": "session", "id": "ses_01J..." },
  "retry_after_seconds": 1
}
```

The holder's principal name/id is included only for admin callers.

## 9. Changes

### 9.1 Model

A change is finite work. Initial kinds:

| Kind | Purpose | Tasks |
| --- | --- | --- |
| `flash` | Write an artifact and optionally reset | `validate`, `erase`, `program`, `verify`, `reset` |
| `reset` | Reset the target | `reset` |
| `probe.inspect` | Attach and report target/core information | `attach`, `inspect` |

The exact flash tasks may be collapsed where `probe-rs` does not expose a stage
boundary. Stage-level progress is still useful even if byte-level progress is not
available.

Wire shape:

```json
{
  "id": "chg_01J9Z6...",
  "kind": "flash",
  "summary": "Flash firmware to probe_1a2b...",
  "owner": "9f2c1a7b4e8d0c31",
  "resource": "probe_1a2b3c4d5e6f7890",
  "status": "doing",
  "ready": false,
  "spawn_time": "2026-11-06T12:00:00.000Z",
  "ready_time": null,
  "tasks": [
    {
      "id": "tsk_01J9Z6...",
      "kind": "program",
      "summary": "Program 48 KiB",
      "status": "doing",
      "progress": { "done": 24576, "total": 49152, "unit": "bytes", "percent": 50.0 },
      "spawn_time": "2026-11-06T12:00:01.000Z",
      "ready_time": null
    }
  ],
  "error": null,
  "data": {},
  "log": []
}
```

Status values are stable lowercase strings. Clients must tolerate unknown values
so new states can be added without a protocol break.

### 9.2 Creation

Change creation endpoints are resource-scoped:

| Method | Path | Creates |
| --- | --- | --- |
| `POST` | `/v1/probes/{id}/flash` | flash change |
| `POST` | `/v1/probes/{id}/reset` | reset change |
| `POST` | `/v1/probes/{id}/inspect` | inspect change |

The resource ID in the path is authoritative. The request body contains only
options and an optional artifact reference; it never repeats the resource.

`POST /v1/probes/{id}/flash` validates:

1. The caller has `control`.
2. The resource exists, is not missing, and the generation matches if supplied.
3. The options are valid.
4. The artifact exists, belongs to the caller (or caller is admin), and is not
   expired.
5. The idempotency key is new, or maps to an existing change.

On success it creates the record, returns `202 Accepted`, a body containing the
change ID and initial status, and `Location: /v1/changes/<id>`. Then it spawns
the `ChangeActor`.

Validation failures are synchronous HTTP errors. Execution failures become
terminal change errors. If the client disconnects during an artifact upload, the
partial file is deleted. Once a change has been accepted, disconnecting does not
cancel it.

### 9.3 Execution

```text
create ──► queued ──► waiting ──► doing
                       │           ├─ task validate
                       │           ├─ task erase
                       │           ├─ task program
                       │           ├─ task verify
                       │           └─ task reset
                       │                │
                       └────────────────▼
                                      done
```

- Lease acquisition is a change state (`waiting`), not a task. The first
  hardware task starts only after the lease succeeds.

- The change actor transitions itself and each task, updates `progress`, appends
  log lines to a ring, and notifies `GET /v1/changes/{id}/follow` subscribers.
- Progress is throttled to 10 Hz. The final task/change state is always
  published.
- The flash artifact is a server-owned temporary file; `probe-rs` never receives
  a client-supplied path.
- If the device disappears or the lease is revoked, the change fails with
  `resource_gone` or `lease_revoked`.
- A change continues when the client disconnects. Only an explicit abort, an
  admin abort, a resource loss, or server shutdown stops it.

### 9.4 Cancellation and abort

- `POST /v1/changes/{id}` with `{"action":"abort"}` is allowed for the owner or
  an admin.
- The change enters `cancelling`, sets the cancellation flag, and asks the device
  worker to stop at the next safe point.
- If the backend cannot interrupt a driver call, the change stays `cancelling`
  until that call returns. The response includes `status: "cancelling"`.
- Aborting a terminal change returns the current change with `200` and does not
  change it. Abort is idempotent.
- The worker is not force-killed. After cancellation it closes/reopens the
  hardware as needed so the next lease starts cleanly.

### 9.5 Idempotency

The change-creation endpoints (`/flash`, `/reset`, `/inspect`) accept
`Idempotency-Key`.

- The key is scoped to the principal.
- The manager stores `(principal, key) -> (change_id, request_hash)` for 24
  hours or until the change is pruned.
- Repeating the request with the same hash returns the existing change (and does
  not start another).
- Reusing the key with a different body returns `409 idempotency_conflict`.
- Keys are never accepted for session creation initially; session creation is
  naturally retryable with the same options, and a duplicate session is caught
  by the resource lease.

### 9.6 Retention and restart

- Live changes are kept until terminal.
- Terminal changes are persisted and kept for 30 days or until 5,000 changes
  exist, whichever comes first. Both are configurable.
- Logs are a bounded ring (default 64 KiB or 1,000 lines) and are part of the
  persisted change record. When truncated, the change reports
  `log_truncated: true` and the oldest retained log sequence.
- Change history survives restart, but execution does not. Any `queued`,
  `waiting`, `doing`, or `cancelling` change is persisted as `failed` with
  `error.code = "server_restart"` on startup. A terminal `cancelled` or `done`
  change remains as-is. Its last known tasks, logs, and error remain visible.
- `GET /v1/changes/{id}/follow` can serve a terminal persisted change after a
  restart: it sends the snapshot and closes.
- A new flash after a restart is a new change. There is no automatic retry.

### 9.7 Firmware artifacts

The current base64 body is replaced by an upload:

1. `POST /v1/artifacts` with `Content-Type: application/octet-stream` streams the
   body to a temporary file in a `0700` spool directory.
2. The server enforces a maximum size (default 64 MiB), computes SHA-256, sets
   file mode `0600`, and returns `201` with `{id, size, sha256, expires_at}`.
3. `POST /v1/probes/{id}/flash` references the returned `artifact` ID.
4. The artifact is readable for a TTL (default 1 hour) and deleted by a reaper
   or on explicit `DELETE /v1/artifacts/{id}`.
5. The change validates the artifact and passes its path to the probe worker.
6. Startup removes stale files that are not referenced by a live change.

The artifact store never derives paths from client input. It maps IDs to
server-generated file names. The legacy `/v1` flash endpoint decodes base64 into
an artifact and then creates a change, so there is one ingestion path.

## 10. Sessions

### 10.1 Why sessions are separate from changes

Flashing is finite and has a result. RTT, serial, and DAP are streams that can
last minutes or hours, have bytes in both directions, and need reconnect and
backpressure. Modeling them as changes would force a change to stay open forever
and would blur abort semantics. They get their own resource type and lifecycle.

A session has **one producer** (the hardware lease and worker) and **one or more
subscribers** (attached data-plane clients). This is what lets several engineers
follow the same serial console or probe RTT output without fighting over the USB
device. Subscribers are cheap: one session ring and a cursor per subscriber, not
one hardware connection per client.

### 10.2 Creation and access

Session creation is resource-scoped:

| Method | Path | Body |
| --- | --- | --- |
| `POST` | `/v1/probes/{id}/sessions` | `{"kind":"rtt"\|"dap", "options": {...}}` |
| `POST` | `/v1/serial-ports/{id}/sessions` | `{"options":{"baud_rate":115200,...}}` |

The resource ID in the path is authoritative. Example:

```json
{
  "kind": "rtt",
  "options": { "rtt_addr": "0x20000000", "channels": [0] },
  "max_subscribers": 8,
  "detach_timeout_seconds": 60
}
```

Session options:

| Field | Default | Meaning |
| --- | --- | --- |
| `max_subscribers` | 8 | Per-session cap; server maximum 32. |
| `detach_timeout_seconds` | 60 | Grace period after the last subscriber leaves; maximum 300. |
| `wait_ms` | 5000 | How long to wait for a busy resource before `409`. |

There is no per-session owner, no `sharing` field, and no session-level
`write_access`. `created_by` records the principal for audit and display, but it
does not grant special rights. Access is scope-based:

- `read` may list session metadata, but cannot attach to the data plane.
- `control` may attach, change policy, kick a subscriber, or close the session.
- `admin` can do everything `control` can and inspect all sessions.

Any attached `control` subscriber may write to a serial session; writes are
serialized per message by the session actor, so multiple writers do not
interleave within one write. A subscriber can still choose `mode=read` to avoid
sending input itself.

Any `control` principal (or `admin`) can change `max_subscribers` and
`detach_timeout_seconds` later with `PATCH /v1/sessions/{id}`. Lowering
`max_subscribers` below the current subscriber count is rejected with `409`.
Kind, resource, and hardware options are immutable for the life of the session.
DAP rejects `max_subscribers > 1`.

Response `201 Created`:

```json
{
  "id": "ses_01J9Z6...",
  "kind": "rtt",
  "resource": "probe_1a2b3c4d5e6f7890",
  "created_by": "9f2c1a7b4e8d0c31",
  "state": "starting",
  "created_at": "2026-11-06T12:00:00.000Z",
  "subscribers": 0,
  "max_subscribers": 8,
  "detach_timeout_seconds": 60,
  "resume_count": 0,
  "replayable": true,
  "data": { "oldest_seq": 0, "latest_seq": 0, "buffered_bytes": 0, "bytes": 0 }
}
```

The response is sent once the lease is acquired. Hardware open/attach may still
be in progress; the session moves to `active` or `failed` and reports the state
on the `/io` socket. This avoids holding an HTTP request open for a slow RTT
scan.

`GET /v1/sessions/{id}/io` attaches a new subscriber. Multiple control clients
may attach to the same session unless the kind is DAP. The attach rules are in
section 10.6. Creation does not require the creator to attach: a control client
can create a session and other control clients can follow it immediately.

### 10.3 Subscribers, detach, and resume

Each successful `GET .../io` upgrade creates a subscriber with:

```text
subscriber_id, principal, mode (read|write),
cursor (data sequence), attached_at, remote address
```

`mode` is `write` for any attached `control` subscriber and `read` only if the
client requests `?mode=read`. `read` principals cannot attach at all. For RTT,
`write` mode has no practical effect because there is no input stream; for DAP,
the single control subscriber is bidirectional.

The first server text frame is a `hello` containing the session snapshot, the
subscriber ID, its mode, and the ring bounds:

```json
{
  "v": 1,
  "type": "hello",
  "protocol": 1,
  "subscriber_id": "sub_01J9Z6...",
  "mode": "read",
  "session": {},
  "data_seq": 123,
  "oldest_seq": 100,
  "latest_seq": 123
}
```

- Each subscriber reads from its own cursor. It can attach with
  `?after=<data_seq>` to replay from a prior point.
- If the cursor is inside the live ring, the server replays from the ring. If it
  is older than the ring but still inside the retained durable log, the server
  replays from disk and then switches to the live ring. If it is outside the
  retention window, the server sends a `gap` with the dropped range and continues
  from the oldest retained data. `?replay=strict` closes with `4409` instead.
- Subscriber sockets are independent. Closing one removes only that subscriber.
  It does not stop the producer, release the lease, or affect other followers.
- When the **last** subscriber leaves, the session enters a zero-subscriber grace
  period (`detach_timeout_seconds`). During that period data continues to be
  appended to the durable log and a client can reattach. When the grace expires,
  the session closes, the lease is released, and the worker may return to idle.
  The same timer starts when a session reaches `active` with zero subscribers,
  so a created-but-never-attached session does not hold hardware forever.
- The creator disconnecting while followers remain does **not** start the grace
  period. The session stays active because subscribers are still consuming it.
  `created_by` is audit metadata only; it grants no special rights and does not
  transfer.
- `DELETE /v1/sessions/{id}` closes the session for everyone immediately but
  retains its metadata and data for replay. Any `control` principal may do this;
  `admin` can too. It is idempotent for an already closed session (`204`), and
  `404` for an unknown ID. Use `DELETE /v1/sessions/{id}/data` to purge retained
  data, or `DELETE /v1/sessions/{id}?purge=true` to remove everything.
- `DELETE /v1/sessions/{id}/subscribers/{subscriber_id}` kicks one subscriber
  without closing the session. The socket closes with `4412`; the session and
  other subscribers are unaffected. Any `control` principal or admin may do
  this.
- If the server restarts, serial/RTT sessions attempt to reattach to the resource
  and continue; DAP sessions fail and are not resumable, though their metadata is
  retained. The WebSocket close code is `1012`, and clients reconnect using
  `?after=<last_seq>`. See section 14.

The grace period exists so a brief network glitch or CLI suspend does not kill a
debug session or lose console output. With multiple subscribers, it also means a
single follower's laptop sleeping does not terminate everyone else's stream.

### 10.4 Fanout, ring, and framing

The change-follow stream never carries session bytes. Each session has **one**
bounded ring of `Bytes` chunks and a monotonic `data_seq`. Subscribers store only
a cursor; they do not each get a hardware buffer or a copy of the ring.

```text
writer subscriber (write) ─┐
follower (read) ───────────┼──► SubscriberActor + cursor ──┐
follower (read) ───────────┘                               │
                                                          ▼
SerialWorker ──► SessionActor one ring [seq 41..57] ──► subscribers pull at
                                                          their own cursors
```

A slow cursor does not move the ring; it falls behind until it is outside the
retained window. The producer and faster subscribers keep going. The worker's
channel to the session actor is bounded, but it is sized for runtime health, not
per-subscriber flow control: a healthy session actor always drains it and evicts
from the ring rather than waiting for subscribers.

Implementation shape: a subscriber sends `Poll { cursor }` to the session actor
and receives the next chunks, or a `Gap`; a `watch` channel on `latest_seq`
wakes it when new data arrives. The session actor never pushes into a
subscriber's bounded mailbox, so there is no actor cycle and no deadlock between
a slow socket writer and the producer.

| Kind | Server → client binary frame | Client → server binary frame | Fanout | Resume |
| --- | --- | --- | --- | --- |
| `serial` | `u64 seq (big-endian) + payload` | raw payload | Many; any control subscriber may write | Yes, durable log |
| `rtt` | `u64 seq + u8 channel + payload` | ignored / control only | Many read-only | Yes, durable log |
| `dap` | raw payload | raw payload | Exactly one | No |

- `serial` and `rtt` frames carry a sequence number so the CLI can strip it and
  use it for resume.
- RTT payloads stay raw. The server does not decode defmt; each subscriber
  supplies its own ELF and keeps its current decoder and formatting behavior.
  This is exactly what makes "follow the debug logs" useful: several clients can
  decode the same RTT byte stream independently.
- `dap` is a raw byte stream because the DAP protocol has its own framing; it
  cannot tolerate an inserted header. DAP is single-subscriber and closes on
  detach.
- Text frames are JSON control messages, not data: `{"type":"pause"}`,
  `{"type":"resume"}`, `{"type":"ping"}`, `{"type":"pong"}`.
- The server enforces a maximum binary message size (64 KiB) and writes larger
  serial/RTT payloads as multiple chunks.
- The ring is byte-bounded (defaults: serial 256 KiB, RTT 1 MiB, DAP none) and
  holds only the live tail. Older data lives in the durable session log. A
  subscriber that cannot keep up is served from the ring, then the log; if it is
  outside retention it receives a `gap` marker and skips to the oldest retained
  data, or closes with `4409` under `replay=strict`. A slow subscriber never
  throttles the hardware or another subscriber. The ring is deliberately a
  *live* buffer, not the durable record.
- Data writes from a subscriber go through a separate bounded input channel to
  the worker. Writes are serialized by the session actor, so messages from
  multiple writers do not interleave within one write. The connection reader
  never blocks the downlink writer.
- Serial input is persisted too (user decision), with direction and timestamp,
  so replay/export can show the full interaction. Input can contain secrets;
  session storage permissions and purge/retention apply. Asciicast export still
  defaults to output-only unless `include_input=true`.
- Hardware options belong to the session, not the subscriber. Followers share
  the serial baud/settings and the RTT channel set chosen at creation. RTT
  channel filtering can still be done client-side from the channel byte; changing
  the hardware options requires closing and recreating the session.
- Write permission is checked per frame. A `mode=read` subscriber that sends
  binary data gets a text `{"type":"error","code":"write_forbidden"}` and stays
  attached; the frame is not forwarded.

### 10.5 Quotas and limits

- Global maximum sessions (default 16).
- Per-principal maximum sessions (default 4).
- Per-session maximum subscribers (default 8; hard cap 32).
- One session per resource, enforced by leases.
- Maximum change-follow connections (default 64) and data-plane connections
  (default 128), enforced independently.
- Default maximum session lifetime (default 8 hours) and attached idle timeout
  (default 30 minutes). Both configurable; idle timeout does not apply while
  data is flowing.
- Exceeding a session quota returns `429 too_many_sessions`; exceeding a
  subscriber quota returns `429 too_many_subscribers`. Both include
  `Retry-After`.

### 10.6 Access and management policy

| Concern | Policy |
| --- | --- |
| Session metadata in `GET /v1/sessions` | `read`/`control`/`admin` (metadata only, no payload) |
| Attach to the data plane | `control`/`admin` |
| Replay/export a closed/suspended session | `control`/`admin` |
| Write (serial) | Any attached `control` subscriber; `mode=read` suppresses its own input |
| Manage: PATCH, close, kick | Any `control`/`admin` |
| Creator disconnects, followers remain | No effect |
| Last subscriber leaves | Grace, then close |
| Any subscriber token revoked | Close that principal's subscribers |
| DAP | Single subscriber, `control` only |

There is deliberately no session owner and no sharing flag:

- The **lease** is the technical owner of the hardware. The session holds it; no
  principal does.
- `created_by` is audit metadata. It is useful in logs and UI, but it grants no
  permissions and does not affect revocation.
- Any `control` principal can attach or manage a session because `control` is
  already the "can do anything to hardware" scope. Multiple control clients can
  follow the same session without an opt-in.
- `read` can list session metadata but cannot attach to the data plane. If
  read-only monitoring becomes a requirement, add a dedicated `monitor` scope
  rather than a per-session sharing flag.
- This avoids coupling a long-lived shared console to one client token. Token
  rotation or revocation should not kill a session that other operators are
  using.

The trade-off is that a `control` client can close or modify another operator's
session. For a trusted lab tool this is acceptable. If strict isolation is needed
later, add an optional capability or owner model rather than assuming it now (see
Q18).

Metadata visibility for `read` is a small expansion of the current scope, but it
never exposes session bytes. Update the scope table in
[authentication.md](authentication.md) accordingly.

### 10.7 CLI attachment semantics

There should not be separate attach and follow commands. With no session owner
and a single active session per resource, "create or join" is a mode, not a
different resource workflow. Keep one attach command per resource kind and make
follow/new/read-only explicit flags:

| Intent | Probe/RTT | Serial |
| --- | --- | --- |
| Attach to the active session, or create one if none | `boot probe-attach <server>/<probe>` | `boot serial-shell <server>/<port>` |
| Join an existing session only | `--follow` | `--follow` |
| Create a new session only | `--new` | `--new` |
| Read-only subscriber | `--read-only` | `--read-only` |
| Join a specific session | `--session <id>` | `--session <id>` |
| Request serial write mode | n/a | `--write` |

Default behavior:

- If exactly one active session exists for the resource, attach to it.
- If none exists, create one. `probe-attach` creates an RTT session;
  `serial-shell` creates a serial session.
- `--follow` requires an existing session and never creates one.
- `--new` requires no active session and never joins one; a busy resource is a
  clear `409` with the existing session ID.
- `serial-shell` attaches in write mode by default. `serial-shell --follow`
  attaches read-only to avoid accidental input; `--write` overrides that.
- `--read-only` always requests read mode.
- `probe-attach` is always read-only at the data level; RTT has no input stream.
- `--session <id>` attaches directly and is useful in scripts.

`boot probe-follow` and `boot serial-follow` are therefore removed. They were
duplicating discovery, mode selection, and reconnect logic for no separate
capability. `boot session-replay` remains separate because it targets retained
past data rather than a live hardware session.

`boot probe-run` keeps its post-flash attach behavior and uses the same
create-or-join logic. `boot start-probe-dap` remains separate because it runs a
local TCP-to-DAP bridge, not an interactive stream.

Other session commands stay separate:

- `boot list-sessions [--all|--active|--past]`
- `boot session-replay <server>/<session-id> [--from <seq>] [--elf <path>] [--include-input]`
- `boot session-export <server>/<session-id> --format asciicast-v3 --out <file>`
- `boot start-probe-dap`

## 11. Change follow and session data WebSockets

### 11.1 Change follow protocol

`GET /v1/changes/{id}/follow` upgrades to a WebSocket for exactly one change.
It is the primary progress path; clients do not poll `GET /v1/changes/{id}`.

The change actor keeps a bounded live history of change/task update records and
log entries, and the terminal record is persisted to the runtime database. A
follower has its own cursor into the live history. After a restart, a terminal
change is loaded from the store and can still be followed (the server sends the
snapshot and closes).

Server → client, first frame:

```json
{"v":1,"type":"hello","protocol":1,"seq":0,
 "change":{"id":"chg_01J9Z6...","status":"doing","tasks":[]}}
```

Then task and change updates are sent individually as they happen:

```json
{"v":1,"type":"change.updated","seq":1,
 "change":{"id":"chg_01J9Z6...","status":"doing","ready":false,
           "error":null,"data":{}}}
{"v":1,"type":"task.updated","seq":2,
 "task":{"id":"tsk_01J9Z6...","kind":"program","status":"doing",
         "progress":{"done":24576,"total":49152,"percent":50.0}}}
{"v":1,"type":"change.log","seq":3,
 "entries":[{"time":"...","level":"info","message":"..."}]}
{"v":1,"type":"change.completed","seq":4,
 "change":{"id":"chg_01J9Z6...","status":"done","ready":true}}
```

Rules:

- `hello` is always the first frame and contains the full change snapshot,
  including all tasks and retained logs.
- `change.updated` carries change-level fields only; it does not repeat tasks.
- `task.updated` carries one complete task snapshot; clients upsert by task ID.
  A task that appears for the first time is a creation.
- `change.log` carries new log entries.
- `change.completed` is the terminal marker and includes the final change-level
  fields.
- Clients apply updates to the snapshot from `hello`. They must tolerate
  duplicate or reordered updates by using `seq`.
- `?after=<seq>` resumes. If the cursor is still in the per-change history, the
  server replays the individual updates from there. If it has been evicted, it
  sends a fresh `hello` snapshot with `replayed: false`.
- If the change is already terminal when the client connects, the server sends
  the terminal snapshot and closes with `1000`.
- Multiple clients may follow the same change. They all get the same updates;
  there is no mutation over this socket.
- Task/change transitions are sent as they happen. High-frequency progress
  fields may be coalesced to 10 Hz; terminal state is always sent.
- A follower socket has a bounded outbound queue. Overflow closes with `4408`;
  the client reconnects with `after=<seq>` or accepts a fresh snapshot.
- The live per-change history is bounded (default 256 update records or 5
  minutes). Terminal changes are persisted, so a later follow can still return
  the final snapshot and logs. There is no server-wide sequence or epoch.

`GET /v1/changes/{id}` remains available for a one-shot snapshot.

### 11.2 Session data protocol

`GET /v1/sessions/{id}/io` upgrades to a WebSocket. Every upgrade is a new
subscriber. First server frame:

```json
{"v":1,"type":"hello","protocol":1,
 "subscriber_id":"sub_01J9Z6...","mode":"read","session":{},
 "data_seq":123,"oldest_seq":100,"latest_seq":123}
```

Then binary frames according to section 10.4. Text control frames are reserved
for protocol messages. A binary frame before `hello` is a protocol error.
`?after=<data_seq>` resumes from a cursor; `?replay=live` (default) skips gaps
with a marker and `?replay=strict` fails with `4409` instead. Each subscriber has
its own cursor, so two clients can follow the same session from different points.

`oldest_seq` and `latest_seq` describe the retained durable range, not just the
live ring. `?after=0` replays the entire retained session. For a closed or
suspended session, the server sends the replay followed by `session.ended` and
closes with `1000`. For an active session, it reads the durable log up to the
live boundary and then continues from the ring.

`?include_input=true` additionally emits retained serial input records as text
frames:

```json
{"v":1,"type":"session.input","seq":42,"data":"<base64>"}
```

Input is never sent to live subscribers by default. It is available for explicit
audit/replay and asciicast export; it may contain secrets.

Session lifecycle changes are reported as text messages on this socket:
`session.active`, `session.suspended`, `session.resumed`, `session.degraded`, and
`session.failed`, each followed by an updated session snapshot. A client that
only wants metadata can use `GET /v1/sessions/{id}`.

### 11.3 Authentication and close codes

- Change-follow sockets require the change owner or an admin. `control` is not
  enough to read another principal's change details. This may be relaxed later
  if team-wide change watching becomes a requirement.
- The session data plane requires `control`; any attached control subscriber may
  write (serial), or choose `mode=read`. A `read` token can see session metadata
  but cannot open `/io`.
- The principal's scope is stored on the connection actor.
- Token existence and expiry are revalidated on attach and periodically (default
  30 seconds) for long-lived sockets. Any command also rechecks. A revoked or
  expired token closes the connection with `4401`.
- Session token revocation affects only that principal's subscribers. If it was
  the last subscriber, the normal zero-subscriber grace period applies.

| Close code | Meaning |
| --- | --- |
| `1000` | Normal close, including terminal change snapshot or session replay end |
| `1001` | Going away |
| `1012` | Service restart |
| `4400` | Protocol error |
| `4401` | Unauthorized or token revoked/expired |
| `4403` | Forbidden (insufficient scope or permission) |
| `4404` | Session/change not found |
| `4408` | Slow consumer |
| `4409` | Cursor expired / fresh snapshot required |
| `4410` | Session closed |
| `4411` | Resource gone |
| `4412` | Subscriber removed by a control principal/admin |

### 11.4 Backpressure

- Change follower: bounded outbound queue (default 256 updates). On overflow,
  close with `4408`; the client reconnects with its last `seq` or accepts a fresh
  snapshot. Update rate is low after coalescing.
- Session data: one pull-based ring per session with per-subscriber cursors, as
  described in section 10.4. The producer never waits for a subscriber. A
  subscriber that falls outside the ring gets a `gap` and skips forward, or is
  closed with `4409`; other subscribers and the hardware are unaffected.
- Control mailbox: bounded; HTTP adapters apply a short timeout and return
  `503 runtime_busy` rather than queueing without limit.

### 11.5 Client algorithm (no polling)

```text
Flash:
1. POST /v1/artifacts with the ELF, or reuse an artifact.
2. POST /v1/probes/{id}/flash -> 202 + Location: /v1/changes/{id}
3. GET /v1/changes/{id}/follow
4. Receive hello + updates until change.completed.
5. On socket loss: reconnect with ?after=<last_seq>.
   - cursor in history -> replay
   - cursor evicted   -> fresh hello snapshot

Session:
1. POST /v1/probes/{id}/sessions or /v1/serial-ports/{id}/sessions
   -> 201 + Location: /v1/sessions/{id}
2. GET /v1/sessions/{id}/io -> stream data
3. On disconnect: reconnect with ?after=<last_data_seq>
4. For a past session: GET /v1/sessions/{id}/io?after=0, replay,
   receive session.ended, exit
```

### 11.6 Why not a general event stream

A server-wide `/events` socket would need subscriptions, a global sequence,
fan-out, filtering, replay, and resync. The CLI does not need any of that to
flash or attach: it knows the change/session ID from the creation response and
can follow exactly that resource. Inventory and session lists are ordinary GETs,
not high-frequency progress.

Defer a general event stream until there is a concrete dashboard, hotplug UI, or
multi-resource watcher. It can be added later without changing the creation
endpoints.

## 12. HTTP surface

### 12.1 Endpoints

The surface is resource-scoped:

- Creation paths identify the parent resource (`/probes/{id}`, `/serial-ports/{id}`)
  and return a new change/session with `Location`.
- The body contains options and references, never a second copy of the resource
  ID.
- `GET /v1/changes/{id}/follow` follows exactly the change returned by the
  creation call.
- `GET /v1/sessions/{id}/io` attaches to exactly the session returned by the
  creation call.
- There is no global event stream and no generic `POST /changes` or
  `POST /sessions`.

| Method | Path | Scope | Notes |
| --- | --- | --- | --- |
| `GET` | `/v1/health` | public | status, version, uptime |
| `GET` | `/v1/server` | read | limits, counts, uptime, degraded components |
| `GET` | `/v1/probes` | read | inventory, no hardware open |
| `GET` | `/v1/probes/{id}` | read | metadata, lease summary, cached target |
| `GET` | `/v1/serial-ports` | read | inventory |
| `GET` | `/v1/serial-ports/{id}` | read | metadata, lease summary |
| `POST` | `/v1/probes/{id}/flash` | control | create flash change; `202` + `Location` |
| `POST` | `/v1/probes/{id}/reset` | control | create reset change; `202` + `Location` |
| `POST` | `/v1/probes/{id}/inspect` | control | create inspect change; `202` + `Location` |
| `GET` | `/v1/changes?select=all\|in-progress\|ready` | control | own changes; admin all |
| `GET` | `/v1/changes/{id}` | control | own/admin |
| `GET` | `/v1/changes/{id}/follow` | control | WebSocket for one change; owner/admin |
| `POST` | `/v1/changes/{id}` | control | `{"action":"abort"}` |
| `POST` | `/v1/probes/{id}/sessions` | control | create RTT/DAP session; `201` + `Location` |
| `POST` | `/v1/serial-ports/{id}/sessions` | control | create serial session; `201` + `Location` |
| `GET` | `/v1/sessions?select=active\|past\|all` | read | session metadata; retained past sessions included |
| `GET` | `/v1/sessions/{id}` | read | metadata for `read`/`control`/`admin` |
| `PATCH` | `/v1/sessions/{id}` | control | change `max_subscribers`/lifecycle policy |
| `POST` | `/v1/sessions/{id}/resume` | control | retry a suspended serial/RTT session |
| `GET` | `/v1/sessions/{id}/subscribers` | control | `control`/`admin`; subscriber IDs, modes, cursors |
| `DELETE` | `/v1/sessions/{id}` | control | close and retain data; `?purge=true` removes all |
| `DELETE` | `/v1/sessions/{id}/data` | control | purge retained session data, keep metadata |
| `DELETE` | `/v1/sessions/{id}/subscribers/{subscriber_id}` | control | kick one follower |
| `GET` | `/v1/sessions/{id}/io` | control | WebSocket data plane; multiple subscribers; any control subscriber may write |
| `GET` | `/v1/sessions/{id}/export` | control | derived export; `format=asciicast-v3` |
| `POST` | `/v1/artifacts` | control | streamed upload; `201` + `Location` |
| `GET` | `/v1/artifacts/{id}` | control | metadata; owner/admin |
| `DELETE` | `/v1/artifacts/{id}` | control | owner/admin |
| `GET` | `/v1/tokens` | admin | unchanged semantics |
| `POST` | `/v1/tokens` | admin | unchanged semantics |
| `DELETE` | `/v1/tokens/{selector}` | admin | unchanged semantics |
| `DELETE` | `/v1/tokens?all=true` | admin | unchanged semantics |

`GET /v1/probes/{id}` may include a cached target from a previous inspection,
but it never attaches. Refreshing target information is an inspect change.

### 12.2 Examples

**Flash**

```http
POST /v1/artifacts HTTP/1.1
Authorization: Bearer boot_v1_...
Content-Type: application/octet-stream

<elf bytes>
```

```http
HTTP/1.1 201 Created
Location: /v1/artifacts/art_01J9Z6...

{"id":"art_01J9Z6...","size":49152,"sha256":"...","expires_at":"2026-11-06T13:00:00.000Z"}
```

```http
POST /v1/probes/probe_1a2b.../flash HTTP/1.1
Authorization: Bearer boot_v1_...
Idempotency-Key: deploy-2026-11-06-1

{"artifact":"art_01J9Z6...","options":{"reset":true,"verify":true}}
```

```http
HTTP/1.1 202 Accepted
Location: /v1/changes/chg_01J9Z6...

{"change":"chg_01J9Z6...","status":"queued"}
```

The `202` body intentionally carries only the change ID and initial status. Follow
`GET /v1/changes/chg_01J9Z6.../follow`, or fetch the snapshot at the `Location`.

**Follow a change**

```http
GET /v1/changes/chg_01J9Z6.../follow HTTP/1.1
Authorization: Bearer boot_v1_...
Upgrade: websocket
```

The first frame is a full change snapshot; subsequent frames are updates, logs,
and the terminal result.

**Session**

```http
POST /v1/serial-ports/port_1a2b.../sessions HTTP/1.1
Authorization: Bearer boot_v1_...

{"options":{"baud_rate":115200},"max_subscribers":8}
```

```http
HTTP/1.1 201 Created
Location: /v1/sessions/ses_01J9Z6...

{"id":"ses_01J9Z6...","kind":"serial","state":"starting",
 "subscribers":0,"max_subscribers":8, ...}
```

The creating client attaches:

```http
GET /v1/sessions/ses_01J9Z6.../io HTTP/1.1
Authorization: Bearer boot_v1_...
Upgrade: websocket
```

Another control client follows the same console read-only:

```http
GET /v1/sessions/ses_01J9Z6.../io?mode=read HTTP/1.1
Authorization: Bearer boot_v1_...   # control token
Upgrade: websocket
```

Each upgrade gets its own `subscriber_id` and cursor. A `read` token can list the
session metadata but cannot attach; a `control` token can attach read-only with
`?mode=read` even when the session allows writes.

**Abort**

```http
POST /v1/changes/chg_01J9Z6... HTTP/1.1

{"action":"abort"}
```

### 12.3 Errors

HTTP errors use a stable machine code in addition to the Dropshot message. The
wire contract is the `code` field; the adapter may use a custom response body or
Dropshot's error details, as long as the status and fields below are preserved.
The initial shape:

```json
{
  "code": "resource_busy",
  "message": "probe is in use",
  "resource": "probe_1a2b...",
  "holder": {"type":"session","id":"ses_01J9Z6..."},
  "retry_after_seconds": 1
}
```

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid_request` | 400 | Malformed body, options, or idempotency key |
| `unauthorized` | 401 | Missing/invalid/expired/revoked token |
| `forbidden` | 403 | Insufficient scope or permission |
| `not_found` | 404 | Unknown resource, change, session, artifact |
| `resource_busy` | 409 | Lease held by another change/session |
| `resource_ambiguous` | 409 | Selector matches more than one probe |
| `resource_replaced` | 410 | Generation changed; re-enumerate |
| `idempotency_conflict` | 409 | Same key, different body |
| `dap_exclusive` | 409 | DAP sessions allow exactly one subscriber |
| `too_many_sessions` | 429 | Per-principal or global session quota |
| `too_many_subscribers` | 429 | Per-session subscriber quota |
| `storage_exhausted` | 429 | Session/change storage quota cannot be satisfied |
| `not_resumable` | 409 | DAP or terminal session cannot be resumed |
| `not_asciicast_compatible` | 409 | Stream is binary or not valid UTF-8 for asciicast export |
| `session_active` | 409 | Data purge requires the session to be closed/suspended |
| `write_forbidden` | WS error | Read-only subscriber attempted a serial write |
| `artifact_expired` | 410 | Artifact TTL elapsed |
| `draining` | 503 | Server is shutting down |
| `runtime_busy` | 503 | Control mailbox timeout |
| `degraded` | 503 | Required runtime component is unavailable |

Execution errors are returned inside the change:

```json
{
  "status": "failed",
  "error": {"code":"resource_gone","message":"probe disappeared","retryable":true},
  "ready": true
}
```

### 12.4 Versioning and compatibility

- There is no `/v2`. There are no existing deployments, so `/v1` changes in
  place.
- The old streaming endpoints (`GET .../attach`, `GET .../shell`, `GET .../dap`,
  and `POST /v1/probes/{selector}`) are replaced by the resource-scoped creation
  endpoints plus `changes/{id}/follow` and `sessions/{id}/io`. They are removed,
  not aliased.
- Token endpoints keep their existing `/v1/tokens` paths and semantics.
- WebSocket messages carry `"v": 1` independently of the HTTP path. New fields
  are additive; clients ignore unknown fields and unknown enum values.
- `boot-api-client` is regenerated from the revised OpenAPI spec. WebSocket
  message types live in `boot-protocol` and are used by both server and client.
  The generated crate does not depend on the server runtime.

## 13. Authentication integration

- The existing `Auth<S>` extractor remains the single HTTP authorization path.
  It moves from downcasting `AuthState` to downcasting `ApiState`, which contains
  `auth: AuthState` and `runtime: RuntimeHandle`.
- `boot-protocol` has no auth dependencies.
- Scope policy:
  - `read`: inventory, server status, token-free health, and session metadata
    (no data-plane access, no change details).
  - `control`: change creation, follow/abort of own changes, session creation,
    resume, replay/export, and management, session data-plane attach, artifacts,
    and serial writes.
  - `admin`: tokens and all changes/sessions; can follow/abort any change,
    replay any session, and purge data.
- Session authority:
  - Sessions have a `created_by` field for audit and display only. There is no
    per-session owner and no owner-based authorization.
  - Any `control` principal may attach, PATCH policy, resume, kick a subscriber,
    or close a session. `admin` can do the same and inspect everything.
  - All session metadata is visible to `read`; session bytes and replay require
    `control`.
  - Resource inventory is visible to all `read` tokens; lease holder details are
    admin-only.
- Changes and artifacts still record their creator. Only the creator or an admin
  may abort a change or delete an artifact. Persisted session/change history is
  not removed by token revocation.
- Revocation:
  - Change-follow and session sockets close within the revalidation interval.
  - Revoking a subscriber's token closes only that principal's subscribers. If it
    was the last subscriber, the normal zero-subscriber grace period applies.
  - Queued changes created by a revoked token are cancelled.
  - Running changes request cancellation at the next safe point. If the backend
    cannot interrupt a driver call, the change remains `cancelling` until that
    call returns. The target may be left partially programmed; the change's
    history records the interruption.
- WebSocket upgrade authentication uses the normal `Authorization` header. No
  token appears in a URL. A future browser client needs a short-lived ticket
  endpoint; that is out of scope.
- The token store remains SQLite and per-request, now sharing `boot.sqlite3` and
  the connection pool with runtime metadata. Token lookups remain indexed reads;
  runtime writes are state transitions, not per request. Since the CLI can write
  the DB directly, the server cannot rely on an in-process revocation
  notification; periodic revalidation remains the fallback.

## 14. Persistence and recovery

### 14.1 Decision summary

| State | Storage | Survives restart | Notes |
| --- | --- | --- | --- |
| Tokens | `boot.sqlite3` | Yes | Shared API DB |
| Session metadata | `boot.sqlite3` | Yes | Kind, resource, options, state, sequence bounds |
| Session data | Append-only segment files | Yes | Replayable until retention/purge |
| Change history | `boot.sqlite3` | Yes | Audit/inspection; execution is not resumed |
| Artifacts | Spool files + metadata in `boot.sqlite3` | TTL | Existing design |
| Resources | In-memory | Rebuilt | Inventory watcher |
| Leases | In-memory | No | Reacquired during recovery |
| Subscribers/cursors | In-memory | No | Clients reconnect with `after` |
| In-flight change execution | n/a | **No** | Marked failed with `server_restart` |
| DAP session | n/a | **No** | Marked failed; no resume or data replay |

The key distinction:

- **Sessions are durable resources.** Their identity, configuration, sequence
  number, and bytes survive a restart. A serial/RTT session can reattach to a
  still-present target.
- **Changes are durable history, not durable execution.** A flash cannot be
  safely resumed without knowing the exact hardware state. On restart, a
  non-terminal change becomes a terminal `failed` change with
  `error.code = "server_restart"`.

#### 14.1.1 Store boundary

Use one production `Store` struct, not one giant trait:

```rust
pub struct Store {
    pool: SqlitePool,
    session_dir: PathBuf,
    artifact_dir: PathBuf,
}

impl TokenStore for Store { /* ... */ }
impl SessionStore for Store { /* ... */ }
impl ChangeStore for Store { /* ... */ }
impl ArtifactStore for Store { /* ... */ }
```

Rationale:

- One `Store` owns the pool, paths, migrations, and transaction helpers.
- Narrow capability traits keep auth tests from implementing session/change
  methods, and runtime tests from implementing token methods.
- Call sites depend on the narrowest trait they need:
  `Arc<dyn TokenStore>` for auth, `Arc<dyn SessionStore>` for recovery, etc.
  The runtime can hold `Arc<Store>` and pass trait objects internally.
- Do not add a single `Store` trait with every method. It would couple auth to
  runtime schema and make fakes noisy.

`store::Store` is the production implementation. The CLI opens the same store
for token administration and runs the same migrations.

### 14.2 Storage layout

| Data | Path | Permissions |
| --- | --- | --- |
| API/state DB | `$XDG_DATA_HOME/boot/boot.sqlite3` | `0600`, existing checks |
| Session logs | `$XDG_DATA_HOME/boot/sessions/<session_id>/` | dir `0700`, files `0600` |
| Artifacts | `$XDG_DATA_HOME/boot/artifacts/` | dir `0700`, files `0600` |

Runtime state reuses the existing API database rather than introducing a second
SQLite file:

- `boot-api` and `boot admin` already open and migrate `boot.sqlite3`.
- Token tables and runtime tables share the same permission checks and backup
  story.
- Session payloads stay in segment files, so the database only receives small
  metadata writes: session create/state transitions and change create/task
  transitions. It does not receive one row per data chunk.
- The CLI already has the DB open for token administration; it runs the runtime
  migrations too, but does not write session/change rows.
- A migration failure is already fatal at startup for auth; the same rule applies
  to runtime tables.

Keep the existing SQLite settings:

```sql
PRAGMA application_id = 0x424F4F54;  -- "BOOT"
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA secure_delete = ON;
PRAGMA journal_mode = DELETE;
PRAGMA synchronous = FULL;
```

The API DB was designed for low-frequency writes with a self-contained rollback
journal. Persistence adds only metadata transitions, not per-chunk writes, so
the existing journal choice still fits. If session/change state transitions
later cause contention, revisit WAL with measurements; do not switch blindly.

Paths are overridable with the existing `--api-db`/`BOOT_API_DB` and a new
`--session-dir`/`BOOT_SESSION_DIR`. There is no separate `--state-db`.

This makes `boot-api/src/store/` the shared database handle: one pool, one
initial migration set, and both `TokenStore` and `RuntimeStore` implemented over
it. Extend the initial migration rather than adding a follow-up migration.

[authentication.md](authentication.md) must be updated: the API DB is no longer
token-only. It now also contains session/change metadata and bounded change logs.
It still contains no plaintext tokens and no session payload bytes.

### 14.3 Session metadata schema

```sql
-- Included in the initial migration for the shared boot.sqlite3.
-- application_id remains 0x424F4F54 ("BOOT"); do not change it.

CREATE TABLE sessions (
    id                       TEXT PRIMARY KEY,       -- ses_<ULID>
    kind                     TEXT NOT NULL,          -- serial|rtt|dap
    resource_id              TEXT NOT NULL,
    generation               INTEGER NOT NULL,
    options_json             TEXT NOT NULL,
    max_subscribers          INTEGER NOT NULL,
    detach_timeout_seconds   INTEGER NOT NULL,
    created_by               TEXT NOT NULL,          -- audit only
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    closed_at                INTEGER,
    state                    TEXT NOT NULL,          -- creating|starting|active|suspended|resuming|closing|closed|failed
    first_seq                INTEGER NOT NULL,       -- oldest retained data
    latest_seq               INTEGER NOT NULL,       -- high-water data sequence
    data_bytes               INTEGER NOT NULL,
    replayable               INTEGER NOT NULL,       -- 0 once data is purged
    resume_count             INTEGER NOT NULL DEFAULT 0,
    last_error_json          TEXT,
    CHECK (kind IN ('serial','rtt','dap')),
    CHECK (state IN ('creating','starting','active','suspended','resuming','closing','closed','failed'))
);

CREATE TABLE session_segments (
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    segment_no  INTEGER NOT NULL,
    first_seq   INTEGER NOT NULL,
    last_seq    INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    path        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, segment_no)
);

CREATE TABLE session_marks (
    mark_id     TEXT PRIMARY KEY,                    -- mrk_<ULID>
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,                       -- resumed|suspended|shutdown|gap|closed
    detail_json TEXT
);

CREATE INDEX session_marks_by_session ON session_marks(session_id, seq);
CREATE INDEX sessions_by_state ON sessions(state, updated_at);
```

`session_marks` records non-data events such as a restart gap or a recovery
attempt. They are replayed as control messages, not payload.

### 14.4 Session data log

Each serial and RTT session writes append-only segment files. DAP streams are not
persisted for replay; only DAP session metadata is retained. A segment rotates at
a configured size (default 64 MiB). The record format is:

```text
magic:      "BLOG" (4 bytes)
version:    u8
record_type:u8     // output, input, gap, lifecycle
channel:    u8     // RTT channel; 0 for serial/DAP
flags:      u16
seq:        u64
timestamp:  u64    // Unix milliseconds
length:     u32
crc32c:     u32    // covers the payload
payload:    [u8; length]
```

Rules:

- `seq` is monotonic per session and survives restarts. New data continues after
  the last durable sequence; it never restarts at zero.
- A hard server crash can lose the last few records if they were not synced.
  The log writer flushes periodically (default 100 ms or 1 MiB) and on segment
  close. Segment manifests track the highest durable sequence; the API DB
  records a high-water mark at flush boundaries. On startup, the server takes
  the maximum of the two, keeps the sequence monotonic, records a `gap` for the
  missing range, and continues after the high-water mark. It never reuses a
  sequence number.
- The last incomplete or corrupt record is truncated. If earlier data is
  corrupt, the segment is quarantined and a `gap` is recorded.
- Replay reads the segment files. The in-memory ring is only the live tail.
  A subscriber with `after` older than the ring is served from disk before
  switching to live data.
- A `gap` record marks known missing data, such as server downtime or a disk
  write failure. Clients see it as a control message.
- The writer has a bounded queue (default 4 MiB). If the disk cannot keep up,
  the session reports `persistence_degraded`, keeps the live stream running, and
  writes a `gap` for the dropped records. It does not silently claim lossless
  replay.
- Serial input is written as `input` records with the same monotonic record
  sequence. Live subscribers receive only `output` records; replay defaults to
  output too. Asciicast export can include input events, and `session-replay`
  can display them with an explicit flag. Input records may contain secrets.
- `seq` is a record sequence, not a byte offset. Subscriber-visible output
  sequences can skip input records; that is not a gap. An explicit `gap` record
  marks missing output.
- Session data is not compressed initially. Compression can be added per segment
  later without changing the API.

### 14.5 Recovery on restart

Startup sequence:

1. Open and migrate `boot.sqlite3`.
2. Scan session directories, truncate invalid tails, and reconcile
   `first_seq`/`latest_seq`/`data_bytes` with the sequence recorded in the
   API DB. Missing ranges become gap marks.
3. Load non-terminal sessions. Treat `active`/`starting` as `suspended` with
   reason `server_restart`.
4. Start the inventory watcher and wait for its first enumeration.
5. For each suspended serial/RTT session whose resource is present with the same
   generation:
   - acquire the lease;
   - move to `resuming`;
   - open the serial port or reattach the probe/RTT;
   - on success move to `active`, increment `resume_count`, and append a
     `resumed` mark;
   - on failure stay `suspended`, record `last_error`, and keep the data
     replayable.
6. DAP sessions are marked `failed` with `server_restart`; they never auto-resume
   and have metadata only (no data replay).
7. Any non-terminal change is marked `failed` with `server_restart` and persisted.
8. Start serving HTTP/WebSocket traffic. Recovery may still be in progress; the
   session state is visible as `resuming` or `suspended`.

Recovery details:

- **Serial:** reopen the same stable resource ID and settings. Bytes produced
  while the server was down are lost; append a gap mark with the downtime.
- **RTT:** reopen the probe and reattach RTT using the stored address/channel
  options. If the target reset or the RTT control block is gone, the session
  stays `suspended` rather than resetting or disturbing the target.
- **Resource missing:** keep the session `suspended` and replayable. It does not
  block a new session on that resource. `POST /v1/sessions/{id}/resume` retries
  if the resource returns; `DELETE` closes/purges it.
- **Manual resume:** allowed for `suspended` serial/RTT sessions. The caller has
  `control`; the session reacquires the lease and follows the same path as
  recovery.
- **Graceful shutdown:** before exit, mark live sessions `suspended` with reason
  `server_shutdown`, flush the log, and close client sockets with `1012`. On
  restart they use the same recovery path.

There is at most one active session per resource, enforced by leases. A
suspended session does not hold a lease; if a new session takes the resource,
the old one remains replay-only until manually resumed after the new one closes.

### 14.6 Replay and deletion

- `GET /v1/sessions` includes `first_seq`, `latest_seq`, `data_bytes`,
  `replayable`, `closed_at`, and `resume_count`. `select=active` covers
  starting/active/resuming; `past` covers closed/failed/suspended; `all` covers
  both.
- `GET /v1/sessions/{id}/io?after=0` replays the entire retained serial/RTT
  session. DAP has no data replay. For a closed/suspended session, the server
  sends the replay and then a `session.ended` message, closing with `1000`.
- For an active session, replay reads the durable log up to the live boundary
  and then continues from the ring.
- `?replay=strict` closes with `4409` if the requested cursor is outside
  retention instead of skipping with a gap.
- CLI: `boot session-replay <server>/<session-id> [--from <seq>] [--elf <path>] [--include-input]`.
- `DELETE /v1/sessions/{id}` closes a live session but retains data.
- `DELETE /v1/sessions/{id}/data` purges the segment files and marks the session
  `replayable = false`; metadata remains for audit. It is rejected with `409`
  while the session still holds hardware (close or suspend it first).
- `DELETE /v1/sessions/{id}?purge=true` closes the session if needed, then
  removes metadata, marks, and data.

Replay is "entire retained session", not literally forever. Retention is
configurable (section 14.8).

### 14.7 Change persistence

Change execution is not resumed, but history is worth keeping:

- Persist a change record at creation and on each task transition (not every
  progress tick).
- Persist the terminal snapshot, tasks, error, and bounded log ring.
- Persistence schema:

```sql
CREATE TABLE changes (
    id            TEXT PRIMARY KEY,        -- chg_<ULID>
    kind          TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    ready_at      INTEGER,
    snapshot_json TEXT NOT NULL,           -- full current/final change
    log_json      TEXT NOT NULL,           -- bounded log ring
    CHECK (status IN ('queued','waiting','doing','done','failed','cancelling','cancelled'))
);

CREATE INDEX changes_by_status ON changes(status, updated_at);
```

- On startup, any non-terminal change becomes `failed` with
  `error.code = "server_restart"`, `ready = true`, and a log line explaining the
  interruption. It is persisted.
- `GET /v1/changes/{id}/follow` can serve a terminal change from the store after
  restart: send the snapshot and close.
- No automatic retry. A user can submit a new flash change.
- Artifacts remain TTL-based; change history does not pin them forever. A past
  change may reference an expired artifact; this is visible in its metadata.

Why not resume execution:

- A flash may have partially erased or programmed the target. The old probe
  session, core state, and progress are gone.
- Automatically reflashing could be catastrophic on a device that is already
  running or has changed state.
- A durable saga/undo model does not help: there is no safe compensating action
  for a partially written flash. This is the same reason Steno is not adopted
  (section 4.4).

### 14.8 Retention and quotas

Defaults, all configurable:

| Data | Retention |
| --- | --- |
| Active sessions | Until closed |
| Suspended sessions | Until closed/purged; data subject to retention |
| Past session data | 7 days or 10 GiB total |
| Per-session data | 2 GiB |
| Change history | 30 days or 5,000 changes |
| Change logs | 64 KiB or 1,000 lines per change |
| Artifacts | 1 hour or explicit delete |

The reaper deletes oldest terminal sessions/changes first when a quota is
exceeded. If session storage is full and cannot evict enough history, new
session creation fails with `429 storage_exhausted`; existing live sessions are
not silently discarded. `GET /v1/server` exposes usage and quota values.

### 14.9 Durability, security, and failure

- Session data can contain secrets from a serial console or RTT logs. The
  session directory is `0700`, files are `0600`, and deletion is a real unlink.
  Document that backups and filesystem snapshots may retain deleted data.
- `DELETE .../data` removes segment files; SQLite `secure_delete` clears
  metadata pages where supported.
- Disk full or log write failure: mark the session `persistence_degraded`, keep
  the live stream for a short grace period, then suspend/fail the session. Do not
  claim the missing range is replayable; append a gap when possible.
- Corrupt segment: truncate the invalid tail; if earlier data is corrupt,
  quarantine the segment and mark a gap. Do not fail the whole server.
- Database migration failure is fatal at startup; auth and runtime tables share
  the same `_sqlx_migrations` history.
- The API DB and session directories are backed up or snapshotted by the
  operator only if they accept that change logs and session logs may contain
  sensitive data.

### 14.10 Database writes

- Session creation: one insert.
- State transitions: small updates; not per data chunk.
- Session log segments: file writes, not DB writes per chunk.
- Change creation and task transitions: small updates.
- No per-request writes on inventory/auth hot paths.
- Observability counters stay in memory.

### 14.11 Open terminal-session export: asciicast v3

The canonical storage format stays the native segment log. It preserves sequence
numbers, gaps, RTT channels, binary payloads, and resume semantics. For
compatibility, add an **export** format based on
[asciicast v3](https://docs.asciinema.org/manual/asciicast/v3/), the current
asciinema format.

Asciicast v3 is newline-delimited JSON:

```json
{"version":3,"term":{"cols":80,"rows":24,"type":"xterm-256color"},"timestamp":1504467315,"title":"serial console"}
[0.248,"o","\u001b[1;31mHello\u001b[0m\r\n"]
[1.001,"o","Booting...\r\n"]
[0.143,"m","session resumed"]
[0.887,"x","0"]
```

It has `o` output, `i` input, `m` marker, `r` resize, and `x` exit events, with
relative intervals. It is supported by the asciinema CLI and player.

**We should not use it as the storage format.** It loses or cannot represent:

- monotonic data sequence numbers and cursor-based replay;
- gaps and server-restart boundaries;
- RTT channel numbers;
- arbitrary binary bytes (event data must be valid UTF-8 JSON strings);
- DAP protocol framing;
- session state, leases, write permissions, and subscriber metadata.

An asciicast file is a terminal recording, not a byte-stream journal.

Export endpoint:

```http
GET /v1/sessions/{id}/export?format=asciicast-v3&channel=0&include_input=false
Authorization: Bearer boot_v1_...
```

- `format` is `asciicast-v3` initially. v2 can be added if a tool needs it.
- `channel` selects an RTT channel; serial uses the single stream. The default
  is channel 0.
- `include_input=false` by default, matching asciinema's default non-capture of
  keyboard input. Serial writes can be emitted as `i` events when enabled.
- The export streams from the durable log; it does not load the session into
  memory.
- Header timestamp is the session `created_at`; title defaults to
  `<kind> <resource_id>` or the session ID; `term` defaults to 80x24 and
  `xterm-256color`.
- Gap and lifecycle marks become `m` events. Session close becomes `x`.
- Only UTF-8-compatible output is exportable. If the stream contains invalid
  UTF-8 or non-text binary, the endpoint returns `409 not_asciicast_compatible`
  unless `?lossy=true` is explicitly requested. Defmt-encoded RTT is binary and
  is not asciicast-compatible.
- DAP sessions have no data replay and therefore no asciicast export.
- Media type: `application/x-asciicast`; suggested extension: `.cast`.

CLI:

```text
boot session-export <server>/<session-id> --format asciicast-v3 --out session.cast
```

Alternatives considered:

| Format | Why not chosen |
| --- | --- |
| ttyrec | Simple and open, but older, less tooling, no current browser player ecosystem |
| `script` + `scriptreplay` | Text-oriented timing files, awkward for binary and metadata |
| Custom JSONL | Reinvents asciinema; no player compatibility |
| Asciicast as canonical storage | Loses sequence/gap/channel/binary semantics |

The native log remains the source of truth; asciicast is a derived compatibility
view for terminal-like sessions. Implementation is a small streaming JSONL
writer over the segment reader; no asciinema library dependency is required.

## 15. Observability

- Use tracing spans: `change{id,kind,resource}`, `session{id,kind}`,
  `device{id,generation}`, `ws{principal,remote}`, `lease{resource}`.
- Log change creation, abort, completion, lease conflicts, session attach/detach,
  resource add/remove, worker faults, supervision restarts, persistence
  degradation, recovery attempts, and shutdown.
- Never log firmware bytes, serial/RTT data, tokens, token hashes, or
  Authorization headers.
- Audit events mirror the current control-operation logging with actor ID,
  operation, resource, change/session ID, and outcome.
- `GET /v1/server` exposes:
  - version, protocol version, uptime, health (`ok|degraded|draining`);
  - resource counts by kind/state;
  - active/queued changes by status;
  - sessions by kind/state and subscriber counts (attached, max, slow/gap);
  - suspended/resumable sessions and resume failures;
  - session data bytes, retention usage, and log writer queue depth;
  - waiting lease count;
  - active change followers and dropped-update count;
  - WebSocket connection count;
  - per-principal limits (for admin).
- A future `/metrics` exporter can read the same counters. It is not part of
  this design.

## 16. Failure model

| Failure | Detection | User-visible result | Recovery |
| --- | --- | --- | --- |
| Probe unplugged during flash | Worker error or inventory removal | Change `failed` `resource_gone`; lease released | Re-enumerate; next operation retries |
| Probe unplugged during RTT | Worker read error or inventory removal | All subscribers close `4411`; session becomes `suspended`, data replayable | Reconnect device and resume, or replay/close |
| Serial port unplugged | Read/write error | All subscribers close `4411`; session becomes `suspended`, data replayable | Reconnect device and resume, or replay/close |
| Two clients request one probe | Lease conflict | `409 resource_busy`; change may wait | Follow the existing session or abort holder |
| Change actor panic | Supervisor/JoinHandle | Change `failed`; lease released | Client retries with a new change |
| Session actor panic | Supervisor | All subscriber sockets closed; session `suspended`, data replayable | Recovery/manual resume or recreate |
| Subscriber actor panic/slow | Connection task/ring cursor | Only that subscriber closes `4408`/`4409` | Reconnect with `after` or reattach |
| Change follower actor panic/slow | Queue overflow/cursor | Only that follower closes `4408`; change continues | Reconnect with `after` or fresh snapshot |
| Probe worker panic | Thread join/monitor | Resource `faulted`; session `suspended` | Worker respawn/recovery after backoff |
| Lease manager panic | Supervisor | Active sessions suspended/recovered; changes failed | Restart; runtime degraded if repeated |
| Client slow on change socket | Queue overflow | Close `4408` | Reconnect with last `seq` or fresh snapshot |
| Client slow on data socket | Cursor outside retention | `gap`, optionally close `4409` | Resume/replay; other subscribers unaffected |
| Creator disconnects, followers attached | Subscriber count stays > 0 | Session remains active | Creator may reattach; no ownership change |
| Creator token revoked | Revalidation | Only that principal's subscribers close `4401`; session continues if others remain | Reauthenticate and reattach |
| Follower token revoked | Revalidation | Only that principal's subscribers close `4401` | Reauthenticate and reattach |
| Server restart | Process exit | Sockets close `1012`; serial/RTT sessions resume or suspend; DAP/change failures persisted | Clients reconnect/replay; changes are new |
| Disk full or log write failure | SessionLogWriter error | Session `persistence_degraded`, then `suspended`/failed with a gap | Free space; resume or replay retained data |
| Corrupt session segment | Startup scan/read error | Invalid tail truncated; gap mark | Replay retained data; no server-wide failure |
| Shutdown during change | Draining signal | Change failed with `server_shutdown` after grace | Retry after restart |

## 17. Testing

### 17.1 Unit tests

- Actor handle/mailbox: send, reply, timeout, cancelled request, closed actor.
- Thread actor: clean start/stop, cancellation flag, panic containment.
- Lease manager: exclusive acquisition, FIFO queue, cancellation, timeout,
  idempotent release, device removal revocation.
- Change state machine: legal/illegal transitions, abort at each state,
  terminal idempotency, retention.
- Session state machine: starting/active/suspended/resuming/closing/closed, zero-
  subscriber grace, multiple subscribers, access/management policy.
- Data ring: sequence numbers, byte accounting, independent cursors, eviction/gap,
  resume, one slow cursor not affecting another.
- Session log: record framing, CRC, segment rotation, partial-tail truncation,
  durable sequence continuation, gap marks.
- Runtime store: session/change CRUD, retention, purge, recovery queries.
- Change follower: per-change sequence, replay, snapshot fallback, terminal
  close, coalescing.
- Asciicast export: header, relative intervals, output/input/marker/exit events,
  UTF-8 validation, gap markers.
- Protocol serde: golden JSON for every message; unknown fields/enum values.
- Error mapping: runtime errors to HTTP/WS codes.

### 17.2 Actor/runtime tests

- Fake probe and serial backends with scripted delays, progress, cancellation,
  and errors.
- Deterministic runtime: manual clock or very short configured durations.
- Supervisor restart: panic a critical actor and verify affected work failure.
- Resource generation: remove/re-add a fake device and verify stale handles get
  `410`.
- Quota enforcement: global and per-principal session limits; per-session
  subscriber limit; storage retention.
- Recovery: simulate restart with a fake resource present and absent; verify
  active/suspended transitions and data continuity.
- Change restart: simulate a non-terminal change and verify it becomes failed
  with `server_restart`.

### 17.3 HTTP/WebSocket integration tests

- Upload an artifact and flash it; follow the change; observe progress and
  `change.completed`; verify the fake received the exact bytes.
- Abort a flash; verify `cancelling` then `cancelled`, lease released.
- Two flash changes on one probe: first holds, second waits, then runs.
- Two sessions on one probe: second gets `409` with holder; a follower attaches
  to the existing session instead.
- Three subscribers on one serial/RTT session receive the same bytes. One
  subscriber stalls; verify the other two continue and the slow subscriber gets
  `gap` or `4409` without affecting the producer.
- Creator disconnects while followers remain; verify the session stays active and
  followers continue. Last follower leaves; verify grace then close/lease
  release.
- Creator token is revoked while followers remain; verify only the creator's
  subscribers close and the session continues.
- `read` can list session metadata but gets `403`/`4403` on `/io`; `control`
  attaches successfully. A control follower can attach with `mode=read`.
- Two control subscribers can write to the same serial session; verify writes are
  serialized per message. A `mode=read` subscriber is rejected with
  `write_forbidden`, but other control writers still work.
- Serial input is persisted with direction; replay/export can see it.
- DAP second attach returns `409 dap_exclusive`.
- Kick one subscriber; verify only that socket closes.
- Disconnect an RTT data socket; reattach with `after`; verify replay and no
  lost bytes within retention.
- Fill data beyond the live ring; verify replay reads from the durable log.
- Close a session; reconnect with `after=0`; verify full replay and
  `session.ended`.
- Export a UTF-8 serial/RTT session as asciicast v3; verify the header, event
  intervals, and playback-compatible lines. Export binary data and verify `409
  not_asciicast_compatible`, then `?lossy=true` behavior.
- Restart the server with a fake serial/RTT resource still present; verify the
  session resumes, sequence continues, and a downtime gap is recorded.
- Restart with the resource absent; verify the session is `suspended`,
  replayable, and `POST /resume` works when the resource returns.
- Restart with a DAP session; verify it is `failed`, metadata is retained, and
  data replay is unavailable.
- Restart with a non-terminal change; verify it is `failed` with
  `server_restart` and can still be followed.
- Purge session data; verify replay returns `replayable=false` and no bytes.
- Disk write failure: verify `persistence_degraded`/gap behavior and no silent
  lossless claim.
- Change follow: connect, disconnect, reconnect with `after`; verify replay;
  force a gap and verify a fresh snapshot.
- Slow change follower: force queue overflow; verify `4408` then successful
  resume.
- Revoke a token while change/data sockets are open; verify close `4401`.
- Device disappears during a session/change; verify suspended/terminal messages
  and lease release.
- Shutdown: verify draining, `1012`, session suspension/persistence, change
  failure, and clean thread exit.

### 17.4 Manual hardware smoke tests

Real hardware is not required in CI. Keep a small manual matrix for:
`probe-rs` open/attach, flash, reset, RTT scan/read, serial read/write, DAP, and
unplug during operation. Run it on both supported Linux architectures.

## 18. Migration plan

| Phase | Work | Exit criteria |
| --- | --- | --- |
| 0 | Add `boot-protocol`; introduce `ApiState { auth, runtime }`; re-export protocol types; keep all behavior unchanged | Existing tests pass; generated client builds |
| 1 | Actor primitives, inventory watcher, stable resource IDs, `/v1/probes`, `/v1/serial-ports`, `/v1/server`, `/v1/health` | No hardware opened on inventory; IDs/generations stable |
| 2 | Lease manager, probe worker, artifact upload, change manager, `POST /v1/probes/{id}/flash\|reset\|inspect`, `/v1/changes/{id}/follow`, `/v1/artifacts` | Flash is async with progress/abort; client disconnects do not cancel it; follow replay works |
| 3 | Session manager, RTT and serial session actors, multi-subscriber fanout, `POST /v1/probes/{id}/sessions`, `POST /v1/serial-ports/{id}/sessions`, `/v1/sessions/{id}/io`, subscriber list/kick | One handle per serial port; reconnect within grace; multiple followers receive the same stream; conflicts are `409` |
| 4 | Shared DB runtime tables, session log writer/reader, replay API, asciicast v3 export, recovery manager, retention/purge, change history persistence | Sessions resume after restart when the resource is present; closed sessions replay/export; in-flight changes become failed/interrupted |
| 5 | CLI migration: change-follow UI, session resume/replay, `follow` commands, artifact upload; remove old attach/shell paths | CLI has no polling loops or base64 flash; multiple terminals can follow one session |
| 6 | DAP session/worker | DAP works or fails cleanly; no pipe proxy |
| 7 | Remove obsolete handlers/dead code and update docs | Only the resource-scoped v1 surface remains |

Each phase should keep `cargo test --workspace` green and regenerate the
OpenAPI-derived client. Phases 1-3 can land behind an internal runtime flag if a
smaller blast radius is desired.

## 19. Risks and alternatives

### 19.1 Risks

| Risk | Mitigation |
| --- | --- |
| Actor/thread complexity is high for a small tool | Build primitives once, keep them small, and migrate phase by phase; only probe/serial/DAP need threads |
| `probe-rs` cannot cancel a flash mid-call | Model `cancelling` explicitly; guarantee a clean reopen/close after the call; document best effort |
| Detached session buffers consume memory | One shared ring per session (not per subscriber), per-kind byte caps, per-session lifetime cap, global quota |
| A slow follower causes data loss for itself | Explicit `gap` marker and cursor expiry; producer and other followers are unaffected |
| WebSocket proxies idle out connections | Ping/pong every 15 seconds; reconnect/resume in CLI |
| WebSocket schema drift breaks clients | `boot-protocol` types, golden serialization tests, additive-only fields, unknown-value tolerance |
| Worker thread leak on stuck driver call | Join timeout and process exit; log the resource; do not block shutdown forever |
| Auth revocation is not instantaneous with CLI direct DB writes | Periodic revalidation (30 seconds), per-command checks, close on failure |
| Session logs grow faster than retention can evict | Per-session/global byte quotas; oldest terminal sessions purged first; new sessions fail with `429 storage_exhausted` |
| Restart loses the last unsynced session chunks | Periodic flush, explicit gap marks, no claim of lossless replay; clients see the gap |
| Asciicast export misrepresents binary/stream semantics | Native log canonical; UTF-8 compatibility check; explicit `lossy` opt-in; gaps/markers preserved |
| API DB or segment corruption | Truncate invalid tails, quarantine bad segments, keep a backup of `boot.sqlite3`; auth and runtime share the DB, so migrations and permissions are per-file |
| Restart reattaches to the wrong device | Stable resource ID plus generation check; otherwise stay suspended |
| Session logs contain secrets | `0700`/`0600`, retention/purge, documented backup caveat |

### 19.2 Alternatives considered

| Alternative | Why rejected |
| --- | --- |
| One global `Arc<Mutex<HardwareManager>>` | Holds locks across blocking hardware calls, no lifecycle, easy deadlocks, poor testability |
| `spawn_blocking` per request | Exhausts the blocking pool, cannot own a device across operations, aborts are unreliable |
| General actor framework (Actix, Ractor, Kameo) | `!Send` hardware state and blocking calls fit thread actors better; framework semantics add risk for a small tool |
| snapd-style polling/long-polling | Explicit requirement is push; polling adds latency and server load |
| One WebSocket for change updates and all data | Head-of-line blocking; change backpressure would throttle serial/RTT; data resume and change replay have different semantics |
| A global server event stream | Requires subscriptions, global sequencing, fan-out, filtering, replay, and resync without a concrete client need; per-change follow covers the CLI |
| Persist session data as SQLite blobs | High write amplification and poor fit for append-only streams; segment files plus a small metadata DB is cheaper |
| Asciicast as canonical session storage | Loses sequence, gaps, RTT channels, binary data, and session semantics; keep it as export |
| Resume in-flight changes after restart | Hardware/probe state is unknown; automatic reflash is unsafe. Persist history instead |
| Per-endpoint semaphores instead of leases | Prevents some races but provides no sessions, multi-client access, queueing, or reconnect |
| Keep base64 flash bodies | 33% overhead, memory spikes, no progress/cancel; upload streaming is simpler at the edges |
| Model sessions as changes | Forces infinite changes and conflates abort with disconnect; sessions need buffering and resume |
| Oxide Steno for change execution | Single-resource, non-compensable hardware work; Steno's undo/persistent-log/versioning model is a poor fit and adds upgrade/draining complexity |

## 20. Resolved decisions and remaining questions

These were reviewed on 2026-11-09. There are no blocking product questions left;
the remaining items are implementation defaults that can be tuned after
measurement.

| # | Question | Decision |
| --- | --- | --- |
| Q1 | Is `/v1` the right surface, or should we add v2? | Decided: stay on `/v1`; change it in place. |
| Q2 | Do we need a general server event stream? | Decided: defer indefinitely. Per-change follow plus session `/io` covers the CLI. |
| Q3 | Should a revoked token's running change continue? | Decided: cancel at the next safe point. Queued changes are cancelled; a running driver call may finish before the change reaches `cancelled`. |
| Q4 | Should sessions survive a server restart? | Decided: yes for serial/RTT when the resource is still present. DAP fails and has metadata only. The target is never reset automatically. |
| Q5 | Should artifacts be one-shot or TTL-retained? | Decided: TTL-retained (1 hour default) with explicit delete. |
| Q6 | Should other principals be able to attach to a session? | Decided: any `control` principal can attach; `read` can only list metadata. No per-session sharing flag. |
| Q7 | How should change-follow updates be shaped? | Decided: send `change.updated` and `task.updated` individually as they happen, with a full snapshot in `hello` and replay after a gap. |
| Q8 | Should DAP auto-reconnect? | Decided: no. DAP is single-subscriber; close and recreate. |
| Q9 | Polling vs `udev` for inventory? | Decided: poll every 1 second first; add `udev` behind the same interface later. |
| Q10 | Should session create wait for a lease or queue? | Decided: wait up to `wait_ms` (default 5 seconds), then `409`. Changes queue in `waiting`. |
| Q11 | Should we expose a Prometheus `/metrics` endpoint now? | Decided: no; expose counters in `/v1/server`, add an exporter later. |
| Q12 | Should `probe-run --attach` hand off the lease atomically from flash to RTT? | Decided: no. Flash completes and releases; the CLI then creates an RTT session. |
| Q13 | Should idle resource workers stay open? | Decided: yes, for `idle_resource_ttl` (60 seconds), then close. |
| Q14 | How many subscribers should a session allow? | Decided: default 8, hard cap 32; configurable. |
| Q15 | Should `read` tokens be able to follow session data? | Decided: no. `read` lists metadata; session bytes require `control`. A future `monitor` scope can change this. |
| Q16 | Should more than one subscriber be able to write to a serial session? | Decided: yes, always multi-writer. Every attached control subscriber may write; `mode=read` only suppresses its own input. Writes are serialized per message. |
| Q17 | Should Steno be revisited? | Decided: no for now; revisit only if changes become durable, multi-host, or genuinely compensatable. |
| Q18 | Do sessions need an owner for isolation? | Decided: no. Any `control` principal manages a session; `created_by` is audit only. Revisit with a capability model if multi-tenant isolation is needed. |
| Q19 | Should in-flight changes be persisted or resumed? | Decided: persist history and a terminal failure record; never resume execution. |
| Q20 | What are the default retention windows? | Decided: sessions 7 days or 10 GiB; changes 30 days or 5,000. |
| Q21 | Should automatic resume retry indefinitely? | Decided: attempt once at startup, then stay suspended for manual `POST /resume`. |
| Q22 | One `Store` struct or one `Store` trait? | Decided: one struct implementing narrow capability traits. |
| Q23 | Should asciicast be canonical storage or export? | Decided: export only. The native segment log remains canonical. |
| Q24 | Should the CLI keep separate attach and follow commands? | Decided: no. One attach command per resource with `--follow`, `--new`, `--read-only`, and `--session`. |

Implementation defaults that are not blocking:

- segment rotation: 64 MiB
- log flush: 100 ms or 1 MiB
- session ring: serial 256 KiB, RTT 1 MiB
- idle worker TTL: 60 seconds
- default terminal size for asciicast export: 80x24
- asciicast input export: opt-in (`include_input=true`)
- compression: none initially

## 21. References

- snapd REST API overview: <https://forum.snapcraft.io/t/snapd-rest-api/17954>
- snapd OpenAPI and schemas: <https://github.com/canonical/snapd/tree/master/docs/api>
- snapd changes/tasks: `docs/api/v2/paths/changes.yaml`,
  `docs/api/v2/components/schemas/Change.yaml`, and `Task.yaml` in the snapd
  repository
- snapd notices and long-polling: `docs/api/v2/paths/notices.yaml`
- Actors with Tokio, Alice Ryhl: <https://ryhl.io/blog/actors-with-tokio/>
- Tokio channels and `select!`: <https://tokio.rs/tokio/tutorial/channels>
- Erlang/OTP design principles (supervision trees, let it crash):
  <https://www.erlang.org/doc/design_principles/des_princ.html>
- Oxide Steno (distributed sagas): <https://github.com/oxidecomputer/steno>
- Oxide RFD 289, Steno Upgrade (saga versioning and draining):
  <https://rfd.shared.oxide.computer/rfd/0289>
- asciicast v3 specification: <https://docs.asciinema.org/manual/asciicast/v3/>
- asciinema player/CLI: <https://asciinema.org/>
- Existing boot authentication design: [authentication.md](authentication.md)
- Existing release/update design: [release-and-update-design.md](release-and-update-design.md)

## 22. Iteration log

| Date | Revision | Change |
| --- | --- | --- |
| 2026-11-06 | 1 | Initial draft: actor runtime, leases, changes, sessions, WebSocket protocol, `/v1` surface, migration plan |
| 2026-11-06 | 2 | Multi-client follow: one session ring with many independent subscribers, sharing/write policy, subscriber APIs, per-subscriber gap handling; evaluated Oxide Steno and rejected it as a dependency |
| 2026-11-07 | 3 | Removed per-session ownership: `created_by` is audit-only, any `control`/`admin` manages sessions, sharing is `control`/`shared`, and write access is `control`/`none` |
| 2026-11-07 | 4 | Removed the `sharing` field: session metadata is visible to `read`, data attach/management requires `control`/`admin`, and `write_access` remains `control`/`none` |
| 2026-11-09 | 5 | Stay on `/v1`; use resource-scoped creation (`/probes/{id}/flash`, `/probes/{id}/sessions`, `/serial-ports/{id}/sessions`), per-change `changes/{id}/follow`, and defer the general event stream |
| 2026-11-09 | 6 | Added persistence: runtime state DB and append-only session logs, serial/RTT restart recovery, full retained-session replay, terminal change history; in-flight changes never resume |
| 2026-11-09 | 7 | Reuse the existing `boot.sqlite3` for session/change metadata instead of a separate state DB; keep session payloads in segment files and retain the existing journal/permission model |
| 2026-11-09 | 8 | Clarified the Store boundary (one struct, capability traits) and added asciicast v3 export for terminal-compatible sessions while keeping the native segment log canonical |
| 2026-11-09 | 9 | Removed separate CLI follow commands; one attach command per resource with `--follow`, `--new`, `--read-only`, and `--session` modes |
| 2026-11-09 | 10 | Resolved open questions: cancel changes at safe point on revocation, always multi-writer serial, individual change/task updates, input persistence, and recorded defaults |
