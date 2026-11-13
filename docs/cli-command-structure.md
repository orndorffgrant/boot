# CLI Command Structure — Design

Status: Implemented  
Date: 2026-11-12  
Implemented: 2026-11-13  
Scope: `boot-cli`, documentation

## Summary

Standardize the CLI on flat verb-noun command names:

```text
boot <verb>-<object> [arguments] [flags]
```

Each operation is one top-level command. There are no command groups and no
object-first names.

Examples:

```text
boot list-probes
boot run-firmware local/0483:374b:... firmware.elf
boot attach-probe local/0483:374b:...
boot attach-serial local/ttyACM0
boot replay-session local/ses_...
boot create-token pi --name ci --scope read
boot help-probe
boot update --check
```

This keeps the existing verb-first majority of the CLI and renames only the
object-first exceptions. It also removes the `admin` group, flattens its token
commands, and moves self-update to `boot update`.

Help output is customized rather than relying on clap's flat command list.
`boot help` and `boot --help` print a grouped overview, and flat
`help-<object>` commands print focused groups without adding nested
subcommands.

Server inspection and editing are consolidated: `show-server` defaults to
`local`, reports configuration and auth status, and for `local` also reports
the systemd service state and bind address. `edit-server` changes a server's
URL, CA certificate, or stored token; with no flags it opens a YAML document in
`$EDITOR` and saves the validated result. The separate `check-auth` and
`set-token` commands are removed.

No compatibility aliases are provided. There are no existing deployments, and
`AGENTS.md` rules out legacy shims.

## Goals

- One predictable naming pattern for every command.
- Preserve resource selectors and the existing attach/session workflows.
- Remove object-first names such as `probe-run`, `session-replay`, and
  `serial-shell`.
- Remove the `admin` catch-all without replacing it with a new one.
- Consolidate server inspection and credential editing into `show-server` and
  `edit-server` instead of separate one-off commands.
- Make the server HTTP API the normal token-administration path, with `--direct`
  reserved for local database recovery.
- Provide a custom grouped help overview and object-grouped help without
  introducing nested subcommands.
- Keep the top-level help small enough to scan in one screen.
- Make the command names obvious enough that most users do not need to read the
  design doc.

## Non-goals

- Changing the HTTP or WebSocket API.
- Changing `<server>/<resource>` selectors.
- Changing session, change, lease, or authentication semantics.
- Adding command groups or nested subcommands.
- Adding aliases or compatibility shims; there are no existing deployments.
- Reworking output formats such as `--json`.
- Implementing the remaining session and attach flag fixes in this pass.

## Previous state

Before this change, `boot-cli/src/main.rs` mixed conventions:

- Verb-noun: `start-server`, `stop-server`, `restart-server`, `run-server`,
  `list-servers`, `add-server`, `remove-server`, `set-token`, `check-auth`,
  `list-probes`, `list-serial-ports`, `list-sessions`.
- Object-verb: `probe-run`, `probe-attach`, `session-replay`, `session-export`.
- Anomalies: `start-probe-dap` (verb-noun compound), `serial-shell`
  (object-verb), `api-desc` (noun-only).
- Grouped commands: `admin create-token`, `admin list-tokens`,
  `admin revoke-token`, `admin update`.

The mixed ordering made `--help` hard to scan: probe commands were split
between `list-probes` and `probe-attach`, and the `admin` group mixed token
administration with local binary updates.

## Command grammar

```text
boot <verb>-<object> [positional arguments] [flags]
boot <verb> [flags]                       # self-directed commands only
```

Rules:

1. The top-level command starts with a verb.
2. The object follows the verb, joined by a hyphen.
3. `list-*` commands use plural objects (`list-probes`, `list-sessions`).
   Commands that act on one thing use singular objects (`run-firmware`,
   `attach-serial`, `replay-session`).
4. Resource selectors keep the `<server>/<resource>` form.
5. Flags express modes and options, not alternate commands. There is one attach
   command per resource kind rather than separate attach and follow commands.
