# boot CLI Internal Architecture — Design

Status: Draft for approval; review decisions incorporated  
Date: 2026-11-18  
Scope: `boot-cli`, with additions to `boot-protocol` and `boot-api`  
Related: [cli-command-structure.md](cli-command-structure.md),
[api-architecture-design.md](api-architecture-design.md),
[rtt-artifact-decoding-design.md](rtt-artifact-decoding-design.md)

## 1. Summary

The CLI currently works, but each command is a vertical slice that mixes argument
parsing, configuration lookup, API calls, WebSocket framing, progress rendering,
table formatting, prompts, and error wording. The same probe/serial/session
workflow is reimplemented several times, and each copy has slightly different
behavior.

This design splits `boot-cli` into four cooperating layers:

1. **CLI layer** — clap definitions, command metadata, help, and conversion from
   parsed arguments into a terminal-independent `CommandIntent`.
2. **Functional layer** — product workflows. It turns an intent into a `Plan`: an
   ordered list of user-meaningful CLI tasks. It is deterministic, does not
   touch the network, and does not print.
3. **Imperative layer** — executes the plan. It owns ordering, cancellation,
   progress events, retries, and calls capability ports.
4. **I/O layer** — terminal/UI, API transport, local database, systemd, editor,
   and updater adapters. This is where `println!`, `indicatif`, `comfy-table`,
   HTTP, SQLite, and WebSockets live.

Two reusable functions are central to the design:

- `follow_change(...)` follows any change: typed updates, progress, logs,
  cursor-based replay, reconnect, and cancellation.
- `follow_session(...)` follows any session: RTT, serial, replay, and future
  DAP; typed events, cursor-based replay, reconnect/resume, input pumping, and a
  pluggable output sink.

Every command that follows a change or session uses these functions. No command
parses WebSocket frames itself.

The design includes four API additions that remove races and client-side policy
duplication. They land in the same change set:

- atomic "attach or create" session acquisition;
- server-side resource selector resolution;
- idempotency keys for change creation;
- a shared binary session-frame codec in `boot-protocol`.

The rewrite lands as one coordinated change set, not a phased migration.
Structured `--json` output is deferred; typed result views are still introduced
now so structured output can be added later without reworking workflows.

## 2. Decisions at a glance

| # | Decision | Rationale |
| --- | --- | --- |
| C1 | Split the CLI into CLI, functional, imperative, and I/O layers | Each layer has one reason to change; workflows and UI can be tested independently |
| C2 | Parse into a terminal-independent `CommandIntent` | Workflows no longer depend on clap and can later power a TUI or scripted client |
| C3 | Compile intents into a linear `Plan` of user-facing tasks | Gives one execution model, reviewable plans, consistent progress, and `--dry-run` |
| C4 | Use a typed plan builder with slot keys | Planner stays pure and testable; executor gets concrete values without a general workflow engine |
| C5 | Keep the plan linear, not a DAG | Current workflows are sequential; a DAG would be a mini workflow engine with no current benefit |
| C6 | Executor owns effects, ordering, cancellation, and error context | Commands stop hand-rolling retries, spinners, and cleanup |
| C7 | UI is injected; commands never print directly | Enables non-TTY output, tests, a future TUI, and structured output later |
| C8 | One `follow_change` and one `follow_session` in the CLI client layer | Removes duplicated WebSocket loops and makes reconnect/gap behavior uniform |
| C9 | Normalize wire messages into CLI events | Commands see `SessionData`, `Gap`, `ChangeTask`, etc., not wire framing |
| C10 | Session output goes to stdout; progress and diagnostics go to stderr | Makes `boot attach-* > log` work and keeps pipelines clean |
| C11 | Add atomic attach-or-create to the API | Removes the list-then-create race and gives `--follow`/`--new` reliable semantics |
| C12 | Add server-side selector resolution | Resource matching rules live in one place and ambiguous selectors become explicit |
| C13 | Add idempotency keys to change creation | Retrying after a lost response does not flash twice |
| C14 | Add a shared session binary-frame codec to `boot-protocol` | Server and client stop assuming 8/9-byte headers independently |
| C15 | Rewrite in one coordinated change set with no compatibility shims | Unreleased software; there is no need to keep old and new command paths alive side by side |
| C16 | Defer user-facing `--json`; keep typed views internal | Typed views make tables and tests cleaner now and make structured output a later rendering change |

## 3. Why change

### 3.1 Current shape

`boot-cli/src/main.rs` parses a flat clap command and dispatches directly to a
module function:

```text
clap Command ──► main match ──► <module>::<function>(args, cli_db, api_db)
```

Each module function owns the whole command:

```text
args
  ├─ open CLI DB / resolve server
  ├─ validate selectors and flags
  ├─ make HTTP calls
  ├─ open WebSocket
  ├─ decode protocol frames
  ├─ print progress
  ├─ print results
  ├─ prompt
  └─ map errors to strings
```

That works for a small command set, but it does not scale in command count,
output modes, progress quality, or session behavior.

### 3.2 Current CLI inventory

| Area | Current location | Current behavior |
| --- | --- | --- |
| Parsing and dispatch | `main.rs` | One flat `Command` enum and a hand-written match; every handler receives raw DB paths |
| Help | `help.rs` | Pre-parse interception, static overview/focused group registry |
| Server lifecycle/config | `server.rs` | Local DB, systemd, provisioning, auth probing, table printing, editor flow |
| Token administration | `admin.rs` | API and direct-DB modes, confirmation, token tables, expiry parsing |
| Probes/firmware | `probe.rs` | List, resolve, upload, flash, follow change, attach, serial-shell recursion |
| Serial | `serial_port.rs` | List, resolve, session attach, stdin pump, binary output |
| Sessions | `session.rs` | List, replay WebSocket loop, export HTTP stream |
| Update | `update.rs` | GitHub release fetch, download, checksum, binary replacement, rollback |
| Client/auth helpers | `auth.rs` | HTTP client construction, token lookup, generic error mapping |
| CLI database | `cli_db.rs` | SQLite schema, server entries, token storage |
| systemd | `systemd_service.rs` | `systemd-run`/`systemctl` calls and parsing |

### 3.3 Problems this design must solve

| # | Problem | Consequence |
| --- | --- | --- |
| P1 | Commands mix UI, policy, transport, and persistence | Small UX changes require edits across every module |
| P2 | `split_server_resource` and selector parsing are duplicated | Error messages and accepted forms drift |
| P3 | Probe/port/session resolution lists all resources and matches client-side | Races, O(n) traffic, and matching rules duplicated from the server |
| P4 | The "find active session, otherwise create" flow is open-coded twice | Race between list and create; `--follow`/`--new` behavior differs |
| P5 | `upgraded_to_websocket` is copied three times | Framing and upgrade behavior drift |
| P6 | Change follow is hand-rolled only in `run-firmware` | Other change commands cannot reuse progress/reconnect |
| P7 | Session follow is hand-rolled in attach, replay, and serial paths | Reconnect, gaps, binary frames, and lifecycle states differ per command |
| P8 | `attach-probe` ignores binary frames; replay strips assumed header sizes | Protocol framing is implicit and fragile |
| P9 | `run-firmware` reads the whole ELF into memory and clones it | Avoidable memory use; upload cannot stream or report true progress |
| P10 | `run-firmware` ignores task progress and change logs | Users see only a spinner and a final status |
| P11 | Only `run-firmware` has progress; lists and exports have none | Inconsistent UX on slow networks and large exports |
| P12 | Dozens of `println!`/`eprintln!` sites and multiple table builders | No single output path, no stdout/stderr discipline, and no clean seam for future structured output |
| P13 | Errors are `anyhow` strings built ad hoc | Inconsistent guidance; auth mapping uses string matching |
| P14 | List commands report per-server errors but still exit 0 | Scripts cannot tell that nothing succeeded |
| P15 | No reconnect, cancellation policy, or gap reporting in follow paths | A dropped socket looks like a session ending |
| P16 | No tests for plan/execute/follow behavior | Every refactor risks subtle workflow regressions |
| P17 | `show-server` performs raw HTTP health/probe/token calls | Duplicates the typed client and cannot report structured status |
| P18 | `confirm` lives in `admin.rs` but is used by `update.rs` | Module boundaries are accidental |
| P19 | `export-session` writes directly to the destination file | A failed export leaves a partial file |
| P20 | `start-dap` is visible but always fails | Command discovery promises something that does not exist |
| P21 | `boot-cli` depends on `boot-api` with the `run` feature for `--direct` and server startup | The client binary pulls server/hardware code; direct admin is coupled to server types |

## 4. Goals and non-goals

### 4.1 Goals

- One reusable implementation for following any change.
- One reusable implementation for following any session.
- A plan/task/execute model with consistent progress for every long-running
  command.
- A functional layer that can be unit-tested without a terminal, network, or
  hardware.
- A UI layer that supports human and non-TTY output without changing workflows,
  with typed views ready for structured output later.
