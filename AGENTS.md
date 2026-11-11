# Codebase Overview

## Architecture

`boot` is a Rust workspace for flashing, debugging, and observing embedded
targets through a server and CLI.

The server is layered:

1. **Wire protocol** — shared request/response and WebSocket message types.
2. **API adapter** — HTTP and WebSocket handling, authentication, translation
   into runtime operations.
3. **Runtime** — resource ownership, leases, changes, sessions, recovery, and
   policy.
4. **Persistence** — shared metadata storage plus append-only session payload
   files.

The CLI is a client. It can also launch a local server for the local workflow.

```text
CLI / other clients
      │ HTTP commands           │ per-resource WebSockets
      ▼                         ▼
┌───────────────────────────────────────────────┐
│ API adapter                                   │
└───────────────────────┬───────────────────────┘
                        │ runtime handles
┌───────────────────────▼───────────────────────┐
│ Runtime                                       │
│ resources, leases, changes, sessions, recovery│
└───────────────┬───────────────────┬───────────┘
                │ commands          │ events/data
┌───────────────▼────────┐  ┌───────▼────────────┐
│ Probe worker thread    │  │ Serial worker thread│
└───────────────┬────────┘  └───────┬────────────┘
                ▼                   ▼
             USB probe           serial device
```

## Domain Model

- **Resource** — a discovered probe or serial port, with stable identity and
  lifecycle state.
- **Lease** — exclusive permission to use one resource.
- **Change** — finite work such as flash, reset, or inspect. It has tasks,
  progress, logs, cancellation, and a terminal result.
- **Session** — a long-lived stream such as RTT, serial, or DAP. A session has
  one producer/hardware lease and zero or more subscribers.
- **Subscriber** — one client attached to a session with its own cursor.
- **Artifact** — uploaded firmware with a bounded lifetime.
- **Store** — the shared persistence boundary for tokens, runtime metadata,
  and durable state.

Ownership rules:

- Hardware state belongs to one worker thread, never to an HTTP handler.
- A resource has at most one lease.
- A session owns its lease for the whole session, not only while subscribers are
  attached.
- A change owns its lease until it reaches a terminal state.
- Subscribers never own hardware or block the producer.

## Crates

- **`boot-protocol`** — wire contract shared by server and client. It must not
  depend on Tokio, Dropshot, or hardware crates.
- **`boot-api`** — API adapter, runtime, store, and hardware workers.
  - The API layer translates requests and never touches hardware.
  - The runtime owns hardware access, policy, lifecycle, and recovery.
  - The store implements narrow capability traits over one shared database.
  - Blocking hardware operations run on dedicated threads, not async workers.
- **`boot-api-client`** — generated typed HTTP client from the server's OpenAPI
  document.
- **`boot-cli`** — user-facing workflows, rendering, decoding, reconnection,
  and confirmation behavior. It should express intent, not duplicate server policy.

Dependency direction:

```text
boot-cli ──► boot-api-client ──► boot-protocol
    │
    ├────► boot-protocol
    └────► boot-api (for running a local server)
```

## Runtime and Persistence

The runtime uses two execution styles:

- async tasks for control-plane state, coordination, sockets, and fan-out;
- dedicated OS threads for blocking or `!Send` hardware state.

Data flows through bounded queues, rings, and durable logs. Slow consumers must
receive an explicit gap or be disconnected; they must never throttle hardware or
other consumers.

Persistence uses one shared SQLite database for metadata and append-only files
for session payload bytes. Avoid per-chunk database writes, parallel databases,
or compatibility migrations. Live coordination state such as leases and cursors
stays in memory; durable state is restored on startup and reacquired explicitly.

Finite changes and long-lived sessions are distinct resource types and should
stay separate in both API and runtime design.

## Client/Server Interaction

The API is resource-scoped: an operation is created against the resource it acts
on, then observed or attached to as its own change or session. Push traffic is
per-change or per-session rather than a general event bus. Inventory and lists
remain ordinary request/response operations.

Authentication uses scoped bearer tokens. Authorization and ownership policy
belong in the server, not in individual handlers or CLI commands.

## Structural Rules

- HTTP handlers call runtime handles; they never open hardware.
- All hardware access goes through a lease and the owning worker.
- Do not add a second database, migration history, or parallel store layer.
- Prefer per-resource sockets; do not add a global event bus without a concrete
  need.
- Do not add compatibility aliases, duplicate paths, or legacy shims. There are
  no existing deployments.
- Keep sequences monotonic. Missing data is an explicit gap, never a reused
  sequence number.
- Keep protocol types in `boot-protocol`; keep runtime and hardware types out.
- Reuse shared client/session/change helpers instead of reimplementing socket
  loops or lease policy in CLI commands.

## Development Expectations

- `cargo test --workspace` must pass.
- Keep `cargo fmt --check` clean.
- Do not commit generated output or local databases.
- Tests must not require real hardware; keep hardware behind the worker boundary
  and use fakes where practical.
- Hardware smoke tests are a separate manual step.