6. Do not add synonyms for existing verbs. The verb list is closed unless a new
   user intent genuinely requires a new verb.
7. `update` is the only verb-only command. It has no useful object because it
   always operates on the installed binary.

### Verb semantics

| Verb | Meaning | Commands |
| --- | --- | --- |
| `add` | Register local configuration | `add-server` |
| `attach` | Create or join a live session | `attach-probe`, `attach-serial` |
| `create` | Mint a server-side token | `create-token` |
| `edit` | Modify local configuration | `edit-server` |
| `export` | Write retained session data to a file | `export-session` |
| `help` | Show grouped command help | `help-server`, `help-token`, `help-probe`, `help-serial`, `help-session`, `help-firmware`, `help-dap`, `help-openapi` |
| `list` | Enumerate resources | `list-servers`, `list-tokens`, `list-probes`, `list-serial-ports`, `list-sessions` |
| `remove` | Unregister local configuration | `remove-server` |
| `replay` | Stream retained session data to stdout | `replay-session` |
| `restart` | Restart the local service | `restart-server` |
| `revoke` | Invalidate a server-side token | `revoke-token` |
| `run` | Execute in the foreground | `run-server`, `run-firmware` |
| `show` | Print details or a generated document | `show-server`, `show-openapi` |
| `start` | Start a managed or long-lived process | `start-server`, `start-dap` |
| `stop` | Stop the local service | `stop-server` |
| `update` | Replace the installed binary | `update` |

`run-server` stays in the foreground. `start-server` delegates to the local
service manager. `run-firmware` preserves the existing `probe-run` behavior of
flashing and then optionally attaching. It is named for the firmware that is
executed, not for the probe that flashes it; the probe is selected by the
`<server>/<probe>` argument. `start-dap` starts a local TCP-to-DAP bridge.

## Commands

### Server management

| Command | Purpose |
| --- | --- |
| `start-server` | Start the local server service |
| `stop-server` | Stop the local server service |
| `restart-server` | Restart the local server service |
| `run-server` | Run the server in the foreground |
| `list-servers` | List configured servers |
| `show-server` | Show one server's configuration and auth status; adds local service state |
| `add-server` | Add a server and optionally store its token |
| `edit-server` | Edit a server's URL, CA certificate, or token |
| `remove-server` | Remove a server and its stored token |

`show-server` and `edit-server` replace the separate `check-auth` and
`set-token` commands.

`show-server` prints one server's configuration and auth state. The server name
is optional and defaults to `local`:

```text
boot show-server [name]
```

For `local`, it also reports the `boot-api` systemd service state and the host
and port it is bound to:

```text
Name:    local
URL:     http://127.0.0.1:46673
Service: Running
Bind:    127.0.0.1:46673
TLS:     http
CA:      none
Token:   stored
Server:  reachable
Read:    ok
Admin:   ok
```

- Local configuration is printed even when the server is unreachable.
- The auth portion reports reachability, missing or invalid credentials, read
  scope, and admin scope. `BOOT_TOKEN` and `BOOT_TOKEN_FILE` still override the
  stored credential for the check.
- Service state is read from the `boot-api` systemd unit. `active` maps to
  `Running`, `inactive` to `Stopped`, and `failed` to `Failure`; other states are
  shown as systemd reports them. If systemd is unavailable, the state is
  `unknown`.
- `Bind` comes from the unit's `--host` and `--port` arguments and defaults to
  `127.0.0.1:46673` when both are omitted. If the unit cannot be inspected,
  `Bind` is `unknown`.
- `URL` is the configured connection URL; `Bind` is the actual host and port from
  the service unit. They can differ when `--advertise-url` is used.
- `Service` and `Bind` are omitted for non-local servers, because the CLI only
  manages the local service.
- The service check and auth check are independent. A stopped service usually
  yields `Service: Stopped` and `Server: unreachable`.