- Clear separation between product policy (workflows), effects (executor), and
  presentation (UI).
- Better session UX: cursor-based replay, explicit gaps, reconnect, resume, and
  clear detach/ended behavior.
- Better firmware UX: streaming upload, task-level progress, logs, and clear
  partial-success reporting.
- Better error UX: typed errors, one API error mapper, actionable guidance, and
  predictable exit codes.
- Keep command names and user-facing workflows from
  [cli-command-structure.md](cli-command-structure.md) unless a change is
  explicitly listed here.
- Keep the CLI usable without a TTY and keep output clean enough to pipe,
  without committing to a structured-output flag in this pass.

### 4.2 Non-goals

- Changing the flat verb-noun command grammar.
- Adding nested command groups.
- A full TUI, shell, or REPL. The layering should allow one later.
- A general workflow engine, DAG scheduler, or expression language.
- Moving all policy to the server. Local config, update, server lifecycle, and
  interactive session I/O remain client responsibilities.
- Implementing the DAP backend. `start-dap` remains a separate product decision.
- Implementing a PTY or full terminal raw-mode emulation for serial. The design
  leaves a seam for it.
- Adding user-facing `--json`/structured output in this pass. Typed views are
  introduced now; the flag and serializers come later.
- Extracting token store/auth into a new shared core crate. `--direct` is
  isolated behind a local adapter in this pass.
- Changing authentication scopes or token formats.
- Splitting the `boot` binary into separate client/server binaries. This is noted
  as a boundary concern, not part of this design.
- Preserving compatibility with unreleased behavior. There are no deployments,
  so corrections land directly rather than behind aliases.

### 4.3 User-visible changes to expect

Most commands keep their names and arguments. The intended UX changes are:

- progress and diagnostics move to stderr; command data and session bytes stay
  on stdout;
- long commands show CLI tasks and nested server tasks instead of one spinner;
- session attach/replay reconnects when possible and reports gaps explicitly;
- attach/create no longer races when two clients try the same resource;
- `--dry-run` is available on every command and shows what it would do;
- `list-sessions` defaults to active sessions; `--all` and `--past` cover
  retained history;
- batch commands report partial failures and exit non-zero if any requested
  server fails;
- `start-dap` stays visible and continues to return the current
  "not implemented" error until the backend exists;
- `--json` is not added in this pass; typed views prepare for it later.

These decisions are now part of the proposal.

## 5. Influences

### 5.1 Backend architecture

The CLI design follows the same separation that
[api-architecture-design.md](api-architecture-design.md) uses for the server:

| Server concept | CLI counterpart |
| --- | --- |
| API adapter translates HTTP/WS into runtime operations | CLI layer translates clap into intents; UI adapter renders events |
| Runtime owns policy, leases, changes, sessions | Functional layer owns workflow policy and plan construction |
| Worker threads perform effects | Executor owns effect ordering and calls capability ports |
| Per-resource WebSockets, not a global event bus | One follow connection per change/session; no global CLI event bus |
| Bounded queues and explicit gaps | Follow functions track cursors and report gaps explicitly |
| Narrow capability traits over one store | Narrow capability ports over one API client/local store |
| Change/task model with terminal states | Plan task states with terminal outcomes and partial-success reporting |

The important borrowed idea is not the actor runtime. It is the discipline of
separating **policy**, **effects**, and **presentation**, with a typed model at
each boundary.

### 5.2 CLI prior art

| Project | What to borrow | What not to copy |
| --- | --- | --- |
| Cargo | `ops` functions separated from clap; a `Shell`/console abstraction for output, progress, color, and verbosity; structured `--message-format` output | Cargo's workspace/config complexity |
| Docker CLI / GitHub CLI | Injected `Streams`/`IOStreams` and a CLI factory; commands depend on interfaces, not globals | Large command tree and flag surface |
| Terraform | Explicit plan/apply split, reviewable plan, machine-readable plan output | Resource diff graphs and provider machinery |
| Nix | Build plan display, `--dry-run`, structured output | Evaluation language and derivation graph |
| rustup | Terminal/progress abstraction with non-TTY fallback; distribution operations separated from commands | Toolchain-specific model |
| jj / uv | `Ui`/printer abstraction and typed command errors in a modern Rust CLI | Their domain-specific workspace models |
| snapd / snap | Change/task status vocabulary and follow semantics | Polling-based notification |

The recurring pattern is: **commands are thin; operations are testable; I/O is
injected; output is a renderer, not a side effect scattered through the
operation.**

## 6. Target architecture

### 6.1 Layering

```text
                    argv
                     │
                     ▼
┌──────────────────────────────────────────────────────────┐
│ CLI layer                                                │
│  clap definitions, help, defaults, flag conflicts        │
│  parse -> CommandIntent                                  │
└───────────────────────────┬──────────────────────────────┘
                            │ intent
┌───────────────────────────▼──────────────────────────────┐
│ Functional layer                                         │
│  one workflow per intent                                 │
│  intent -> Plan (ordered CLI tasks)                      │
│  pure: no printing, no network, no filesystem writes     │
└───────────────────────────┬──────────────────────────────┘
                            │ plan
┌───────────────────────────▼──────────────────────────────┐
│ Imperative layer                                         │
│  PlanExecutor: order, cancellation, progress, retries    │
│  task action -> capability port calls                    │
└───────────────┬───────────────────────────────┬──────────┘
                │ ports                         │ events
┌───────────────▼──────────────┐   ┌────────────▼──────────┐
│ I/O adapters                 │   │ UI adapter            │
│  API client + follow         │   │  progress, tables,    │
│  local CLI DB                │   │  views, prompts,      │
│  systemd/editor/updater      │   │  session sink         │
└───────────────┬──────────────┘   └────────────┬──────────┘
                │                               │
                ▼                               ▼
          server / local OS                 stdout/stderr
```

Conceptually, `command` + `ui` are the user-facing CLI/UI layer. They are split
into two modules so parsing and rendering can be tested independently, and so
the functional/imperative layers never depend on a terminal.

### 6.2 Dependency rules

```text
command ──► workflow ──► plan
                │
                ▼
             domain
command ──► app ──► executor ──► ports
                         │
                         └────► ui
ports impl ──► boot-api-client, cli_db, systemd, updater
ui impl ──► console, indicatif, comfy-table
```

Rules:

- `workflow` must not depend on clap, `println!`, `reqwest`, `sqlx`,
  `indicatif`, or `comfy-table`.
- `executor` must not depend on clap. It may depend on `plan`, `domain`, and
  capability traits.
- `ui` must not depend on API/client types except protocol DTOs needed for
  rendering session events.
- `ports` may depend on `boot-api-client`, `boot-protocol`, `cli_db`, and
  `systemd_service`.
- No layer calls back upward. The CLI layer may call `app`; `app` calls
  `workflow`, `executor`, and `ui`.
- Shared wire types remain in `boot-protocol`; CLI-only request/plan types stay
  in `boot-cli`.

### 6.3 Layer responsibilities

| Layer | Owns | Must not own |
| --- | --- | --- |
| CLI (`command`) | clap types, flag conflicts, defaults, help registry, intentional errors, `CommandIntent` conversion | Network, DB, progress rendering, session loops |
| Functional (`workflow`) | Product decisions, plan shape, task titles, task policies, artifact/session policy, warnings | Effects, output formatting, HTTP errors, terminal state |
| Plan (`plan`) | Plan/task/action types, typed slots, builder | I/O, clap |
| Imperative (`execute`) | Task ordering, cancellation, progress events, retries, partial failure, action dispatch | Product decisions, output formatting |
| Ports (`ports`) | Typed access to API/local/system | Plan semantics, rendering |
| Follow (`follow`) | Change/session connection, decoding, cursor, reconnect, resume, input pump | Command flags, table/view rendering |
| UI (`ui`) | Progress, tables/fields/views, prompts, session sink, error display, exit code mapping | API calls, policy |
| Local (`local`) | CLI DB, direct token store, systemd, updater primitives | Command workflows |

### 6.4 Target module layout

```text
boot-cli/src/
  main.rs                    # parse, build context, run app, map exit code
  command/
    mod.rs                   # CLI/Command, global args, dispatch to intent
    help.rs                  # grouped overview/focused help registry
    server.rs                # clap args for server commands
    token.rs                 # clap args for token commands
    probe.rs                 # clap args for probe/firmware commands
    serial.rs                # clap args for serial commands
    session.rs               # clap args for session commands
    system.rs                # clap args for update/openapi
  domain/
    mod.rs                   # selectors, artifact/session policy, intent types
  workflow/
    mod.rs                   # intent -> plan dispatch
    server.rs
    token.rs
    probe.rs
    serial.rs
    session.rs
    system.rs
  plan/
    mod.rs                   # Plan, PlanTask, TaskId, ProgressMode, Action
    builder.rs               # PlanBuilder + typed Key<T>
    action.rs                # action enum and payloads
  execute/
    mod.rs                   # PlanExecutor, ExecutionEvent, Outcome
    actions.rs               # action implementations
  ports/
    mod.rs                   # capability traits + Ports bundle
    api.rs                   # real API adapter
    local.rs                 # CliDb adapter
    system.rs                # systemd/editor/updater adapter
  follow/
    mod.rs                   # follow_change + follow_session
    change.rs
    session.rs
    ws.rs                    # upgrade, frame pump, backoff
    event.rs                 # normalized CLI events
  ui/
    mod.rs                   # Ui, OutputMode, Console
    progress.rs              # indicatif + non-TTY fallback
    render.rs                # table/field renderers; structured output later
    prompt.rs                # confirmation, editor, token input
    session.rs               # SessionSink and input source
    error.rs                 # error presentation + exit codes
  local/
    cli_db.rs                # existing CLI DB
    direct_admin.rs          # --direct token store adapter
    systemd.rs               # existing systemd service helpers
    updater.rs               # existing update mechanics
  error.rs                   # typed CliError
```

The exact file split can change; the boundaries are the design. The important
part is that `workflow/` and `plan/` contain no terminal or transport code.

## 7. Functional model

### 7.1 From clap to intent

Clap types remain in `command/`. They are allowed to express flag conflicts and
basic value parsing. They are not the functional input.

```rust
// command/probe.rs
#[derive(Args)]
pub struct RunFirmwareArgs {
    pub server_and_selector: String,
    pub elf_path: PathBuf,
    #[arg(long)]
    pub no_attach: bool,
    #[arg(short = 's', long = "serial-shell")]
    pub serial_shell: Option<String>,
}

// command -> intent
CommandIntent::RunFirmware(RunFirmware {
    target: parse_resource_selector(&args.server_and_selector)?,
    firmware: args.elf_path,
    attach: if args.no_attach {
        AttachPolicy::None
    } else if let Some(target) = args.serial_shell {
        AttachPolicy::Serial(parse_resource_selector(&target)?)
    } else {
        AttachPolicy::Rtt
    },
})
```

Intent types live in `domain/` and contain normalized values:

- `ResourceSelector { server: String, selector: String }`
- `ServerSelector(String)` (defaults applied in one place)
- `ArtifactSource { Upload(PathBuf), Existing(String), Inherit, Clear }`
- `SessionAcquire { AttachOrCreate, AttachOnly, CreateOnly }`
- `SessionAccess { Read, Write }`
- `FollowCursor { Latest, Start, After(u64) }`
- `AttachPolicy`, `TokenMode`, `ListScope`, etc.

This removes selector parsing from every module and makes errors consistent.

### 7.2 Workflow to plan

A workflow is a deterministic function:

```rust
pub fn plan_run_firmware(intent: RunFirmware) -> Result<Plan, CliError>
```

It may:

- validate semantic combinations that clap cannot express;
- choose task order and visibility;
- choose `AttachOnly`/`CreateOnly`/`AttachOrCreate`;
- attach warnings and confirmation requirements;
- decide what happens on partial success.

It may not:

- open the CLI DB;
- call the API;
- read the firmware file;
- start tasks;
- print anything.

A workflow that needs data at execution time expresses that as a plan task
(`ResolveResource`, `UploadArtifact`, etc.), not as a network call during
planning.

### 7.3 Plan tasks and actions

A plan is linear. Each task is user-meaningful. A CLI plan is not a durable
server change: it is computed for one invocation and never persisted. Durable
`Change` and `Session` records remain server-owned; CLI tasks wrap them.

```rust
pub struct Plan {
    pub title: String,
    pub tasks: Vec<PlanTask>,
    pub warnings: Vec<PlanWarning>,
}

pub struct PlanWarning {
    pub level: Level,
    pub message: String,
}

pub struct PlanTask {
    pub id: TaskId,
    pub title: String,
    pub detail: Option<String>,
    pub action: Action,
    pub progress: ProgressMode,
    pub policy: TaskPolicy,
    pub confirmation: Option<Confirmation>,
}

pub struct TaskPolicy {
    pub visible: Visibility,       // User | Verbose
    pub continue_on_error: bool,   // used by batch collection
    pub cancel: CancelPolicy,      // SafePoint | Immediate | NotCancellable
}

pub struct Confirmation {
    pub prompt: String,
    pub default: bool,
    pub required: bool,
}

pub enum ProgressMode {
    None,
    Spinner,
    Bar { unit: Unit },
    ChildTasks,   // server-side change tasks
    Stream,       // session data or foreground process
}
```

Example plan for `run-firmware`:

```text
Plan: flash firmware and attach
  1. Load server local                         [quick, hidden by default]
  2. Resolve probe local/0483:374b:...         [quick, hidden by default]
  3. Upload firmware.elf                       [bar, bytes]
  4. Flash firmware                            [child tasks]
  5. Attach to RTT and stream output           [stream]
```

The functional plan exposes the user-visible stages. The executor may make
several API calls inside one task (for example, create a change and follow it).
That is deliberate: the plan should read like the operation, not like an HTTP
trace.

Representative actions:

```rust
pub enum Action {
    LoadServer { selector: ServerSelector, out: Key<ServerHandle> },
    ResolveResource { server: Key<ServerHandle>, selector: String,
                      kind: ResourceKind, out: Key<ResourceHandle> },
    Collect { scope: ListScope, request: CollectRequest },
    InspectServer { server: Key<ServerHandle>, include_service: bool },
    UploadArtifact { path: PathBuf, out: Key<ArtifactHandle> },
    RunChange { resource: Key<ResourceHandle>, kind: ChangeKind,
                artifact: Option<Key<ArtifactHandle>> },
    AttachSession { resource: Key<ResourceHandle>, kind: SessionKind,
                    policy: SessionPolicy, artifact: ArtifactRef },
    FollowSession { session: Key<SessionHandle>, options: FollowSessionOptions },
    ExportSession { session: Key<SessionHandle>, format: ExportFormat,
                    out_path: PathBuf },
    UpdateServer { name: String, change: ServerChange },
    CreateToken { target: TokenTarget, request: TokenRequest,
                  save_to: Option<String> },
    RevokeToken { target: TokenTarget, selector: TokenSelector },
    Service { action: ServiceAction },
    PrepareLocalServer { options: ServerOptions, out: Key<LocalServer> },
    RunServerForeground { options: ServerOptions },
    FetchRelease { request: UpdateRequest, out: Key<Release> },
    InstallRelease { release: Key<Release>, options: UpdateOptions },
    OpenEditor { document: EditableDocument, out: Key<EditedDocument> },
}
```

Actions are named after the effect, not the endpoint. `AttachSession` is one
action even though it may load/ensure a session, apply an artifact change, and
then follow it. The follow itself still goes through the reusable function.

An action may return a `View`, write typed slots used by later tasks, stream
data, or return nothing:

```rust
pub enum Outcome {
    Done,
    View(View),
    Streamed { summary: Option<View> },
    Partial { view: View, failures: Vec<CliError> },
}
```

The executor uses the last non-`Done` outcome as the command outcome; if every
task returns `Done`, the result is `Done`. Actions that produce a value for a
later task use a `Key<T>` slot instead. This keeps list and show commands from
needing artificial output keys.

Small supporting enums such as `Visibility`, `CancelPolicy`, `Level`,
`TaskOutcome`, `ChangeTaskUpdate`, and `Unit` live in `plan/` and have no I/O.

### 7.4 Typed outputs and the plan builder

Some tasks produce values used by later tasks. A small typed slot system keeps
the plan linear without a general dataflow engine.

```rust
pub struct Key<T> {
    slot: usize,
    _marker: std::marker::PhantomData<fn() -> T>,
}

pub struct PlanBuilder { /* tasks + slot types */ }

impl PlanBuilder {
    pub fn load_server(&mut self, selector: ServerSelector)
        -> Key<ServerHandle>;
    pub fn resolve_probe(&mut self, server: Key<ServerHandle>, selector: &str)
        -> Key<ResourceHandle>;
    pub fn resolve_serial_port(&mut self, server: Key<ServerHandle>, selector: &str)
        -> Key<ResourceHandle>;
    pub fn upload_artifact(&mut self, path: PathBuf)
        -> Key<ArtifactHandle>;
    pub fn flash(&mut self, probe: Key<ResourceHandle>, artifact: Key<ArtifactHandle>);
    pub fn attach_session(
        &mut self,
        resource: Key<ResourceHandle>,
        kind: SessionKind,
        policy: SessionPolicy,
        artifact: ArtifactRef,
    );
}
```

The workflow for `run-firmware` becomes:

```rust
let server = plan.load_server(selector.server);
let probe = plan.resolve_probe(server, &selector.selector);
let artifact = plan.upload_artifact(intent.firmware);
plan.flash(probe, artifact);

match intent.attach {
    AttachPolicy::None => {}
    AttachPolicy::Rtt => {
        plan.attach_session(probe, SessionKind::Rtt, SessionAcquire::AttachOrCreate,
                            ArtifactRef::Inherit);
    }
    AttachPolicy::Serial(target) => {
        let serial_server = plan.load_server(target.server);
        let port = plan.resolve_serial_port(serial_server, &target.selector);
        plan.attach_session(port, SessionKind::Serial, SessionAcquire::AttachOrCreate,
                            ArtifactRef::None);
    }
}
```