- If the default `local` entry is not configured, the command reports that
  clearly. Use `start-server` to provision it or `show-server <name>` for
  another entry.

`edit-server` changes an existing server entry in place. With flags, it applies
the requested changes directly:

```text
boot edit-server <name>
                 [--url <url>]
                 [--ca-cert <path> | --clear-ca-cert]
                 [--token-stdin | --token-file <path> | --token <token> | --clear-token]
```

- `--url` updates the connection URL.
- `--ca-cert` installs a CA certificate; `--clear-ca-cert` removes it. The two
  options are mutually exclusive.
- `--token-stdin` and `--token-file` are preferred over `--token`, which exposes
  the secret in shell history and process listings. Use at most one token source.
- `--clear-token` removes the stored credential without removing the server and
  cannot be combined with a token source.

With no editable flags, `edit-server <name>` opens a YAML document in `$EDITOR`:

```yaml
url: https://pi.local:46673
ca_cert: /etc/boot/pi-ca.pem
# Set a value to replace the stored token, "" to clear it, or null to keep it.
token: null
```

- The document is the editable state of the server entry. `url` is required and
  must be a valid `http` or `https` URL.
- `ca_cert` is a path or `null`; `null` or `""` removes the CA override. A
  non-null path must point to a readable file.
- The stored token is never written to the document. `token: null` keeps the
  current token, `token: ""` clears it, and any other value replaces it.
- The server name is fixed by the positional argument and is not a YAML field.
- Missing `ca_cert` or `token` keys are treated as `null`.
- Unknown YAML keys are rejected so a typo does not silently become a no-op.
- The file is created with mode `0600` in a private temporary location and is
  deleted after the editor exits, including on validation failure.
- `$EDITOR` must be set. Editor commands with arguments, such as
  `code --wait`, are supported. The command requires an interactive terminal.
- If the editor exits non-zero or the file is missing, nothing is stored.
- When the editor exits, the document is parsed and validated. If it is invalid,
  the existing entry is left unchanged and the command exits non-zero.
- If it is valid, the entry is written atomically. Saving an unchanged document
  is a no-op.

`edit-server` only updates local configuration. Use `show-server` to verify the
result and `add-server`/`remove-server` to create or delete entries.

### Tokens and self-update

| Command | Purpose |
| --- | --- |
| `create-token` | Create a server-side API token |
| `list-tokens` | List server-side token metadata |
| `revoke-token` | Revoke one token or all tokens |
| `update` | Replace the installed binary |

Token administration defaults to the server's HTTP admin API. `create-token` and
`revoke-token` take the server in their positional selector; `list-tokens`
defaults to all configured servers and accepts `--server <name>` to narrow:

```text
boot create-token <server> --name <name>
                  [--read-only | --scope <scope>]...
                  [--expires-in <duration>] [--replace] [--save-to <server>]
boot list-tokens [--server <name>]
boot revoke-token <server>/<id-or-name> [--yes]
boot revoke-token <server> --all [--yes]
```

The selected server's stored credential must have the `admin` scope. With no
`--server`, `list-tokens` queries every configured server and prints a server
column, matching the other list commands. A server that is unreachable or lacks
admin scope is reported and skipped; the remaining results are still printed.

`--direct` switches to opening the API database directly and does not take a
server positional:

```text
boot create-token --direct --name <name> ...
boot list-tokens --direct
boot revoke-token --direct <id-or-name> [--yes]
boot revoke-token --direct --all [--yes]
```

- Without `--direct`, `create-token` requires the server positional and
  `revoke-token` takes `<server>/<id-or-name>` for one token or a bare `<server>`
  together with `--all`. `list-tokens` has no server positional; use `--server`
  to narrow it to one server.
- With `--direct`, the server positional is omitted. `revoke-token` takes a bare
  `<id-or-name>` instead of the `<server>/<id-or-name>` form, and `list-tokens`
  reads the local API DB.