Properties:

- Slots are private and only created by `PlanBuilder`.
- The executor stores slot values by index and only exposes typed retrieval
  through `Key<T>`; a missing producer is an internal plan error.
- The executor validates that a key's producer ran before its consumer.
- There is no expression language, condition language, or loop. If a workflow
  needs a runtime decision, it is encoded in a named action such as
  `EnsureSession` with an explicit acquire policy.
- `--dry-run` can render the plan without executing it.
- Tests can assert plan shape and order.

### 7.5 Execution model

```rust
pub struct PlanExecutor<'a> {
    ports: &'a Ports,
    ui: &'a dyn Ui,
    cancel: CancellationToken,
}

impl PlanExecutor<'_> {
    pub async fn execute(&self, plan: Plan) -> Result<Outcome, CliError>;
}
```

Execution is sequential. For each task:

1. Check global cancellation at a safe point.
2. If the task has a confirmation, ask the UI and stop if declined.
3. Ask the UI to start the task.
4. Run the task's action with a task-level cancellation token.
5. Forward progress/child events to the UI.
6. Record success/failure. Stop unless `continue_on_error`.
7. Finish the task with a duration and outcome.

Events are typed and UI-agnostic:

```rust
pub enum ExecutionEvent {
    PlanStarted { title: String },
    TaskStarted { id: TaskId, title: String },
    TaskProgress { id: TaskId, progress: TaskProgress },
    TaskMessage { id: TaskId, level: Level, message: String },
    ChildTask { id: TaskId, update: ChangeTaskUpdate },
    TaskFinished { id: TaskId, outcome: TaskOutcome },
    PlanFinished { outcome: &'static str },
}
```

The executor does not print. The UI decides whether a task is a spinner, a bar,
a child-task list, or nothing. The final `Outcome` is the last non-`Done` task
outcome.

`View` is the renderable result shape:

```rust
pub enum View {
    Fields(Vec<Field>),
    Table(Table),
    Text(String),
    None,
}
```

`Table` and `Field` are neutral row/field models. `comfy-table`, color, and
terminal width stay in `ui/`. Typed views can later derive `Serialize` when
`--json` is added; no JSON variant is needed now.

`TaskProgress` is CLI progress, distinct from `boot_protocol::Progress`:

```rust
pub struct TaskProgress {
    pub done: u64,
    pub total: Option<u64>,
    pub unit: Unit,
    pub message: Option<String>,
}
```

### 7.6 Capability ports

Ports are narrow capability traits, mirroring the server's store split. The
executor's `Ports` bundle holds one implementation of each.

| Port | Responsibilities |
| --- | --- |
| `Catalog` | Load/list/update local server entries; token lookup |
| `Inventory` | List/resolve probes and serial ports; list sessions |
| `Artifacts` | Stream upload a file; fetch metadata |
| `Changes` | Create flash/reset/inspect; follow a change |
| `Sessions` | Get/ensure/set artifact/resume/close; follow a session; export |
| `Tokens` | Create/list/revoke over API or direct local store |
| `Service` | systemd start/stop/restart/status/bind/executable |
| `Editor` | Open a temp document and return edited text |
| `Release` | GitHub release lookup and download |
| `Installer` | Verify and atomically replace the installed binary |
| `Clock` | Timestamps and testable elapsed time |

The real implementation wraps `boot-api-client::Client`, `CliDb`, and local OS
commands. Tests use fakes that implement only the ports they need.

## 8. Reusable change and session following

### 8.1 `follow_change`

One function follows any change:

```rust
pub struct FollowChangeOptions {
    pub after: Option<u64>,
    pub reconnect: ReconnectPolicy,
    pub abort_on_cancel: bool,
    pub cancel: CancellationToken,
}

pub trait ChangeObserver {
    fn snapshot(&mut self, change: &Change, replayed: bool);
    fn change_updated(&mut self, change: &Change);
    fn task_updated(&mut self, task: &Task);
    fn log(&mut self, entries: &[LogEntry]);
    fn completed(&mut self, change: &Change);
    fn reconnecting(&mut self, attempt: u32, delay: Duration);
    fn history_truncated(&mut self, change: &Change);
}

pub async fn follow_change(
    changes: &dyn ChangeApi,
    change_id: &str,
    options: FollowChangeOptions,
    observer: &mut dyn ChangeObserver,
) -> Result<ChangeOutcome, CliError>;
```

Behavior:

- Connect through the generated `follow_change` client method, then upgrade to
  a WebSocket through one shared helper.
- Decode `ChangeWsMessage` into `ChangeObserver` callbacks.
- Track the sequence cursor. Start from `after` when supplied.
- On an unexpected socket close before the change is terminal, back off and
  reconnect with `after=cursor`.
- On a server "lagged" reset, emit `history_truncated` and continue from the new
  snapshot.
- On a terminal change, return its final `Change`.
- On user cancellation, close the socket. Do not abort the change unless
  `abort_on_cancel` is set.
- If the change is already terminal, deliver the snapshot and return
  immediately.

This function is used by:

- `run-firmware` for flash progress;
- future `reset-probe` and `inspect-probe` commands;
- a future explicit `follow-change` command.

### 8.2 `follow_session`

One function follows any session:

```rust
pub struct FollowSessionOptions {
    pub from: FollowCursor,
    pub access: SessionAccess,
    pub include_input: bool,
    pub reconnect: ReconnectPolicy,
    pub resume: ResumePolicy,
    pub replay: ReplayPolicy,
    pub cancel: CancellationToken,
}

pub trait SessionObserver {
    fn hello(&mut self, session: &Session, cursor: SessionCursor);
    fn state(&mut self, session: &Session);
    fn data(&mut self, event: SessionDataEvent);
    fn input(&mut self, data: &[u8]);
    fn gap(&mut self, gap: SessionGap);
    fn error(&mut self, error: &SessionError);
    fn ended(&mut self, session: &Session);
    fn reconnecting(&mut self, attempt: u32, delay: Duration);
}

pub async fn follow_session(
    sessions: &dyn SessionApi,
    session_id: &str,
    options: FollowSessionOptions,
    input: Option<&mut dyn SessionInput>,
    observer: &mut dyn SessionObserver,
) -> Result<SessionOutcome, CliError>;
```

Behavior:

- Connect to `GET /v1/sessions/{id}/io` with `after`, `mode`, `replay`, and
  `include_input`.
- Send the `hello` snapshot to the observer with cursor metadata.
- Decode text control messages and binary data frames into normalized events.
- Track the latest cursor. Reconnect with `after=cursor`.
- On `session.suspended`, optionally call `resume_session`, then reconnect.
- On `session.closed` or `session.failed`, deliver `ended` and return.
- On `gap`, apply the replay policy:
  - `Strict` returns an error with the missing range;
  - `BestEffort` reports the gap and continues from the new range.
- If access is `Write` and an input source is present, pump input frames while
  data flows. Input never blocks output and restarts after a reconnect.
- On cancellation, close the socket, stop the input pump, and return a clean
  detach outcome.

This function is used by:

- `attach-probe`;
- `attach-serial`;
- `replay-session`;
- the attach stage of `run-firmware`;
- future DAP attach;
- a future live-recording command.

`export-session` remains on the server export endpoint initially because the
server owns asciicast formatting and can stream retained data without a live
subscriber. The same observer/sink abstraction can support live recording later.

### 8.3 Normalized session events

Wire messages are normalized before they reach UI code:

```rust
pub enum SessionDataEvent {
    Raw { channel: u8, data: Vec<u8> },
    RttRecords { channel: u8, records: Vec<RttRecord> },
    RttRaw { channel: u8, data: Vec<u8> },
    RttDecodeError { channel: u8, message: String },
}

pub enum SessionError {
    Protocol(String),
    Decoder { channel: u8, message: String },
    Server { code: String, message: String },
    WriteForbidden,
}

pub struct SessionCursor {
    pub oldest_seq: u64,
    pub latest_seq: u64,
    pub applied_seq: u64,
}

pub struct SessionGap {
    pub from_seq: u64,
    pub to_seq: u64,
    pub dropped_bytes: u64,
}

#[async_trait::async_trait]
pub trait SessionInput {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, CliError>;
}

pub trait SessionSink {
    fn raw(&mut self, channel: u8, data: &[u8]) -> Result<(), CliError>;
    fn rtt_records(&mut self, channel: u8, records: &[RttRecord]) -> Result<(), CliError>;
    fn input(&mut self, data: &[u8]) -> Result<(), CliError>;
    fn status(&mut self, message: &str);
    fn gap(&mut self, gap: &SessionGap);
}
```

`follow_session` takes a `SessionObserver`. The UI's observer implementation
delegates data calls to a `SessionSink` that knows whether to write to stdout, a
file, or a structured event stream. Lifecycle/state handling stays in the
observer; byte rendering stays in the sink.