- Supplying a server positional or `--server` together with `--direct` is an
  error. Supplying a `<server>/<token>` selector with `--direct` is also an error.
- `--all` requires a bare server positional (or `--direct`) and cannot be combined
  with a token selector.
- `--api-db <path>` is only meaningful with `--direct`; supplying it without
  `--direct` is an error.
- Direct mode is the recovery path when the server is stopped, the admin
  credential is lost, or the operator has filesystem access to the API DB.
- Direct mode uses the same database permissions as the server and does not
  contact the server.
- `list-tokens --direct` prints the same metadata without a server column and
  has no request identity, so it does not mark a `current` token.
- The `<server>/<id-or-name>` form matches the selector convention used by
  probes and sessions. Split at the first `/`; the remainder is the token id or
  name.

`--save-to <server>` still stores the returned plaintext token in the CLI DB for
a server entry, in either mode.

`update` is local-only; it has no server positional and no `--direct` mode.

### Probes

| Command | Purpose |
| --- | --- |
| `list-probes` | List available debug probes |
| `run-firmware` | Flash firmware and optionally attach |
| `attach-probe` | Attach to RTT output |
| `start-dap` | Start a DAP bridge for a probe |

### Serial ports

| Command | Purpose |
| --- | --- |
| `list-serial-ports` | List available serial ports |
| `attach-serial` | Attach to a serial session and open a shell |

### Sessions

| Command | Purpose |
| --- | --- |
| `list-sessions` | List live and retained sessions |
| `replay-session` | Replay a retained session |
| `export-session` | Export a retained session as asciicast v3 |

### Other

| Command | Purpose |
| --- | --- |
| `show-openapi` | Print the OpenAPI specification |

### Help output

`boot help`, `boot --help`, and a bare `boot` print a custom grouped overview
instead of clap's flat subcommand list:

```text
$ boot help
boot — flash, debug, and observe embedded targets

Usage:
  boot <command> [args]
  boot help-<object>

Server service
  start-server       Start the local server service
  stop-server        Stop the local server service
  restart-server     Restart the local server service
  run-server         Run the server in the foreground

Server entries
  list-servers       List configured servers
  show-server        Show a server's configuration and auth status
  add-server         Add a server and optionally store its token
  edit-server        Edit a server's URL, CA certificate, or token
  remove-server      Remove a server and its stored token

Tokens
  create-token       Create a server-side API token
  list-tokens        List server-side token metadata
  revoke-token       Revoke a server-side API token, or all tokens

Probes and firmware
  list-probes        List available debug probes
  run-firmware       Flash firmware and optionally attach
  attach-probe       Attach to RTT output
  start-dap          Start a DAP bridge for a probe

Serial ports
  list-serial-ports  List available serial ports
  attach-serial      Attach to a serial session and open a shell

Sessions
  list-sessions      List live and retained sessions
  replay-session     Replay a retained session
  export-session     Export a retained session as asciicast v3

System
  update             Update the locally installed boot binary
  show-openapi       Print the OpenAPI specification

Help
  help-server        help-token         help-probe        help-serial
  help-session       help-firmware      help-dap          help-openapi

Options:
  --cli-db <PATH>    Path to the CLI database
  --api-db <PATH>    Path to the API database
  -h, --help         Print help
  -V, --version      Print version
```

- `boot help` and `boot --help` print the same overview.
- A bare `boot` prints the same overview and exits with the usage-error status.
- `boot help <command>` delegates to `<command> --help`.
- The overview lists every operational command exactly once and uses these fixed
  groups and order: Server service, Server entries, Tokens, Probes and firmware,
  Serial ports, Sessions, System, Help.
- `show-server` lives under Server entries even though it reports local service
  state.
- `run-firmware` and `start-dap` live under Probes and firmware.
- The Help section lists the focused help commands as a compact block rather
  than giving each one a full description.

#### Focused help

Each object has a flat `help-<object>` command that lists the commands related
to that object:

| Command | Lists |
| --- | --- |
| `help-server` | `start-server`, `stop-server`, `restart-server`, `run-server`, `list-servers`, `show-server`, `add-server`, `edit-server`, `remove-server` |
| `help-token` | `create-token`, `list-tokens`, `revoke-token` |
| `help-probe` | `list-probes`, `run-firmware`, `attach-probe`, `start-dap` |
| `help-serial` | `list-serial-ports`, `attach-serial` |
| `help-session` | `list-sessions`, `attach-probe`, `attach-serial`, `replay-session`, `export-session` |
| `help-firmware` | `run-firmware` |
| `help-dap` | `start-dap` |
| `help-openapi` | `show-openapi` |

```text
$ boot help-probe
Probe commands:
  list-probes    List available debug probes
  run-firmware   Flash firmware and optionally attach
  attach-probe   Attach to RTT output
  start-dap      Start a DAP bridge for a probe
```

- Each `help-<object>` command prints the command name and one-line purpose.
  Use `<command> --help` for full flags and arguments.
- Help commands are meta-commands: they take no selectors or arguments.
- A command may appear in more than one focused group when it spans objects.
  `run-firmware` and `start-dap` appear under `help-probe` because they take a
  probe selector, and also under their own tighter groups.
- `update` has no object and appears only in the full overview.
- A single group registry is the source of truth for both the overview and the
  focused help commands. Tests should verify that every operational command
  appears exactly once in the overview, every command with an object appears at
  least once in the focused groups, and no group is empty.

## Why flat compound

### Advantages

- **Smallest change.** Most commands were already verb-first. Only six command
  names flip order or change shape, and the `admin` group is flattened.
- **No command taxonomy.** There is no need to decide whether a command belongs
  under `boot server` or `boot token`. Help groups are presentation-only and may
  overlap.
- **Custom grouped help.** `boot help` presents task groups and `help-<object>`
  gives a focused view, all without adding nested subcommands.
- **Argument structures stay put.** The existing `Args` structs map almost
  one-to-one to the renamed commands.
- **Self-describing names.** `attach-probe`, `attach-serial`, `replay-session`,
  and `export-session` read unambiguously without a group context.

### Trade-offs

- **The command set is larger.** There are around twenty operational commands
  plus the `help-<object>` commands. The custom overview and focused help keep
  this scannable, but the help renderer must group every command.
- **Focused help still needs an object name.** `help-probe` lists probe-related
  commands, but users must know to ask for that object. The grouped overview
  carries task-level discovery.
- **Help groups are a second registry.** The group membership must be kept in
  sync with the command definitions. Tests should enforce full coverage and
  overview uniqueness.
- **Custom help bypasses clap's generated list.** The custom renderer must keep
  usage, options, and command descriptions accurate as commands change.
- **Compound names can grow.** `start-probe-dap` was shortened to `start-dap`;
  future commands should be reviewed to avoid three-part compounds.
- **Naming consistency is enforced by review, not by types.** A group structure
  makes inconsistent object names more obvious. Here, the naming rules and
  parse tests are the guardrail.
- **`show-server` combines several concerns.** It prints local configuration,
  performs a reachability/scope check, and for `local` also inspects systemd.
  This replaces `check-auth`, but makes inspection depend on network and systemd
  state. The pieces are reported independently so local configuration is still
  useful when either is unavailable.
- **`edit-server` has two editing modes.** Flags are scriptable; no flags opens
  an interactive YAML document. The interactive path must securely handle the
  temporary file and avoid exposing the stored token.

## Alternatives considered

### Grouped verb-first

```text
boot list probes
boot run firmware ...
boot attach probe ...
boot start server
```

This gives a small, stable verb list and makes action discovery strong. Thin
groups such as `boot stop server` and `boot show openapi` are the cost.

Rejected because it adds a nesting level for no behavioral gain, requires more
renaming, and makes simple operations such as stopping the local service read
less directly. It also makes `boot update` awkward because it has no object.