This prevents each command from pattern-matching protocol details and makes
rendering a single policy:

| Session kind | Human rendering |
| --- | --- |
| RTT | decoded records, or lossy text for raw payloads |
| Serial | raw bytes to stdout |
| Replay | same as the source kind, plus input lines when requested |
| DAP (future) | opaque bytes or a dedicated DAP sink |

### 8.4 Cursor, reconnect, and gaps

The follow functions own cursor semantics:

- A cursor is the highest applied data/update sequence.
- Reconnect asks the server for everything after that cursor.
- If replay is not possible, the server sends a fresh snapshot or a `gap`; the
  observer is told explicitly.
- Sequence numbers are never reused. A gap is always visible, never silently
  filled.
- Reconnect uses bounded, jittered exponential backoff.

Default policies:

| Workflow | Reconnect | Resume suspended | Replay |
| --- | --- | --- | --- |
| Live attach | Until cancelled | Yes | Best effort |
| Run-firmware change follow | Bounded (for example 5 attempts) | N/A | Best effort |
| Replay retained session | None or bounded | No | Strict by default |
| Export retained session | N/A (HTTP stream) | No | Strict |

A failed reconnect must report the change/session ID and a command the user can
run to continue observing it.

### 8.5 Input and output

- Session output is written by a `SessionSink` implemented by the UI.
- For human mode, serial/RTT raw bytes go to stdout.
- For `--out`/export paths, the sink writes to a temp file and renames on
  success.
- Status, decoder changes, reconnect notices, gaps, and detach messages go to
  stderr.
- Interactive input is a separate `SessionInput` abstraction. The real
  implementation reads stdin; tests use a scripted source.
- The follow function never writes directly to process stdout/stderr.
- Terminal raw mode is a future `TerminalMode` seam. The initial implementation
  keeps the current line-oriented input behavior.

### 8.6 Cancellation

`main` installs a single cancellation token for Ctrl+C. Follow functions and
long tasks receive it.

- During `follow_session`, Ctrl+C detaches the subscriber. The session stays
  alive for its detach timeout. Print that clearly.
- During `follow_change`, Ctrl+C stops following. The change continues on the
  server. Print the change ID and how to follow it again.
- `--abort` on a change-following command requests cancellation of the change.
- During binary replacement in `update`, cancellation is only honored at safe
  points; the replacement/rollback section is not interruptible.
- `run-server` is exempt from this policy: the server owns its shutdown signals
  and graceful drain.
- A second Ctrl+C exits immediately.

### 8.7 Placement of low-level WebSocket helpers

Reconnect policy, cursors, and CLI observers stay in `boot-cli/src/follow/`.
The pure frame pump — upgrade, read/write a tungstenite stream, encode/decode
protocol frames — can live either in `boot-cli/src/follow/ws.rs` or in a
hand-written `boot-api-client::ws` module, as suggested by the API architecture
document. The rule is:

- `boot-api-client` may own transport mechanics shared by non-CLI clients;
- `boot-cli/follow` owns reconnect policy, resume policy, cursors, and CLI
  events;
- `boot-protocol` owns wire types and the binary frame codec;
- `boot-cli/ui` owns rendering.

Do not put progress bars or terminal behavior in `boot-api-client`.

## 9. UI and output model

### 9.1 stdout and stderr

| Stream | Contents |
| --- | --- |
| stdout | Command result: tables/fields, session bytes, exported data when written to stdout |
| stderr | Progress, warnings, prompts, logs, reconnect notices, diagnostics |

This is a behavior change from today, where attach status and progress share
stdout with session data. It is required for pipes and scripts.

### 9.2 Progress

`ui::Progress` wraps `indicatif` but exposes UI-neutral operations:

```rust
pub trait ProgressUi {
    fn task_started(&self, id: TaskId, title: &str, mode: ProgressMode);
    fn task_progress(&self, id: TaskId, progress: TaskProgress);
    fn task_message(&self, id: TaskId, level: Level, message: &str);
    fn child_task(&self, id: TaskId, update: ChangeTaskUpdate);
    fn task_finished(&self, id: TaskId, outcome: &TaskOutcome);
    fn enter_stream(&self);
    fn exit_stream(&self);
}
```

Rules:

- A TTY gets spinners, bars, and nested child tasks.
- A non-TTY gets line-based task start/finish messages on stderr, with no
  carriage-return animation.
- Progress updates are coalesced to about 10 Hz; terminal updates are always
  shown.
- Only one task is active at a time in the initial executor, so rendering stays
  simple.
- `enter_stream` clears/finishes progress before raw session bytes are written.
- `NO_COLOR` and `--color auto|always|never` are honored.
- `-q`/`--quiet` suppresses progress and non-error messages.

### 9.3 Plans, dry runs, and progress display

Plans are not printed by default. The UI shows tasks as they execute, similar to
Cargo. `--dry-run` prints the full plan, including tasks normally hidden as
"quick", and exits without effects:

```text
$ boot run-firmware --dry-run local/0483:374b:... firmware.elf
Plan: flash firmware and attach
  1. Load server local
  2. Resolve probe local/0483:374b:...
  3. Upload firmware.elf (128 KiB)
  4. Flash firmware (reset, verify)
  5. Attach to RTT and stream output
No changes made.
```

Normal execution:

```text
$ boot run-firmware local/0483:374b:... firmware.elf
✓ Connected to local
✓ Firmware uploaded (128 KiB, sha256 9f2c...)
⠋ Flash firmware
  ✓ validate
  ⠋ program  48%  [██████········]
  · verify
  · reset
✓ Firmware flashed in 2.3s
→ Attaching to RTT session ses_01J...
  decoder: defmt (art_01J..., sha256 9f2c...)
...RTT output...
```

On failure, the UI reports the failed task, server error, and whether earlier
tasks had side effects:

```text
✗ Flash firmware failed in task `program`
  probe disconnected (resource_gone, retryable)
  artifact art_01J... remains available
  no session was created
```

### 9.4 Structured output (deferred)

User-facing `--json`/NDJSON output is deferred. The architecture still replaces
direct `comfy_table` construction in command modules with typed views and shared
renderers, so adding structured output later is a rendering change rather than a
workflow rewrite.

Design for the later step:

- Workflows/actions produce typed result data where practical, and the UI
  renders it.
- Human renderers convert result data to tables or fields; they do not decide
  policy.
- A later structured-output pass can add a renderer and, if needed, carry typed
  results through the execution outcome.
- Streaming commands will need an explicit NDJSON policy later.

Typed views look like this:

```rust
pub struct ProbeRow {
    pub server: String,
    pub id: String,
    pub selector: String,
    pub name: String,
    pub firmware: Option<String>,
}

pub struct ProbeListView { pub probes: Vec<ProbeRow> }
```

### 9.5 Prompts

Prompts are a UI concern:

- `confirm(prompt, default)` for destructive actions;
- token source reading (`--token-stdin`, `--token-file`, `--token`);
- editor flow for `edit-server`;
- no prompt is shown when stdin is not a TTY unless the operation is explicitly
  confirmed by a flag.

### 9.6 Errors and exit codes

One typed error family:

```rust
pub enum CliError {
    Usage(String),
    Config(String),
    Transport { server: String, source: Box<dyn Error> },
    Api { server: String, status: Option<StatusCode>, code: Option<String>, message: String },
    Cancelled,
    TaskFailed { task: TaskId, source: Box<CliError> },
    Internal(anyhow::Error),
}
```

Rules:

- `main` maps errors to human output through the UI. A structured error path can
  be added with the later structured-output work.
- One API error mapper handles 401, 403, 404, 409, and transport failures, with
  operation context. It must not rely on matching message substrings.
- Errors include the server name and the failed task.
- Exit codes: `0` success, `1` operational failure, `2` usage, `130`
  interrupted.
- Batch commands print successful rows and report failures on stderr. Per the
  review decision, any requested server failure makes the command exit non-zero.
  An `--allow-partial` escape hatch can be added later if scripts need it.

## 10. Command mapping

### 10.1 Existing handler to workflow mapping