### Noun-verb groups

```text
boot probe list
boot probe run
boot session replay
```

This gives the best resource-centric discovery: everything about probes lives
under `boot probe`. It was the first proposal.

Rejected because it reverses the current dominant style, requires more renaming,
and puts local server lifecycle and remote registry commands together under
`boot server`, which preserves the current ambiguity instead of resolving it.

### Hybrid

Keep resource groups for probes, serial, and sessions, but flat commands for
server lifecycle and tokens.

Rejected because having two conventions is the problem this design is solving.

## Implementation

The change touches `boot-cli/src/main.rs` and the command modules:

- Replace `AdminArgs` and `AdminCommand` with top-level token commands that
  support server selection and `--direct`. `create-token` and `revoke-token` use
  a server positional; `list-tokens` uses an optional `--server` flag and
  defaults to all servers.
- Add `show-server` with an optional name that defaults to `local`, plus
  systemd status and bind-address reporting for local.
- Add `edit-server` for URL, CA certificate, and token changes, including the
  no-flag `$EDITOR` YAML flow with temp-file cleanup and validation.
- Add a static group registry and a custom help renderer. `boot help`,
  `boot --help`, and a bare `boot` render the grouped overview; `help-<object>`
  renders a focused group; `boot help <command>` delegates to
  `<command> --help`.
- Wire `update.rs` to the top-level `update` command.
- Rename the probe, serial, and session argument types and functions to match
  the new command names.
- Update user-facing hints that reference the old command names.
- Add parse tests for the new names, server selection (positional and
  `--server`), the `<server>/<token-id>` selector, and the `--direct` mode,
  including conflicts between modes.
- Add tests that every operational command appears exactly once in the overview,
  every command with an object appears at least once in the focused groups, no
  group is empty, and the custom overview includes usage and global options.

## Follow-ups

These are related structural fixes that are not part of the rename:

- Add `--active`, `--past`, and `--all` to `list-sessions`. It currently always
  requests `all`.
- Add `--server <name>` filters to `list-probes`, `list-serial-ports`,
  `list-sessions`, and `list-servers`.
- Make `--session` mutually exclusive with `--follow` and `--new` in
  `attach-probe` and `attach-serial`.
- Make `--serial-shell` conflict with `--no-attach` in `run-firmware`.
- Align `attach-serial` with the session design: `--follow` should attach
  read-only by default, and `--write` should override it.
- Implement or hide `start-dap`; it currently always fails with "DAP sessions
  are not implemented yet".
- Update the remaining documentation examples that still use the old names.

## Decision log

| Date | Decision |
| --- | --- |
| 2026-11-12 | Proposal created with verb-first groups. |
| 2026-11-12 | Switched the proposal to flat verb-noun compounds. |
| 2026-11-12 | Named the flash-and-run command `run-firmware`, after the artifact rather than the probe. |
| 2026-11-12 | Removed `check-auth` and `set-token`; added `show-server` with auth status and `edit-server` with token/URL/CA editing. |
| 2026-11-12 | Token commands default to the HTTP admin API with the server name as the first positional; `--direct` selects the local API DB. |
| 2026-11-12 | `revoke-token` uses `<server>/<token-id>` for single-token revocation; `--all` keeps a bare server positional. |
| 2026-11-12 | `list-tokens` takes an optional `--server` flag and defaults to all configured servers. |
| 2026-11-12 | No migration guide or aliases; there are no existing deployments. |
| 2026-11-12 | `show-server` defaults to `local`; local output includes systemd service state and bind address. |
| 2026-11-12 | `edit-server` with no flags edits a validated YAML document in `$EDITOR`; the stored token is never written to the temp file. |
| 2026-11-12 | Added a custom grouped `boot help`/`--help` overview and flat `help-<object>` focused help commands. |
| 2026-11-13 | Implemented the command structure, server show/edit, token modes, and grouped help. |