| Current handler | Target workflow/action |
| --- | --- |
| `probe::list_probes` | `workflow::probe::list` + `Collect` |
| `probe::run_firmware` | `workflow::probe::run_firmware` + `UploadArtifact`/`RunChange`/`AttachSession` |
| `probe::attach_probe` | `workflow::probe::attach` + `ResolveResource`/`UploadArtifact`/`AttachSession` |
| `probe::start_dap` | `workflow::probe::start_dap` (blocked until backend) |
| `serial_port::list_serial_ports` | `workflow::serial::list` + `Collect` |
| `serial_port::attach_serial` | `workflow::serial::attach` + `ResolveResource`/`AttachSession` |
| `session::list_sessions` | `workflow::session::list` + `Collect` |
| `session::replay_session` | `workflow::session::replay` + `FollowSession` |
| `session::export_session` | `workflow::session::export` + `ExportSession` |
| `admin::create_token` | `workflow::token::create` + `CreateToken` |
| `admin::list_tokens` | `workflow::token::list` + `Collect` |
| `admin::revoke_token` | `workflow::token::revoke` + `RevokeToken` |
| `server::start_server` | `workflow::server::start` + `PrepareLocalServer`/`Service` |
| `server::run_server` | `workflow::server::run` + `RunServerForeground` |
| `server::list_servers` | `workflow::server::list` + `Collect` |
| `server::show_server` | `workflow::server::show` + `InspectServer` |
| `server::add_server` | `workflow::server::add` + `UpdateServer` |
| `server::edit_server` | `workflow::server::edit` + `UpdateServer`/`OpenEditor` |
| `server::remove_server` | `workflow::server::remove` + `UpdateServer` |
| `update::update` | `workflow::system::update` + `InstallRelease` |
| `show_openapi` | `workflow::system::openapi` + `Collect`/`View` |

The current function names disappear, but the user-facing command names do not
change.

### 10.2 Overview

| Command | Workflow plan | Key API/OS calls | Output |
| --- | --- | --- | --- |
| `start-server` | prepare local config, start service, show server | CLI DB, API DB/store, systemd | server view |
| `stop-server` | stop service | systemd | status view |
| `restart-server` | restart service | systemd | status view |
| `run-server` | foreground server (special passthrough) | `boot_api::run` | server logs |
| `list-servers` | collect local entries | CLI DB | server table |
| `show-server` | load server, inspect service, check auth | CLI DB, health/probe/token endpoints, systemd | field view |
| `add-server` | read token, add entry | CLI DB | server view |
| `edit-server` | update fields or edit document | CLI DB, editor | server view |
| `remove-server` | warn, remove entry | CLI DB | status view |
| `create-token` | create via API or direct store, maybe save | token API or direct store, CLI DB | token + record |
| `list-tokens` | collect per server or direct | token API or direct store | token table |
| `revoke-token` | confirm, revoke one/all | token API or direct store | status view |
| `list-probes` | collect probes | inventory API | probe table |
| `run-firmware` | load server, resolve probe, upload, flash, optionally attach | inventory, artifacts, changes, sessions | progress + stream |
| `attach-probe` | load server, resolve probe, ensure session, follow | inventory, sessions, artifacts | stream |
| `start-dap` | load server, resolve probe, ensure DAP, follow | inventory, sessions | stream (blocked until backend exists) |
| `list-serial-ports` | collect ports | inventory API | port table |
| `attach-serial` | load server, resolve port, ensure session, follow with input | inventory, sessions | stream |
| `list-sessions` | collect sessions with filter | session API | session table |
| `replay-session` | load server/session, follow with replay cursor | session API | stream |
| `export-session` | load server/session, export to temp file, rename | session export API | path view |
| `update` | fetch release, check, confirm, install, maybe restart | GitHub API, installer, systemd | progress + result |
| `show-openapi` | render generated spec | local `boot_api::openapi` | text |
| `help-*` | CLI/help layer, not a plan | none | text |

### 10.3 `run-firmware`

Current behavior:

- reads the entire ELF into memory;
- uploads it;
- starts a flash change;
- opens change-follow by hand;
- updates one spinner from `Hello`/`TaskUpdated`/`ChangeCompleted`;
- ignores change logs and task progress percentages;
- calls `attach_probe` or `attach_serial` directly, bypassing shared setup.

Target behavior:

```text
1. Load server
2. Resolve probe
3. Upload firmware (streamed, progress bar)
4. Flash firmware (create change + `follow_change`, child task progress/logs)
5. If `--no-attach`: finish
   Else if `--serial-shell <target>`:
     Load target server, resolve port, attach serial session, follow with input
   Else:
     attach RTT session (create or reuse), follow
```

Important semantics:

- Flash success and attach failure are reported separately. The command exits
  non-zero if attach fails, but states that flashing succeeded.
- The session inherits the probe's last flashed artifact by default, because the
  flash updates it before the attach step.
- The implicit attach stage uses attach-or-create. The current code implicitly
  passed `--new`; after a successful flash there should be no active RTT session
  anyway, and attach-or-create is friendlier if one exists.
- Ctrl+C during attach detaches; it does not undo the flash.
- `--no-attach` skips the session tasks entirely and conflicts with
  `--serial-shell`; the command conversion rejects that combination instead of
  silently preferring one.
- `--serial-shell` may target a different server than the flash target.

### 10.4 `attach-probe`

Target plan:

```text
1. Load server
2. Resolve probe
3. If `--elf`: upload artifact
4. Ensure RTT session:
   - explicit `--session`: load and validate kind/resource;
   - `--follow`: attach only;
   - `--new`: create only;
   - default: attach or create atomically;
   - apply `--artifact`/`--clear-artifact`/inherit policy.
5. Follow the session (read/write mode, resume on suspend)
```

- `--read-only` sets follow access to read.
- `--session` conflicts with `--follow` and `--new`; the command conversion
  rejects the combination.
- `--session` no longer silently attaches to the wrong kind; the ensure action
  validates it.
- The command reports whether the session was reused or created.
- The attach status and decoder label go to stderr; RTT output goes to stdout.
- If the session ends or the server fails, report the reason and exit
  appropriately.

### 10.5 `attach-serial`

Target plan:

```text
1. Load server
2. Resolve port by ID or path
3. Ensure serial session (attach/create policy from flags, baud rate)
4. Follow serial session with stdin input and resume-on-suspend
```

- `--follow` attaches only.
- `--new` creates only.
- Default attaches or creates.
- `--read-only` disables input. Whether `--follow` should imply read-only by
  default (an existing command-structure follow-up) is a workflow policy change,
  not an architecture change; the plan carries access explicitly.
- Raw serial bytes go to stdout; detach/status messages go to stderr.
- On detach, state that the session stays alive for its detach timeout.
- The same `attach_session` action is used by probe, serial, and firmware
  workflows. Only the resource kind and input policy differ.

### 10.6 `replay-session` and `export-session`

`replay-session`:

```text
1. Load server
2. Load session
3. Follow session with cursor `After(--from)`, access Read, no input
4. Render events through the replay sink
```

- No reconnect by default; a retained session replay should fail clearly if the
  cursor is outside retention.
- `--include-input` controls `Input` event rendering.
- Uses the same follow function as live attach, with a different sink and
  policy.

`export-session`:

```text
1. Load server
2. Load session
3. Export to a temp file next to the destination, with progress
4. Rename into place on success
5. Report the final path
```

- No partial file on failure.
- `--format` stays asciicast-v3 for now.
- Server-side export remains separate from follow because it is a retained-data
  formatting path.

### 10.7 List commands

All list commands use a `Collect` action with a scope:

```text
scope: AllServers | OneServer(name) | Direct
filter: probes | serial-ports | sessions(select) | tokens | servers
```

- Collection across multiple servers may run concurrently with a small bound
  (for example four).
- Each server failure becomes a warning/error row; successful results are still
  rendered, and any requested server failure makes the command exit non-zero.
- Views include a `server` field per row when collecting from multiple servers.
- `list-sessions` gains `--active`, `--past`, and `--all`. The default is
  `active`; `--all` includes retained history and `--past` shows only history.
  Update the command help text to say so.
- `list-probes` and `list-serial-ports` gain `--server` filters.
- Resolution and filtering use server-side APIs where available.

### 10.8 Server and local configuration

`start-server` plan:

```text
1. Prepare local server (validate bind/TLS, provision entry and token)
2. Start systemd unit
3. Load and render server entry
```

`run-server` is a special foreground command:

- It still uses the same context, error mapper, and UI, but it does not show a
  multi-task progress display.
- Server logs pass through to stderr.
- It owns signal handling and graceful shutdown.
- `prepare` provisioning remains one imperative action.

`show-server`:

```text
1. Load server entry (local entry may be missing)
2. For local, inspect systemd status and bind
3. Check health, read scope, and admin scope with typed requests
4. Render a field view
```

- Unreachable server is reported, not treated as a command failure.
- Missing local entry is reported clearly.
- The status fields are a typed view, ready for structured output later.

`edit-server`:

- Flags mode: validate URL/CA/token, update entry, render.
- No-flags mode: open a `0600` temp YAML document in `$EDITOR`, parse, validate,
  compare, and update. Temp-file cleanup stays in the editor port.

### 10.9 Token administration

- Workflows choose API vs direct store based on `--direct`.
- The direct store is a separate `TokenAdmin` port implemented in
  `local/direct_admin.rs`, using the existing `boot_api::store`/`auth` internals.
  This pass does not extract a shared core crate; the port keeps command code
  clean and leaves a clear seam for that later.
- The workflow owns mode validation (`--api-db` requires `--direct`, `--all`
  conflicts, etc.).
- The executor owns confirmation, API calls, and optional `--save-to`.
- `create-token` still prints the secret to stdout and metadata to stderr.
- `revoke-token` still checks whether the caller is revoking its own current
  token; that becomes a typed action, not a string-matching helper.
- Error mapping for missing admin scope is centralized.

### 10.10 `update`

`update` is a good plan showcase:

```text
1. Fetch release
2. Compare installed and available versions
3. Confirm replacement
4. Download binary and checksum
5. Verify SHA-256 and binary version
6. Replace the installed binary atomically
7. Restart the service if needed
```

- The installer action owns backup and rollback on restart failure.
- Rollback is not modeled as general plan undo; it is an invariant of the
  `InstallRelease` action.
- `--check` stops after step 2.
- `--no-restart`, `--force`, `--version`, and `--repo` map to the same actions.
- Progress uses bars for download and spinners for verification/replacement.

### 10.11 Help and special commands

- `help`, `--help`, and `help-<object>` remain in the CLI/help layer.
- The static group registry remains the single source of truth for grouped help.
- Tests continue to assert that every operational command appears exactly once
  in the overview and that focused groups cover every object command.
- `show-openapi` is a one-task render command.
- `start-dap` stays visible in help and command discovery. Its plan remains a
  no-op until the backend exists; the executor surfaces the API's current
  "not implemented" error.

## 11. Proposed API changes

All four additions below are in scope for the single change set. The first two
remove races and duplicated client policy; the other two improve robustness.
Optional additions remain deferred.

### 11.1 Atomic attach-or-create session

Current flow:

```text
CLI lists active sessions
  ├─ found: use it
  └─ not found: POST create
```

Two clients can both see "not found" and race. `--follow` and `--new` also
depend on the same list.

Proposed request field on both session-creation endpoints:

```json
{
  "kind": "rtt",
  "acquire": "attach-or-create",
  "options": {},
  "artifact": "art_...",
  "inherit_artifact": true,
  "max_subscribers": 8,
  "detach_timeout_seconds": 60,
  "wait_ms": 5000
}
```

Values:

| `acquire` | Behavior |
| --- | --- |
| `attach-or-create` | Return the active session for the resource/kind, or create one |
| `attach` | Return the active session or `404 session_not_found` |
| `create` | Create a new session or `409 resource_busy` |

Response:

```json
{
  "session": { "id": "ses_...", "...": "..." },
  "created": true
}
```

- The runtime makes the decision under the lease manager, not in the CLI.
- `201` means created, `200` means attached. The body also says which happened
  so clients do not have to infer from status.
- The CLI's `EnsureSession` action maps directly to this endpoint.

### 11.2 Server-side resource selector resolution

Current CLI matching rules are split between probes and serial ports:

- probe ID, `vvvv:pppp:serial`, or probe name;
- serial port ID or device path.

Proposed endpoints:

```text
GET /v1/probes/resolve?selector=<url-encoded>
GET /v1/serial-ports/resolve?selector=<url-encoded>
```

`resolve` is a reserved subpath. Resource IDs are prefixed (`probe_`, `port_`),
so it cannot collide with a real ID. Responses: the resource DTO.

Errors:

| Status | Code | Meaning |
| --- | --- | --- |
| `404` | `not_found` | No matching resource |
| `409` | `ambiguous_selector` | More than one resource matches; body lists candidates |
| `400` | `invalid_selector` | Malformed selector |

Benefits:

- one authoritative matching implementation;
- ambiguity becomes explicit;
- the CLI stops listing every resource to resolve one;
- `attach-*` and `run-firmware` can validate the target before uploading.

The resource ID remains the stable identifier. Selectors are UX aliases.

### 11.3 Idempotency for change creation

Add `Idempotency-Key` to:

- `POST /v1/probes/{id}/flash`
- `POST /v1/probes/{id}/reset`
- `POST /v1/probes/{id}/inspect`

Semantics are already described in the API architecture document:

- scoped to the principal;
- same key and same request returns the original change;
- same key and different request returns `409 idempotency_conflict`;
- retained for a bounded period.

This lets the CLI retry a change-creation request after a lost response without
flashing twice. It is especially valuable with reconnect-aware plan execution.

### 11.4 Shared binary session-frame codec

The server and client currently agree implicitly on binary frame layout:

```text
u64 big-endian seq
u8 channel        (RTT only)
payload bytes
```

`replay_session` strips 8 or 9 bytes by hand; other paths ignore binary frames.
Add a codec to `boot-protocol`:

```rust
pub struct SessionDataFrame<'a> {
    pub seq: u64,
    pub channel: Option<u8>,
    pub data: &'a [u8],
}

pub enum FrameError { TooShort, InvalidChannel }

pub fn decode_session_frame(kind: &str, bytes: &[u8])
    -> Result<SessionDataFrame<'_>, FrameError>;
```

This is not a wire change. It removes duplicated assumptions and gives the
follow function one decoding point.

### 11.5 Optional/deferred API changes

- **List filters**: allow `GET /v1/sessions?resource_id=...&kind=...&select=...`
  to avoid client-side filtering as inventories grow.
- **Change follow gap metadata**: expose a stable "history truncated" marker and
  oldest retained sequence in the hello message.
- **Session attach result**: include a `created` flag even for explicit
  `--session` loads, or keep `get_session` separate.
- **Live recording/export**: add a follow-to-file mode only if users need live
  export; the current export endpoint is sufficient for retained sessions.
- **DAP**: no API change until the backend exists; the CLI plan shape is already
  ready.

### 11.6 Existing API capabilities to use

These already exist and should be used rather than reinvented:

- streaming artifact upload (`POST /v1/artifacts`);
- change follow with `after` replay (`GET /v1/changes/{id}/follow`);
- session I/O with `after`, `replay`, `mode`, and `include_input`;
- session resume (`POST /v1/sessions/{id}/resume`);
- session artifact set/clear;
- session export asciicast v3;
- list filters for sessions (`active`, `past`, `all`);
- typed OpenAPI-generated client.

## 12. Testing strategy

### 12.1 Unit tests

| Area | Tests |
| --- | --- |
| Command conversion | selectors, defaults, flag conflicts, intents |
| Workflow planners | exact plan task order/kind/policy for each flag combination |
| Plan builder | slot allocation and use-before-produce detection |
| Executor | success, failure, partial failure, cancellation, task context |
| Error mapper | 401/403/404/409, transport, missing token, scope guidance |
| View renderers | tables, fields, non-TTY output; structured output deferred |
| Update planner/action | version checks, checksum, rollback decision |

Planner tests require no network, no terminal, and no files. They assert the
plan, not printed text.

### 12.2 Follow tests

Use a scripted stream rather than a real server:

- hello with snapshot and cursor;
- ordered updates and logs;
- terminal completion;
- socket close before terminal, reconnect with `after=cursor`;
- server lagged/reset, snapshot replacement;
- session suspended, resume success, resume failure;
- gap under strict and best-effort policies;
- input pump sends bytes and stops on close;
- cancellation during data flow;
- malformed text and binary frames;
- binary frame codec for RTT and serial.

Tokio paused time makes backoff tests deterministic. No real sleeps.

### 12.3 Executor tests

Fake ports record calls and return scripted results. A `RecordingUi` records
execution events. Tests cover:

- `run-firmware` with `--no-attach`, default RTT attach, and serial shell;
- attach/create/reuse session policy;
- explicit session kind mismatch;
- upload progress;
- change failure after successful upload;
- flash success followed by attach failure (partial success);
- `export-session` temp-file rename and cleanup;
- list collection across servers with one failure and a non-zero exit;
- `list-sessions` defaults to active and honors `--all`/`--past`;
- `start-dap` remains visible and surfaces the not-implemented error;
- direct token admin through the `local/direct_admin.rs` port;
- update rollback.

### 12.4 UI tests

- Non-TTY output contains one line per task and no animation control bytes.
- Session raw output goes to stdout while status goes to stderr.
- `--quiet` suppresses progress.
- Table output has stable columns and widths.
- No user-facing `--json` flag is wired in this pass.

Snapshot testing with `insta` is optional; plain string assertions are enough
for the first pass. If added, snapshots should cover views and plan rendering,
not every progress animation frame.

### 12.5 Integration tests

- Keep existing OpenAPI, auth, store, and runtime tests.
- Add API tests for attach-or-create, selector resolution, idempotency, and the
  session-frame codec.
- Use fake hardware backends; no test requires a real probe or serial port.
- CLI integration tests may run the binary against a test server, but the
  functional/executor tests should carry most of the coverage.

### 12.6 Quality gates

- `cargo test --workspace`
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets` if it is part of the project's gate
- `cargo deny check licenses` when dependencies change

## 13. Delivery plan (single change set)

The rewrite lands as one coordinated change set. There is no phased migration,
no feature flag, and no period where old and new command implementations coexist.
All commands switch before the change is complete.

The order below is an internal working order for implementation sanity. It is
not a release sequence, and none of it is optional.

### 13.1 Build order inside the change

1. Add `CliError`, `OutputMode`, `Console`/UI traits, and a `CliContext`.
2. Add `domain`, `plan`, `plan::builder`, and `execute` types with unit tests.
3. Add `follow_change` and `follow_session`, including cursor tracking,
   reconnect, gap reporting, resume, input pumping, and the shared
   `boot-protocol` binary frame codec.
4. Add capability ports and their real API/local/system adapters.
5. Implement every workflow and switch `main`/dispatch to intent -> plan ->
   executor.
6. Implement the API additions: attach-or-create, selector resolution,
   change idempotency, and the shared frame codec on the server side.
7. Delete the old command handlers, duplicated WebSocket loops, selector
   parsing, table builders, and ad hoc error mappers.
8. Update help examples, tests, and docs, including any changed flags.
9. Run the full workspace gates.

### 13.2 No coexistence rule

- Do not keep old handler functions as fallbacks.
- Do not add aliases, temporary flags, or dual command paths.
- Do not land a partially migrated command list.
- Do not leave a second WebSocket loop, a second selector parser, or a second
  table renderer behind "for now".
- If review needs to be split by area, it is still one logical change set and is
  merged with all commands migrated.

### 13.3 Completion criteria

- Every operational command goes through `CommandIntent` -> `Plan` ->
  `PlanExecutor`.
- No command module opens a WebSocket, parses protocol frames, prints progress,
  or builds a table directly.
- `follow_change` and `follow_session` are the only change/session follow paths.
- Resource selector parsing and API error mapping exist in exactly one place.
- Human and non-TTY output come from UI renderers; typed views are ready for
  later structured output.
- `--direct` token administration goes through `local/direct_admin.rs`, not
  command code.
- All four API additions and the client actions that use them land together.
- `cargo test --workspace` and `cargo fmt --check` pass.
- The old modules and their tests are deleted rather than left dormant.

## 14. Risks and trade-offs

| Risk | Mitigation |
| --- | --- |
| The plan abstraction grows into a workflow engine | Keep it linear, typed, and small; no expressions, loops, or DAGs. Composite actions have explicit names. |
| Too many port traits make testing/DI noisy | Group capabilities by action family; `Ports` bundles them; fakes implement only what a test uses. |
| Planning becomes stale because it cannot query | Resolve/validate/upload are tasks. Planner decisions come from flags and intents, not live state. Runtime decisions live in named actions. |
| Progress rendering fights raw session output | Explicit `Stream` progress mode and `enter_stream`/`exit_stream`; status goes to stderr. |
| Reconnect hides real failures or loops forever | Bounded policies per workflow; explicit reconnect events; session closed/failed always terminal; attach can retry until cancelled. |
| Cancellation semantics surprise users | Document and print that following is not aborting; add `--abort` for changes; safe-point policy for update. |
| Deferred structured output becomes harder to add later | Introduce typed result data now and keep formatting in UI renderers; adding `--json` later should not require rewriting workflows, though it may need a typed-result seam in the executor. |
| API changes and CLI changes land out of sync | All four API additions and their client actions land in the same change set; no fallback path is kept. |
| Direct token admin keeps the CLI coupled to server/hardware code | Isolate behind `local/direct_admin.rs` in this pass; revisit a shared core crate or operator binary later. |
| The new layering adds boilerplate | Accept some boilerplate at boundaries in exchange for testability and consistent UX; avoid a type for every trivial helper. |
| Session input/terminal handling remains limited | Keep `SessionInput`/`TerminalMode` seams; defer raw mode/PTY to a dedicated terminal UX change. |

## 15. Alternatives considered

### 15.1 Keep the current modules and extract helpers

Extract `split_server_resource`, `resolve_probe`, `find_active_session`, and the
WebSocket loop into shared functions, but keep command functions as the
orchestrators.

- Pros: smallest change; immediate duplication reduction.
- Cons: commands still mix UI, policy, and transport; no uniform progress; no
  dry-run; test coverage remains command-shaped.
- Verdict: rejected as the target and as a released intermediate state.
  Extracting helpers may be a temporary local working step, but the change set
  lands with the full rewrite.

### 15.2 Command trait objects

Give every command a `run()` method behind a trait, with shared context.

- Pros: uniform dispatch and easy registration.
- Cons: hides the flow; async trait objects add complexity; composition and
  plan inspection are harder.
- Verdict: rejected. A typed intent + workflow function is simpler.

### 15.3 Actor runtime in the CLI

Use actors and mailboxes for tasks, sessions, and UI.

- Pros: strong cancellation and streaming isolation.
- Cons: overkill for a short-lived process; adds lifecycle and supervision
  complexity that the server already handles.
- Verdict: rejected. Tokio tasks, cancellation tokens, and the executor are
  sufficient.

### 15.4 Plan as closures

Represent each task as `Box<dyn FnOnce(&mut Context) -> BoxFuture<...>>`.

- Pros: maximum flexibility, little type machinery.
- Cons: opaque plans; cannot render, dry-run, or test shape; hard to enforce
  ordering and cancellation.
- Verdict: rejected. Typed actions are worth the small amount of boilerplate.

### 15.5 Plan as a DAG or workflow engine

- Pros: parallel branches and complex dependencies.
- Cons: current workflows are linear; a DAG engine is a product of its own.
- Verdict: rejected for now. Revisit only if a real workflow needs parallel
  hardware operations with dependencies.

### 15.6 Move all orchestration to the server

Make the CLI a thin shell over a server-side "workflow" API.

- Pros: one implementation of policy; thinner client.
- Cons: local config, update, service lifecycle, editor, and interactive
  sessions remain client concerns; a server round trip for local operations is
  worse UX.
- Verdict: rejected as the sole model. Use the server for resource policy and
  the CLI for presentation and local operations.

### 15.7 Full TUI now

- Pros: richer progress and session views.
- Cons: large scope; terminal state, accessibility, and testing complexity.
- Verdict: out of scope. The layering intentionally allows a TUI later because
  workflows and ports do not depend on clap or the terminal.

## 16. Resolved decisions from review

All open questions have been answered. The implementation plan reflects these
choices:

| Topic | Decision |
| --- | --- |
| API scope | Include all four additions: attach-or-create, selector resolution, change idempotency, and the shared frame codec |
| Plan visibility | Print the full plan only with `--dry-run`; normal execution shows tasks as they run |
| Structured output | Defer user-facing `--json`/NDJSON; introduce typed views now and add renderers later |
| `list-sessions` default | Default to `active`; `--all` includes retained history and `--past` shows only history |
| Partial failures | Any requested server failure makes batch/list commands exit non-zero |
| `start-dap` | Keep it visible and failing until the backend exists |
| Direct token admin | Isolate `--direct` behind `local/direct_admin.rs`; do not add a shared core crate in this pass |
| Terminal mode | Defer raw mode/PTY; keep the `SessionInput`/`TerminalMode` seams |
| Ctrl+C during change follow | Detach locally and leave the server change running; do not abort by default |
| `--dry-run` scope | Support it on every command; simple commands show a one-task plan |

### Approval requested

This document asks for approval of:

- the four-layer split and dependency direction;
- the `CommandIntent` -> `Plan` -> `PlanExecutor` model;
- typed plan actions and ports instead of command-local orchestration;
- one reusable `follow_change` and one reusable `follow_session`;
- stdout/stderr and progress conventions;
- all four API additions in the same change set;
- the single-step delivery plan and completion criteria.

No open product questions remain. Remaining details are implementation
decisions within the boundaries above.

## 17. Decision log

| Date | Decision |
| --- | --- |
| 2026-11-16 | Proposal created: four-layer CLI architecture with intent, plan, executor, and I/O adapters. |
| 2026-11-16 | Chose a linear typed plan with slot keys; rejected a DAG/workflow engine. |
| 2026-11-16 | Chose one `follow_change` and one `follow_session` in the CLI client layer. |
| 2026-11-16 | Chose typed plan actions and injected ports over closure-based tasks. |
| 2026-11-16 | Recommended atomic attach-or-create, selector resolution, change idempotency, and a shared binary frame codec as API additions. |
| 2026-11-16 | Chose a single coordinated rewrite with no phased migration or old/new coexistence. |
| 2026-11-16 | Deferred TUI, PTY/raw mode, and binary splitting; left seams for each. |
| 2026-11-18 | Review decision: include all four API additions in the same change set. |
| 2026-11-18 | Review decision: print the full plan only with `--dry-run`. |
| 2026-11-18 | Review decision: defer `--json`; keep typed views internal for now. |
| 2026-11-18 | Review decision: `list-sessions` defaults to active. |
| 2026-11-18 | Review decision: any batch/server failure exits non-zero. |
| 2026-11-18 | Review decision: keep `start-dap` visible and failing. |
| 2026-11-18 | Review decision: isolate `--direct` behind `local/direct_admin.rs`; no shared core crate now. |
| 2026-11-18 | Review decision: defer terminal raw mode/PTY. |
| 2026-11-18 | Review decision: Ctrl+C during change follow detaches without aborting. |
| 2026-11-18 | Review decision: support `--dry-run` on all commands. |
