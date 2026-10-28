# Authentication for `boot-api`

**Status:** Proposed  
**Date:** 2026-10-28  
**Revision:** 8 — API DB is always read-write; remove read-only mode assumptions  
**Scope:** `boot-api`, `boot-api-client`, `boot-cli`

## Summary

`boot-api` has no authentication today. Its endpoints flash firmware, reset
targets, open RTT/DAP sessions, and read or write serial ports. The server binds to
loopback by default, but `--host` allows exposure, and any local process can use the
API on loopback.

This design adds **opaque bearer tokens**:

- Each client gets a random 256-bit token: `boot_v1_<base64url>`.
- Clients send `Authorization: Bearer <token>` on every request, including
  WebSocket upgrades.
- The server stores only SHA-256 hashes in a SQLite API database.
- Enforcement is a Dropshot `SharedExtractor`, so auth runs **before** request
  bodies are parsed and **before** WebSocket upgrades.
- Tokens are scoped: `read`, `control`, and `admin`. `admin` can create, list, and
  revoke tokens through `/v1/tokens`.
- `boot-cli` auto-provisions an admin token and stores client tokens in a SQLite
  CLI database.
- Token administration lives under `boot admin`: `boot admin create-token`,
  `boot admin list-tokens`, `boot admin revoke-token`. Client-credential commands
  (`add-server`, `set-token`, `check-auth`) stay top-level.
- Admin commands write the local API DB by default and can use the HTTP admin API
  with `--server <name>`.
- Non-loopback binds require TLS or an explicit `--allow-insecure-http`
  acknowledgement. SSH port-forwarding and WireGuard/Tailscale remain the
  recommended remote patterns.
- The design targets Dropshot `0.17.1` (current latest), which requires Rust 1.85
  and edition 2024.

## Current state

- `boot-api/src/lib.rs` defines the API trait; every endpoint is open.
- `boot-api/src/run.rs` uses `type Context = ()` and binds `--host`/`--port`
  (default `127.0.0.1:46673`).
- `boot-cli/src/server_config.rs` stores only `{ name, url }` in
  `$XDG_DATA_HOME/boot/servers.json`.
- `boot-cli/src/probe.rs` and `serial_port.rs` use
  `boot_api_client::Client::new(&server.url)`.
- `boot-cli/src/systemd_service.rs` starts `boot run-server` as a transient unit.

There is no token, password, TLS, or client identity anywhere. There is no existing
`auth.json` or `credentials.json`: authentication has not shipped. The current
development-only `servers.json` is replaced by `boot-cli.sqlite3`; there are no
existing deployments.

## Goals

1. Prevent unauthenticated callers from listing or using hardware.
2. Keep the safe local CLI workflow zero-config.
3. Support remote servers without mandatory certificate management.
4. Support revocation without restarting the server.
5. Keep tokens out of URLs, logs, shell history, and world-readable files.
6. Allow read-only access without granting flash/shell access.
7. Keep `boot-api-client` usable through one shared HTTP-client helper.
8. Replace `servers.json` with SQLite for all client state.
9. Support token administration over HTTP with a dedicated `admin` scope, while
   keeping local file access as the recovery path.

## Non-goals

- Multi-user accounts, password login, or an identity provider.
- Per-probe or per-serial-port ACLs.
- Protection against root on the host or physical device access.
- Preventing an authorized operator from flashing arbitrary firmware. That is the
  product.
- A browser UI. A future one needs a separate token/CSRF design.
- Changing the hardware API or its version.

## Threat model

### Assets

- Probes and the targets attached to them.
- Serial consoles, including bootloaders and recovery shells.
- The server user's USB/serial access.
- Firmware images and RTT output.
- Client tokens for other `boot` servers.
- Admin tokens and the ability to mint or revoke credentials.

### Adversaries

- Anyone on the LAN or overlay if bound to a non-loopback address.
- A malicious local process on a shared machine or CI runner.
- Accidental exposure via `--host 0.0.0.0`, a port-forward, or a container mapping.
- Token theft through logs, shell history, process args, backups, or plaintext.
- A stolen admin token used to mint persistent credentials or revoke access.
- Scanners looking for open debug servers.

### Not defended against

- Root/administrator on the server host. The operator can read the databases and
  mint admin tokens directly.
- Physical access to the host or probe.
- A compromised dependency or build pipeline.
- Denial of service by someone holding a valid control or admin token.

## Design

### Decision summary

| Area | Decision |
| --- | --- |
| Credential | Opaque 256-bit bearer token, `boot_v1_...` |
| Transport | Loopback default; TLS or encrypted overlay for remote |
| API DB storage | SHA-256 hashes in `boot.sqlite3`, mode `0600` |
| CLI DB storage | `boot-cli.sqlite3` for servers and plaintext client tokens, mode `0600` |
| Enforcement | Dropshot 0.17.1 `SharedExtractor` (`Auth<Read>`, `Auth<Control>`, `Auth<Admin>`) |
| Scopes | `read`, `control`, `admin`; `admin` implies both others |
| Token API | `GET/POST /v1/tokens`, `DELETE /v1/tokens/{selector}`, `DELETE /v1/tokens?all=true` |
| CLI naming | `boot admin create-token`, `boot admin list-tokens`, `boot admin revoke-token`; verb-noun leaves |
| Provisioning | First run auto-creates an admin `local` token; `--no-provision` skips only local auto-provisioning |
| API DB access | Read-write for both the server and CLI; migrations always run |
| Revocation | SQLite delete + immediate snapshot update + hot reload; WebSockets revalidate |
| Server DB writes | Writer connection for `/v1/tokens`; no per-request API DB writes |
| OpenAPI | HTTP bearer scheme, 401/403 responses, admin endpoints included |

### Why bearer tokens

- They work for HTTP and WebSocket upgrades without putting a secret in the URL.
- They can be revoked per client, unlike a shared password or stateless JWT.
- They need no identity provider or certificate authority.
- They can be scoped for read-only monitoring clients.
- Rotation is simple: create, update client, revoke old.

Passwords plus sessions were rejected: there is no user directory, and cookies add
CSRF concerns to a CLI-first tool. mTLS and OIDC are stronger in some dimensions but
add certificate/IdP operations that are not justified for a single-operator tool.

### Token format

```text
boot_v1_<base64url-no-pad(32 random bytes)>
```

- 32 CSPRNG bytes = 256 bits of entropy.
- The `boot_v1_` prefix helps secret scanners recognize leaks and allows format
  evolution.
- Base64url without padding avoids header/URL escaping issues.
- Generated tokens are 51 characters. The server rejects malformed, short, long,
  or non-decodable tokens, so a hand-edited weak token is not silently accepted.
- Clients treat the token as opaque; they never parse or shorten it.

Suggested secret-scanning pattern:

```text
boot_v1_[A-Za-z0-9_-]{43}
```

### SQLite storage layout

Use two databases rather than one. This preserves the security property that the
server never needs plaintext client credentials:

| Database | Owner | Contents | Default path |
| --- | --- | --- | --- |
| API DB | `boot-api` / `boot admin` | Token hashes, names, scopes, expiry | `$XDG_DATA_HOME/boot/boot.sqlite3` |
| CLI DB | `boot-cli` | Server entries, plaintext client tokens, CA paths | `$XDG_DATA_HOME/boot/boot-cli.sqlite3` |

On Linux the defaults are `~/.local/share/boot/boot.sqlite3` and
`~/.local/share/boot/boot-cli.sqlite3`. Both database files live in the existing
data directory.

Overrides:

- `BOOT_API_DB` and `--api-db <path>` for the API DB.
- `BOOT_CLI_DB` and `--cli-db <path>` for the CLI DB.
- `boot start-server` passes both paths explicitly to the systemd unit, and
  `boot run-server` accepts the same flags, so a different `HOME` or working
  directory cannot select a different database.

Why SQLite rather than JSON:

- Token create/revoke and server add/remove are single transactions; a crash cannot
  leave a half-written file.
- The server can detect committed changes without re-parsing a whole document.
- Unique names and token hashes are enforced by the schema.
- `servers` and `credentials` can be updated atomically in one transaction.
- The database is a standard format with backup and inspection tools.

Why two databases:

- The API DB contains only hashes and can be backed up or mounted more freely.
- The CLI DB contains plaintext tokens and stays on the client. It also holds the
  auto-provisioned local **admin** token, so CLI DB read access is equivalent to
  administrative API access; keep it `0600` and out of sync/backups.
- The server opens only the API DB at runtime and never queries the CLI DB.
- A future single-database mode is possible but would mix those trust levels.

The API DB is written by both the CLI and the server. The CLI writes it directly
when `boot admin` targets the local database (the default); the server writes it
through the `/v1/tokens` admin endpoints. Keep both paths in the same module:
writers serialize with `BEGIN IMMEDIATE`, and the server updates its in-memory
snapshot on the commit path. `--no-provision` only skips creating the local
token/server entry/credential; migrations and the admin endpoints are unaffected.

### API database schema

```sql
PRAGMA application_id = 0x424F4F54;  -- "BOOT"

CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
    id         TEXT    PRIMARY KEY,   -- 16 lowercase hex characters
    name       TEXT    NOT NULL UNIQUE,
    token_hash BLOB    NOT NULL UNIQUE, -- 32 raw SHA-256 bytes
    scopes     TEXT    NOT NULL,       -- canonical "read", "read,control", or "read,control,admin"
    created_at INTEGER NOT NULL,       -- Unix seconds
    expires_at INTEGER,                -- NULL = never
    CHECK (length(id) = 16),
    CHECK (length(token_hash) = 32),
    CHECK (length(scopes) > 0),
    CHECK (name <> '')
);
```

- `id`: non-secret random identifier for list/revoke. 8 random bytes, lowercase hex.
- `name`: unique and human-readable (`laptop`, `ci`, `pi-console`). Reject names
  that look exactly like a 16-character hex id, so `revoke-token <value>` is
  unambiguous.
- `token_hash`: `sha256(full token string)`, stored as a 32-byte BLOB. Plaintext is
  never stored in the API DB. Because tokens are 256-bit random values, a SHA-256
  map lookup is sufficient; no password KDF or salt is needed.
- `scopes`: canonical comma-separated list, sorted. `control` is stored as
  `read,control`; `admin` as `read,control,admin`. Unknown scopes read by an older
  server are ignored with a warning so a newer CLI can add scopes without breaking
  an older server.
- `created_at`/`expires_at`: Unix seconds. No expiry by default; opt in with
  `boot admin create-token --expires-in`.

Schema notes:

- `PRAGMA application_id = 0x424F4F54` (`BOOT`) distinguishes this file from the
  CLI DB (`0x42434C49`, `BCLI`) and from unrelated SQLite files.
- `schema_migrations` is the source of truth for applied migrations. A database
  with a version newer than the binary supports is a startup error; do not run an
  older server against a newer schema.
- All statements use bound parameters. Token names are never interpolated into SQL.
- `token_hash` is unique, which also creates the lookup index.

### CLI database schema

```sql
PRAGMA application_id = 0x42434C49;  -- "BCLI"

CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS servers (
    name       TEXT PRIMARY KEY,
    url        TEXT NOT NULL,
    ca_cert    TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (name <> ''),
    CHECK (url LIKE 'http://%' OR url LIKE 'https://%')
);

CREATE TABLE IF NOT EXISTS credentials (
    server_name TEXT PRIMARY KEY
        REFERENCES servers(name) ON DELETE CASCADE,
    token       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);
```

- A server and its credential are separate rows but the same database, so `add-server`
  can insert both in one transaction and `remove-server` can rely on
  `ON DELETE CASCADE`.
- `url` is stored once. There is no separate URL/token file that can disagree.
- `ca_cert` is an optional PEM root for self-signed servers. It is a path, not a
  secret.
- `application_id` is `0x42434C49` (`BCLI`), so pointing `--cli-db` at
  `boot.sqlite3` fails fast instead of writing client state into the API DB.

### SQLite PRAGMAs, transactions, and permissions

Every connection:

```sql
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA secure_delete = ON;
```

Database creation / write connections additionally:

```sql
PRAGMA journal_mode = DELETE;
PRAGMA synchronous = FULL;
```

- Use the rollback journal (`DELETE`), not WAL, for both databases. Writes are rare
  and short, and the rollback journal keeps the database self-contained without
  `-wal`/`-shm` side files.
- `secure_delete = ON` zeroes deleted token/credential content in database pages.
  The transient rollback journal is protected by the directory mode.
- `busy_timeout` handles the brief overlap when the CLI writes a token while the
  server reads, or (later) when both the CLI and the server write the API DB.
- Use `BEGIN IMMEDIATE` for multi-statement writes (`boot admin create-token`/
  `boot admin revoke-token`, `/v1/tokens` handlers, server add/remove, and schema
  changes). Keep transactions short; do not do network I/O inside a transaction.
- If write volume grows (routine `last_used_at` updates or a busy remote admin
  setup), revisit `journal_mode = WAL` for the API DB and measure the write rate.
  WAL allows one writer plus concurrent readers. Token administration alone is rare
  enough to keep `DELETE`.

File and directory permissions:

- Create the parent directory with mode `0700` and the database file with explicit
  mode `0600` (for example, pre-create with
  `OpenOptionsExt::mode(0o600)` before opening it with `rusqlite`). Do not rely on
  the process umask.
- On open, verify the main database file is not group- or world-readable. If it is,
  refuse to use it and print the exact `chmod` command. The parent directory being
  `0700` protects transient journals.
- Do not `chmod` a user's file silently; make the operator fix it once.
- The API DB is opened read-write by the server and the CLI. File permissions are
  still verified before use.

### Enforcement in Dropshot 0.17.1

The workspace currently pins `dropshot = "0.16"`. Update it to `0.17` (resolving to
`0.17.1` at the time of writing) and update the lockfile; if the lockfile is
already pinned, use `cargo update -p dropshot --precise 0.17.1`. Dropshot 0.17
requires Rust 1.85 and edition 2024; the workspace already uses edition 2024, so
also record `rust-version = "1.85"` in the workspace package metadata.

Relevant Dropshot 0.17.1 facts, verified against the current API docs:

- `SharedExtractor` and `ExclusiveExtractor` still exist, with the same
  `from_request`/`metadata` shape used below.
- Endpoints may have 0–5 extractors, with at most one exclusive extractor, and it
  must be last. Existing handlers use at most three, so nothing needs splitting.
- `Header<H>` is a new shared extractor that documents headers in OpenAPI. We do
  **not** use it for `Authorization`: it returns raw header values, produces a
  generic `400` rather than our `401`/`403`, and would add an `authorization`
  parameter to generated clients.
- `ServerBuilder::tls(Option<ConfigTls>)` and
  `ConfigTls::AsFile { cert_file, key_file }` are unchanged.
- Dropshot 0.17 has API-manager support, but this project builds its spec through a
  manual `openapi()` function. There is no reason to adopt the API manager in this
  change.
- `ExtractorMetadata { extension_mode, parameters }` and `ExtensionMode::None` are
  unchanged.

Dropshot has no global request middleware. A helper called at the top of a handler
would run **after** extractors, so a 10 MB flash body would be parsed before the
authentication check, and new endpoints could forget the call.

Auth belongs in a shared extractor that is part of every protected handler
signature:

```rust
#[dropshot::api_description]
trait BootApi {
    type Context;

    #[endpoint { method = GET, path = "/v1/health" }]
    async fn health(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint { method = GET, path = "/v1/probes" }]
    async fn probes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{selector}",
        request_body_max_bytes = 10485760,
    }]
    async fn flash_and_run(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;
}
```

Extractor sketch:

```rust
use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

use dropshot::{
    ApiEndpointBodyContentType, ExtractorMetadata, ExtensionMode, HttpError,
    RequestContext, ServerContext, SharedExtractor,
};

pub trait RequiredScope: Send + Sync + 'static {
    const CAPABILITY: Capability;
}

pub struct Read;
impl RequiredScope for Read {
    const CAPABILITY: Capability = Capability::Read;
}

pub struct Control;
impl RequiredScope for Control {
    const CAPABILITY: Capability = Capability::Control;
}

pub struct Admin;
impl RequiredScope for Admin {
    const CAPABILITY: Capability = Capability::Admin;
}

pub struct Auth<S: RequiredScope>(pub Principal, PhantomData<S>);

impl<S: RequiredScope> SharedExtractor for Auth<S> {
    fn from_request<'a, 'async_trait, C>(
        rqctx: &'a RequestContext<C>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, HttpError>> + Send + 'async_trait>>
    where
        C: 'async_trait + ServerContext,
        Self: 'async_trait,
        'a: 'async_trait,
    {
        Box::pin(async move {
            let state = (rqctx.context() as &dyn Any)
                .downcast_ref::<AuthState>()
                .ok_or_else(|| HttpError::for_internal_error("auth state unavailable".into()))?;
            let principal = state.authorize(rqctx.request.headers(), S::CAPABILITY)?;
            Ok(Auth(principal, PhantomData))
        })
    }

    fn metadata(_: ApiEndpointBodyContentType) -> ExtractorMetadata {
        ExtractorMetadata {
            extension_mode: ExtensionMode::None,
            parameters: Vec::new(),
        }
    }
}
```

Notes:

- `Auth` is shared, so it runs before `TypedBody` and `WebsocketUpgrade`.
- It adds no OpenAPI parameters; the security scheme is added separately.
- `type Context` becomes `AuthState`; the downcast keeps the extractor independent
  of the rest of the context.
- Adding the extractor to a handler makes the check part of the API type, but the
  compiler cannot force a new endpoint to include it. A test must enumerate the
  routes and verify that every non-health route rejects missing credentials and
  enforces the intended scope.

### Header validation

- Accept exactly one `Authorization` header; reject multiple values.
- Accept the `Bearer` scheme case-insensitively.
- Allow one or more spaces between scheme and token.
- Reject empty tokens, internal whitespace, non-UTF-8 values, and tokens longer
  than a small bound (for example 128 bytes).
- Do not accept query-string tokens by default. The CLI can set headers on the
  WebSocket upgrade, and query tokens leak into logs and proxy access logs.
- Return the same `401` for all failures; log the specific reason internally.
- Hash the complete token string exactly as sent; do not normalize case.

### Authorization scopes

| Scope | Allows |
| --- | --- |
| `read` | List probes, list serial ports, health |
| `control` | Everything `read` allows, plus flash, target query, RTT attach, DAP, serial shell |
| `admin` | Token administration via `/v1/tokens`; implies `control` |

`control` implies `read`; `admin` implies `control`. The CLI expands scopes when
creating a token so `boot admin list-tokens` shows the effective set.

`get_target` is `control`, not `read`: the current implementation opens the probe
and attaches a session (`probe.attach`), which can halt or disturb the target.
Anything that touches hardware is `control`; only enumeration is `read`.

Endpoint policy:

| Endpoint | Scope |
| --- | --- |
| `GET /v1/health` | public |
| `GET /v1/probes` | `read` |
| `GET /v1/serial-ports` | `read` |
| `GET /v1/tokens` | `admin` |
| `POST /v1/tokens` | `admin` |
| `DELETE /v1/tokens/{selector}` | `admin` |
| `DELETE /v1/tokens?all=true` | `admin` |
| `GET /v1/probes/{selector}/target` | `control` |
| `POST /v1/probes/{selector}` | `control` |
| `GET /v1/probes/{selector}/attach` | `control` |
| `GET /v1/probes/{selector}/attach/{rtt_addr}` | `control` |
| `GET /v1/probes/{selector}/dap` | `control` |
| `GET /v1/serial-ports/{port_id}/shell` | `control` |

Health is public so monitoring and `boot check-auth` can test reachability without
a token. It exposes only status, version, and the fact that auth is required. It does
not expose probe, serial, or token information.

### Token administration API

Token management is part of the HTTP API and guarded by `Auth<Admin>`:

| Method | Path | Result |
| --- | --- | --- |
| `GET` | `/v1/tokens` | List metadata (`id`, `name`, `scopes`, `created_at`, `expires_at`, `current`); never hashes or plaintext |
| `POST` | `/v1/tokens` | Create a token and return the secret exactly once (`201`) |
| `DELETE` | `/v1/tokens/{selector}` | Revoke one token by id or name (`{"revoked": 1}`) |
| `DELETE` | `/v1/tokens?all=true` | Revoke every token (`{"revoked": n}`) |

`POST /v1/tokens` request:

```json
{
  "name": "laptop",
  "scopes": ["read", "control"],
  "expires_in_seconds": 2592000,
  "replace": false
}
```

- `scopes` defaults to `["read"]`. Values are `read`, `control`, and `admin`;
  `control` expands to `read,control`, and `admin` to `read,control,admin`.
  Empty or unknown scopes are a `400`.
- `expires_in_seconds` is optional and must be positive. The CLI converts
  `--expires-in 30d` before calling the endpoint.
- `replace: true` revokes an existing token with the same name in the same
  `BEGIN IMMEDIATE` transaction, then inserts the new one. Without it, a duplicate
  name is `409`.
- The response includes `id`, `name`, `scopes`, `created_at`, `expires_at`, and
  the plaintext `token`. It is the only time the server returns the secret. Send
  `Cache-Control: no-store` and `Pragma: no-cache`; never log the body.
- Names are validated like CLI names: non-empty, bounded length, and not exactly
  16 hex characters (so revoke-by-selector stays unambiguous).

`GET /v1/tokens` returns metadata only. `current: true` marks the token used for
the request; the CLI uses it to warn before self-revocation. The list is ordered by
`created_at` and never includes the token or hash.

Revocation by selector accepts a token `id` or `name`; an unknown selector is
`404`. Revoke-all requires the literal query `all=true`; omitting it is `400`, so a
stray `DELETE /v1/tokens` cannot wipe the deployment. Both revoke forms return the
number of deleted rows. Revoking the caller's own token is allowed; the CLI warns
and requires confirmation.

The admin endpoints are always available when the API is running and the caller has
an `admin` token. They use the API DB writer connection; `--no-provision` does not
change endpoint availability. Admin endpoints follow the same bind and TLS rules as
the rest of the API; a non-loopback bind still requires TLS or
`--allow-insecure-http`.

Rust shape:

```rust
#[endpoint { method = POST, path = "/v1/tokens" }]
async fn create_token(
    rqctx: RequestContext<Self::Context>,
    _auth: Auth<Admin>,
    body: TypedBody<TokenCreateRequest>,
) -> Result<HttpResponseCreated<TokenCreated>, HttpError>;

#[endpoint { method = DELETE, path = "/v1/tokens" }]
async fn revoke_all_tokens(
    rqctx: RequestContext<Self::Context>,
    _auth: Auth<Admin>,
    query: Query<RevokeAllQuery>,
) -> Result<HttpResponseOk<RevokeResult>, HttpError>;
```

Every handler includes `Auth<Admin>`, so authentication runs before `TypedBody`
parsing and before any database write.

### Token lifecycle

#### Local bootstrap

`boot start-server` and `boot run-server`:

1. Open the CLI DB and apply schema migrations.
2. Open the API DB and apply auth migrations. Migrations always run, independent of
   `--no-provision`.
3. If `--no-provision` is set, skip local credential provisioning (steps 4–7) and
   continue at step 8. No `local` token, server entry, or credential is created.
4. Upsert the `local` server entry with the advertised URL.
5. If the API DB has no tokens, generate an **admin** token named `local` with
   scopes `read,control,admin`, write its hash to the API DB, and store the
   plaintext token in the CLI DB credential for `local`. Both writes are separate
   transactions; if the second fails, the token remains valid and the CLI prints
   `boot set-token local --token-stdin` instead of losing the secret.
6. If the API DB has tokens but none has the `admin` scope, do not silently mint a
   second admin token and do not replace anything. Log a warning and print:
   `boot admin create-token --name local-admin --scope admin` then
   `boot set-token local --token-stdin`. Existing tokens stay valid.
7. If the API DB has an admin token but the local credential is missing, do not
   silently revoke anything. Print:
   `boot admin create-token --name local-admin --scope admin --save-to local`, or
   `boot set-token local --token-stdin` if the secret is available elsewhere.
8. Start the server.

The bootstrap secret is never printed to stdout or the journal; it goes directly
from the CLI process to the `0600` CLI database. `boot admin create-token` is the
command that prints a token, once, for copying to another machine.

`--no-provision` affects only the local token, `local` server entry, and CLI
credential. Migrations still run, the API DB stays writable, and `/v1/tokens`
remains available. The database may be empty, in which case protected endpoints
return `401` until a token is created with `boot admin`.

`boot stop-server` must **not** delete the `local` server row or its credential. The
current implementation removes the local entry when stopping; with authentication
that would throw away the local credential and force a reissue on every restart.
Configuration is not service state.

#### Create, list, revoke

These commands write the local API DB by default, so they work with the server
stopped, over SSH, and as the lockout recovery path. Add `--server <name>` to call
the `/v1/tokens` admin endpoints with that server's stored credential instead.

```console
$ boot admin create-token --name laptop
boot_v1_...                      # printed once; store it on the laptop
$ boot admin list-tokens
ID                NAME     SCOPES                 CREATED              EXPIRES
9f2c1a7b4e8d0c31  laptop   read, control          2026-10-28 12:34     never
b3a91c44d2f10e77  local    read, control, admin   2026-10-28 12:30     never
$ boot admin revoke-token laptop
$ boot admin revoke-token --all        # requires explicit confirmation
```

Against a remote server with an admin credential:

```console
$ boot admin --server pi create-token --name ci --read-only --save-to ci
$ boot admin --server pi list-tokens
$ boot admin --server pi revoke-token ci
```

- `--read-only` creates `["read"]`; `--scope control` creates `["read","control"]`;
  `--scope admin` creates `["read","control","admin"]`. `--scope` is repeatable and
  scopes are unioned/expanded. `--read-only` with `--scope` is an error rather than a
  silent override.
- `--expires-in 30d` sets `expires_at`; the API endpoint receives seconds.
- `--replace` revokes an existing token with the same name in the same transaction
  before inserting the new one. Without it, a duplicate name is an error. Replacing
  the token used by the current HTTP request is allowed; the new secret is returned
  in the response.
- `--save-to <server>` stores the plaintext token in the CLI DB for a server entry
  after the API DB commit or HTTP response. If it fails, the token is still valid
  and has already been printed; use `boot set-token` to retry without reissuing.
- Names are unique; revoke-by-name fails if ambiguous. `list-tokens` never prints
  the token or hash; in remote mode it marks the requesting token with `current`.
- `create-token` writes in one `BEGIN IMMEDIATE` transaction and prints the secret
  only to stdout; stderr warns that it will not be shown again.
- Local writes are picked up by the reloader. HTTP admin writes update the
  in-memory snapshot on the commit path, so a new token is usable immediately.
- If the API DB is locked or unwritable, the command fails with a clear error.
- `--server` uses the stored token for that entry and requires the `admin` scope.
  A `403` means the stored credential is not an admin token; store one with
  `boot set-token`.

There is still no unauthenticated bootstrap endpoint. The first admin token comes
from filesystem provisioning (`boot start-server` or `boot admin` with direct
`--api-db` access), which is the same trust boundary as the `0600` database file.

#### Revocation and hot reload

- The server keeps an in-memory snapshot of the token table:
  `HashMap<[u8; 32], TokenRecord>` by hash and `HashMap<String, TokenRecord>` by id.
- A dedicated blocking thread opens its own SQLite connection to the API DB and
  polls `PRAGMA data_version`, `PRAGMA schema_version`, and the file metadata every
  1–2 seconds. When any changes (or the file is replaced), it reopens if needed, runs
  one `SELECT` over `tokens`, and swaps in a new snapshot. There is no per-request
  database I/O.
- `data_version` changes when another connection commits, which covers CLI writes
  and the server's admin writer connection. When `boot-api` writes through
  `/v1/tokens`, it updates the in-memory snapshot on the same commit path (or bumps
  a generation counter) instead of waiting for the poller, so a just-created token
  is accepted immediately and a revoked one stops working at once.
- It works with the rollback journal. The reloader uses its own connection; the
  admin endpoints use a separate writer connection. `--no-provision` leaves both
  connections in place.
- An admin write commits and then swaps the shared snapshot before returning `201`
  or `200`. The request path never holds the write lock: `BEGIN IMMEDIATE` covers
  only the `DELETE`/`INSERT` statements, and the in-memory swap happens after
  `COMMIT`. A failed commit leaves the snapshot untouched.
- At startup, a missing, unreadable, or wrong-application-id API DB is fatal: fail
  closed. A schema version newer than the binary supports is also fatal.
- If a reload fails transiently (`SQLITE_BUSY`, an interrupted read), keep the last
  known-good snapshot and retry. If the database is deleted, replaced with a
  different application id or schema, or remains unreadable for 30 seconds, swap in
  an empty snapshot and log an error. Protected requests then fail with `401`.
- SQLite transactions eliminate the old "half-written JSON parse error" class: a
  reader sees either the old or the new committed token set.
- Revocations from the CLI apply within the reload interval; revocations through
  `/v1/tokens` apply immediately.
- Active WebSockets store the token id and revalidate periodically; revoked or
  expired tokens close the connection.

### WebSockets

All streaming endpoints use `WebsocketUpgrade`. Because `Auth` is shared, it runs
before the upgrade, so a missing or invalid token yields a normal `401` rather than
an upgrade.

After upgrade:

- Pass the `Principal` (token id/name) into the handler.
- On a periodic tick (for example every 5 seconds), check that the token id is
  still present, unexpired, and has `control` in the in-memory snapshot.
- If not, send a WebSocket `Close` with policy-violation status and stop the
  probe/serial/DAP loop.
- Enforce an idle timeout and a global/per-token session limit to avoid leaving
  probes open forever. Revalidation is the minimum; timeouts and limits are
  hardening.

Revocation cannot interrupt a connection instantly, but a few seconds is acceptable
for this tool.

### Transport security and exposure

The default bind remains `127.0.0.1`/`localhost`. Auth is required even on loopback,
because any local process can connect; automatic provisioning makes this invisible.

If `--host` is non-loopback, the server refuses to start unless one of these is true:

1. `--tls-cert` and `--tls-key` are provided. Dropshot has native TLS:

   ```rust
   ServerBuilder::new(api, auth_state, log)
       .tls(Some(ConfigTls::AsFile { cert_file, key_file }))
       .start()?;
   ```

2. `--allow-insecure-http` is passed. This explicitly acknowledges plaintext
   tokens. It is appropriate only on an already-encrypted network
   (WireGuard/Tailscale) or an isolated lab network. Print a loud warning and show
   plaintext mode in `boot list-servers`.

Loopback detection treats `localhost` and loopback IP literals as local. `0.0.0.0`,
`::`, a LAN IP, or a non-local hostname is non-loopback.

Remote-access options, best first:

1. **SSH local port forwarding.** Keep the server on loopback:
   `ssh -L 46673:127.0.0.1:46673 pi`. No TLS/cert management; the token still
   protects against other local users and accidental exposure on the Pi.
2. **Tailscale/WireGuard.** Bind the overlay IP (or `0.0.0.0` with
   `--allow-insecure-http`). The overlay encrypts; the token is the app-layer
   control.
3. **Native TLS.** Use a certificate valid for the hostname the client uses.
   Self-signed CAs can be trusted with `boot add-server ... --ca-cert`.
4. **Reverse proxy.** Bind loopback and let Caddy/nginx terminate TLS. The proxy
   must preserve `Authorization` and redact it from logs.

Other rules:

- Never put tokens in URLs. The CLI uses headers for HTTP and WebSocket upgrades.
- Disable or same-origin-restrict redirects in the client so a malicious server
  cannot redirect the token to another host.
- Do not disable certificate verification. For self-signed servers, add the
  specific CA; never use `danger_accept_invalid_certs`.
- A future browser client must not use a long-lived token in a query string. Use a
  short-lived ticket or a `Secure; HttpOnly; SameSite` cookie with CSRF protection.

### Error semantics

| Status | When | Response | Headers |
| --- | --- | --- | --- |
| `401` | Missing, malformed, unknown, expired, or revoked token | Generic "missing or invalid bearer token" | `WWW-Authenticate: Bearer realm="boot-api"` |
| `403` | Valid token but insufficient scope | "token lacks required scope: admin" | `WWW-Authenticate: Bearer realm="boot-api", error="insufficient_scope", scope="admin"` |
| `400` | Invalid admin request (bad/empty scope, invalid name or expiry, missing `all=true`) | Validation message | |
| `404` | Revoke selector does not exist (after admin auth) | "token not found" | |
| `409` | Create with an existing name and no `replace` | "token name already exists" | |
| `429` | Rate limit (future) | "too many failed authentication attempts" | `Retry-After` |

Do not distinguish unknown, expired, and revoked tokens in the response. Log the
distinction server-side.

CLI mapping:

- `401`: `Server 'pi' rejected the token. Run: boot set-token pi --token-stdin`
- `403` scope: `Token for 'pi' lacks the 'control' scope. Create a new token on the
  server and update this client.`
- `403` for `boot admin --server`: `The stored token for 'pi' is not an admin token.
  Store one with boot set-token pi --token-stdin, or use boot admin --api-db on the
  server host.`
- `404`: `No token matches '<selector>'. Run: boot admin list-tokens`
- `409`: `A token named '<name>' already exists. Use --replace to rotate it.`

### Audit and observability

- Log auth failures at `warn` with request id, remote address, method, path, and
  reason (`missing`, `malformed`, `unknown`, `expired`, `scope`). Never log the
  token, its hash, or the full `Authorization` header.
- Log successful `control` operations at `info` with token id/name, request id,
  selector/port id, and outcome. Do not log firmware bytes or serial payloads.
- Successful `read` auth can be `debug` to avoid noise.
- Log admin operations at `info` with the actor token id, operation
  (`create`/`revoke`/`revoke_all`), target id/name, and count. Never log the new
  token, its hash, or the request body.
- `last_used_at` is not persisted. The server's only API DB writes are token
  administration; nothing writes per request. A future `last_used_at` feature needs
  a dedicated writer or batched queue rather than a write in the auth hot path.
- Token create/revoke and server add/remove log the affected name/id, never the
  secret. SQLite's `secure_delete` clears deleted credential/token content.
- Ensure reverse proxies and log shippers redact `Authorization`.

### OpenAPI and generated clients

`boot-api-client` is generated from the OpenAPI spec at build time. Adding
`/v1/tokens` changes the generated client; existing hardware and WebSocket method
signatures must stay the same. Apply auth through the shared reqwest client:

```rust
fn api_client(server: &ServerEntry) -> Result<boot_api_client::Client> {
    let token = server.token.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Server '{}' has no token. Run: boot set-token {} --token-stdin",
            server.name,
            server.name
        )
    })?;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?,
    );

    let mut builder = reqwest::ClientBuilder::new()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none());

    if let Some(ca) = &server.ca_cert {
        let pem = std::fs::read(ca)?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }

    Ok(boot_api_client::Client::new_with_client(
        &server.url,
        builder.build()?,
    ))
}
```

This works for normal requests and generated WebSocket-upgrade methods because
reqwest applies default headers to the upgrade request.

In `openapi()`, post-process the JSON to add:

```json
"components": {
  "securitySchemes": {
    "bootAuth": {
      "type": "http",
      "scheme": "bearer",
      "bearerFormat": "boot_v1"
    }
  }
}
```

Add `"security": [{"bootAuth": []}]` to every operation except `GET /v1/health`,
which gets `"security": []`. Add `401`/`403` responses to protected operations,
including all four `/v1/tokens` methods. The `Auth` extractor contributes no
parameters.

Implementation checks:

- Confirm that Dropshot 0.17.1 emits an OpenAPI document that progenitor 0.11 can
  consume. Regenerate `boot-api-client` and run
  `cargo build -p boot-api-client`.
- Confirm the generated client exposes the four token operations and that
  `boot admin --server` can call them with the existing `api_client()` helper.
- Verify progenitor does not generate an extra `authorization` function parameter
  from the security scheme. If it does, keep the spec accurate and adapt the client
  wrapper, or add the scheme in a doc-only post-processing step. The CLI helper is
  the single place that attaches the header either way.

## CLI surface

### Command naming

All server-side token operations live under the `boot admin` group. The group keeps
the verb-noun leaf form used elsewhere:

```text
boot admin create-token
boot admin list-tokens
boot admin revoke-token
boot add-server
boot set-token
boot check-auth
boot start-server
boot run-server
```

There are no top-level `create-token`, `list-tokens`, or `revoke-token` commands
and no deprecated aliases. Client-credential commands (`add-server`, `set-token`,
`check-auth`, `list-servers`, `remove-server`) stay top-level because they change
local CLI state, not the server's token table.

Do not add noun-verb forms such as `boot admin token create` or
`boot auth set-token`.

The existing `probe-run`, `probe-attach`, `serial-shell`, and `api-desc` commands
predate this convention. They are out of scope for the auth change; if strict
consistency is desired, add verb-noun aliases (`run-probe`, `attach-probe`,
`open-serial`, `describe-api`) in a separate change with a deprecation window.

### Server-side token management

```text
boot admin [--server <name>] create-token --name <name>
                [--read-only | --scope <scope>]...
                [--expires-in <duration>] [--replace] [--save-to <server>]
boot admin [--server <name>] list-tokens
boot admin [--server <name>] revoke-token <id-or-name> [--yes]
boot admin [--server <name>] revoke-token --all [--yes]
```

Target selection:

- Default: write the local API DB directly (honors the global `--api-db` path).
  This works with the server stopped, over SSH, and as the recovery path if every
  admin token is lost.
- `--server <name>`: call `/v1/tokens` with the credential stored for that server
  entry. The credential must have the `admin` scope.
- An explicit `--api-db` together with `--server` is an error.
- Remote mode uses the same TLS/CA settings as other commands and never sends the
  token in a URL.

Behavior:

- `--scope` is repeatable. `--scope admin` creates an admin token and prints a
  warning, because it can mint and revoke every other token.
- `--replace` rotates a same-named token atomically. Replacing the token used by
  the current HTTP request is allowed; the response contains the new secret.
- `--save-to <server>` stores the new token in the CLI DB after success. This works
  for both direct and remote mode. If the CLI DB write fails, the token remains
  valid and has been printed; use `boot set-token` to store it.
- `revoke-token` asks for confirmation when the selector is the token used for the
  request, and `--all` always asks for confirmation. `--yes` is required in
  non-interactive use; it does not skip the warning text.
- Listing marks the requesting token with `current` in remote mode. Direct mode has
  no request identity, so it shows no marker.
- Direct writes are soon picked up by the reloader; HTTP admin writes update the
  snapshot immediately.
- If the API DB is locked or unwritable, the command fails with a clear error.

### Client-side credentials

```text
boot add-server <name> <url> [--token-stdin | --token-file <path> | --token <token>]
                             [--ca-cert <pem>]
boot set-token <server> [--token-stdin | --token-file <path>]
boot check-auth <server>
boot list-servers
boot remove-server <name>
```

- Prefer `--token-stdin` or a prompt over `--token`: command-line secrets appear in
  shell history and process listings.
- `--token-file` reads a file without echoing the secret.
- `BOOT_TOKEN` and `BOOT_TOKEN_FILE` are available for CI, with the caveat that
  environment variables are visible to same-user processes. `BOOT_TOKEN` overrides
  the stored credential for that command but is not persisted.
- `add-server` inserts the server and any supplied credential in one transaction.
  A missing token adds the server with a warning; `list-servers` shows `token:
  missing` and protected commands fail with the actionable `401` message.
- `set-token` requires an existing server and updates its credential in place. It
  is also how an admin token is stored for `boot admin --server`.
- `check-auth` calls the public health endpoint, a read endpoint, and
  `GET /v1/tokens`; it reports "ok", "unreachable", "missing token", "invalid
  token", "token lacks read scope", and whether the token has `admin` scope.
- `list-servers` shows `token: stored/missing` and the TLS mode, never the token.
- `remove-server` removes the server and its credential. It does **not** revoke the
  server-side token; run `boot admin revoke-token` against the server (locally or
  with `--server`).
- `boot start-server` stores the auto-created admin credential for `local` and
  passes `--api-db` and `--cli-db` to the systemd unit.
- `boot stop-server` stops the unit but leaves the local server row and credential
  intact.

### Server flags

```text
boot run-server [--host <host>] [--port <port>]
                [--api-db <path>] [--cli-db <path>]
                [--no-provision]
                [--tls-cert <pem> --tls-key <pem>]
                [--allow-insecure-http]
                [--advertise-url <url>]
```

`boot start-server` accepts the same flags and forwards them to `run-server`.
`systemd_service.rs` must pass `--api-db` and `--cli-db` explicitly so the service uses
the same databases even if its working directory or `HOME` differs.

- `--no-provision` skips creating the local token, `local` server entry, and CLI
  credential. It does not skip migrations, change the API DB write mode, or disable
  `/v1/tokens`. Use it when tokens are managed entirely out of band; the API can
  start with an empty token table and protected routes return `401` until a token is
  created.
- `--advertise-url` controls the URL stored for the `local` server entry. If it is
  omitted and the bind address is `0.0.0.0` or `::`, fall back to `localhost` (or
  the `--host` value when it is connectable). Format IPv6 bind addresses with
  brackets. Without this, the auto-provisioned local credential points at an
  address no client can use.
- Use `https://` in the advertised URL when TLS is enabled.

## Code changes

| File | Change |
| --- | --- |
| `Cargo.toml` | `dropshot = "0.17"`; add `rusqlite = { version = "0.41", features = ["bundled"] }` and `sha2 = "0.11"`. Record `rust-version = "1.85"`. |
| `boot-api/Cargo.toml` | Add `sha2` and `directories`; make `base64` non-optional and remove it from the `run` feature list; add optional `rand`, `rusqlite`; include the DB module in the `run` feature. |
| `boot-api/src/auth.rs` (new) | Token format, hashing, scopes, `AuthState` snapshot, `Auth` extractor. |
| `boot-api/src/api_db.rs` (new, `#[cfg(feature = "run")]`) | API DB open/schema setup, read and write connection helpers, token create/list/revoke/load, reloader, `data_version` support. |
| `boot-api/src/lib.rs` | `pub mod auth`; add `Auth<Read>`/`Auth<Control>`/`Auth<Admin>` args; add `/v1/tokens` endpoint types and methods; post-process OpenAPI security. |
| `boot-api/src/run.rs` | `type Context = AuthState`; open/setup/provision API DB; open the admin writer; implement token handlers; spawn reloader; TLS/exposure checks; `--api-db`, `--cli-db`, `--no-provision`, `--advertise-url`. |
| `boot-api/src/probe.rs`, `serial_port.rs` | Change `RequestContext<()>` to `RequestContext<AuthState>`; pass `Principal` into WebSocket handlers for revalidation. |
| `boot-cli/Cargo.toml` | Add `rusqlite` (bundled) for the CLI DB. |
| `boot-cli/src/cli_db.rs` (new) | CLI DB open/schema setup, server/credential CRUD. |
| `boot-cli/src/admin.rs` (new) | `boot admin` subcommand tree; direct API DB and `--server` HTTP modes; create/list/revoke; confirmation and `current` warnings. |
| `boot-cli/src/auth.rs` (new) | Client credential lookup, `set-token`, `check-auth` (including admin-scope probe). |
| `boot-cli/src/server_config.rs` | Replace JSON load/save with DB-backed `ServerEntry`/`ServerConfig`. |
| `boot-cli/src/main.rs` | Add the `Admin` subcommand and global `--cli-db`/`--api-db` handling; remove top-level token commands. |
| `boot-cli/src/server.rs` | Prepare both DBs before starting the unit; auto-provision an admin `local` token; upsert `local`; token/TLS flags; non-loopback guard; pass `--api-db`/`--cli-db` to systemd; stop deleting local config in `stop-server`. |
| `boot-cli/src/systemd_service.rs` | Forward `--api-db`, `--cli-db`, TLS, and advertise flags; never print secrets. |
| `boot-cli/src/probe.rs`, `serial_port.rs` | Replace `Client::new` with `api_client()`; map 401/403; use the same client for upgrades. |
| `boot-api-client/build.rs` | No code change; regenerate from the updated spec (including `/v1/tokens`) and validate against Dropshot 0.17.1. |

Shared API DB logic lives in `boot-api` so the server and CLI use the same token
hashing, schema, and format. The CLI DB lives in `boot-cli`.

## Testing

### Unit tests (`boot-api/src/auth.rs`, `api_db.rs`)

- Generated tokens have the expected prefix, length, and 256-bit entropy.
- Header parsing: valid, lowercase scheme, extra spaces, missing scheme, `Basic`,
  empty token, internal whitespace, multiple `Authorization` headers, non-UTF-8.
- Unknown, malformed, expired, and revoked tokens all produce the same `401`.
- `read` cannot access `control`; `control` can access `read`; `admin` implies both.
- `control` and `read` tokens get `403` on every `/v1/tokens` method.
- Admin create returns the plaintext token once and stores only its hash; list
  never returns either; revoke deletes the row.
- `POST /v1/tokens` with a duplicate name is `409`; with `replace` it revokes and
  inserts in one transaction.
- `POST /v1/tokens` with an empty, unknown, or malformed scope is `400`.
- `DELETE /v1/tokens` without `all=true` is `400` and deletes nothing.
- `DELETE /v1/tokens/{selector}` for an unknown selector is `404`.
- Revoking the caller's own token succeeds and the response is still sent.
- `--no-provision` still runs migrations, serves hardware endpoints, and allows
  admin create/revoke; it only skips the `local` token, server entry, and CLI
  credential.
- API DB schema setup is idempotent and rejects a newer schema version.
- Token create/list/revoke round-trips; names are unique; `--replace` revokes first.
- The stored value is a 32-byte SHA-256 hash, not the plaintext token.
- Token names containing SQL metacharacters are handled as bound parameters.
- A hand-edited weak token is rejected by format validation.
- Reload on `data_version` change revokes a removed token; a deleted DB fails
  closed; a transient read error keeps the last good snapshot.
- A token inserted through the API DB write path is accepted immediately, and a
  later poller pass does not drop or duplicate it.
- The API DB rejects group/world-readable permissions.

### Integration tests

- `GET /v1/health` without a token → `200`.
- `GET /v1/probes` without a token → `401` with `WWW-Authenticate`.
- `GET /v1/probes` with a read token → `200`.
- `POST /v1/probes/{selector}` with a read token → `403` before touching hardware.
- `POST /v1/probes/{selector}` with a control token and an empty body → `400`, not
  `401` (auth ran first).
- WebSocket attach without a token → `401`; with a control token the upgrade is
  accepted and then fails at probe lookup if no hardware is present.
- Revoke a token while a WebSocket is open; the connection closes within the
  revalidation interval.
- Create a token through `POST /v1/tokens` and immediately use it on `GET /v1/probes`:
  it must work without waiting for the reloader.
- Revoke through `DELETE /v1/tokens/{name}` and immediately reuse it: it must be
  rejected without waiting for the reloader.
- The auto-provisioned `local` token has `read,control,admin` and can call
  `/v1/tokens`.
- The server applies migrations on startup even with `--no-provision`, and
  `/v1/tokens` remains usable.
- `openapi()` includes the bearer scheme on every path except `/v1/health`, and the
  `/v1/tokens` operations declare `401`/`403`.
- `cargo build -p boot-api-client` succeeds after the Dropshot 0.17 update and spec
  change, and the generated client still compiles with `api_client()`.

### CLI tests

- `boot start-server` on an empty API DB creates an admin `local` token and stores
  its plaintext in the CLI DB only.
- `boot admin create-token` writes `0600`, prints one secret, and
  `boot admin list-tokens` does not.
- `boot admin revoke-token` removes the row and hot reload takes effect.
- `boot admin --server pi ...` calls `/v1/tokens`, uses the stored credential, and
  fails with the admin-scope message for a non-admin token.
- `boot admin --server pi` with `--api-db` is rejected as conflicting.
- `boot admin revoke-token --all` requires confirmation; `--yes` is needed
  non-interactively.
- `boot admin revoke-token <current-token>` warns before deleting.
- `boot admin create-token --save-to local` stores the new token in the CLI DB; a
  failed CLI DB write still leaves a valid printed token.
- `boot set-token` writes `0600`; `boot check-auth` reports the admin-scope state.
- `boot add-server` stores server and credential atomically; a missing token warns
  and `list-servers` shows it.
- Top-level `create-token`, `list-tokens`, and `revoke-token` are not accepted.
- The CLI never reads `servers.json`; only `boot-cli.sqlite3` is used.
- `boot stop-server` does not delete the `local` row or credential.
- The `api_client` helper attaches `Authorization` and disables redirects (test the
  request builder, or use a local mock server).

### Manual smoke test

```console
$ boot start-server
$ boot list-probes
$ curl -i http://127.0.0.1:46673/v1/probes                 # 401
$ curl -i -H "Authorization: Bearer $TOKEN" http://127.0.0.1:46673/v1/probes
$ boot admin list-tokens
$ boot admin create-token --name smoke
$ boot admin --server local list-tokens                    # HTTP admin path
$ curl -i -X POST -H "Authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    -d '{"name":"curl-test","scopes":["read"]}' \
    http://127.0.0.1:46673/v1/tokens                       # 201, secret once
$ boot admin revoke-token local
$ boot list-probes                                        # 401 + actionable message
$ boot admin revoke-token smoke --api-db ~/.local/share/boot/boot.sqlite3  # recovery
```

## Operational runbook

### First-time local setup

```console
$ boot start-server      # creates boot.sqlite3 + boot-cli.sqlite3 if needed
$ boot list-probes       # uses the stored admin local token
$ boot admin list-tokens # shows local with read,control,admin
```

### Add a remote client

On the server host:

```console
$ boot admin create-token --name laptop
boot_v1_...
```

Or from a machine that already holds an admin credential:

```console
$ boot admin --server pi create-token --name laptop
boot_v1_...
```

On the laptop:

```console
$ boot add-server pi https://pi.local:46673 --token-stdin
$ boot check-auth pi
```

For a server reached through SSH instead of TLS:

```console
$ ssh -L 46673:127.0.0.1:46673 pi
$ boot add-server pi http://127.0.0.1:46673 --token-stdin
```

### Rotate a token

1. `boot admin create-token --name laptop-2` on the server (or
   `boot admin --server pi create-token --name laptop-2`).
2. `boot set-token laptop --token-stdin` on the client, pasting the new token.
3. `boot admin revoke-token laptop` on the server (or
   `boot admin --server pi revoke-token laptop`).

Existing WebSockets on the old token close within the revalidation interval.
Because admin writes update the snapshot immediately, the new token works on the
first request.

### Lost or stolen token

```console
$ boot admin revoke-token <id-or-name>
```

If the token is unknown, revoke all and reissue:

```console
$ boot admin revoke-token --all
$ boot admin create-token --name <client>
```

If the stolen token is the only admin credential, use filesystem recovery on the
server host:

```console
$ boot admin revoke-token --all --api-db ~/.local/share/boot/boot.sqlite3
$ boot admin create-token --name local --scope admin --save-to local
```

Review server logs for the token id, request id, remote address, and control
operations performed before revocation. Admin operations are logged with the actor
token id and the affected token.

### Exposing beyond loopback

Prefer SSH forwarding or an encrypted overlay. If a direct bind is required:

```console
$ boot start-server --host 0.0.0.0 \
    --tls-cert /etc/boot/cert.pem --tls-key /etc/boot/key.pem
```

or, only on an encrypted overlay:

```console
$ boot start-server --host 0.0.0.0 --allow-insecure-http
```

The second form prints a warning and shows plaintext mode in `boot list-servers`.

### Backup and incident response

- Stop the server/CLI or use SQLite's backup command; do not copy a live database
  with ordinary `cp`:
  `sqlite3 ~/.local/share/boot/boot.sqlite3 ".backup '/secure/boot.sqlite3.bak'"` and
  `sqlite3 ~/.local/share/boot/boot-cli.sqlite3 ".backup '/secure/boot-cli.sqlite3.bak'"`.
- Exclude the CLI DB from backups and cloud sync: it contains plaintext client
  tokens.
- The API DB contains hashes, not secrets, but protect it anyway.
- On suspected compromise: `boot admin revoke-token --all`, reissue tokens, inspect
  logs, and rotate any firmware or serial secrets that may have been exposed through
  the API.
- Admin endpoints make remote revocation possible, but the local `--api-db` path
  remains the recovery path when no admin credential is available. Keep shell access
  to the server host.
- If a database is corrupted, restore the last good backup and reissue tokens rather
  than hand-editing it.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| JSON auth/credentials files | No transactions, partial-write and parse-error states, no uniqueness constraints, and the user requirement is SQLite. |
| One SQLite database for the API DB and CLI DB | Simpler, but mixes server auth hashes with plaintext client tokens; two databases preserve least privilege and backup policy. |
| `sled`/`redb` embedded key-value store | No SQL, weaker inspection/backup tooling, and more custom migration code for little benefit. |
| Static shared secret in the CLI DB | No per-client revocation, names, or audit; rotation disrupts everyone. |
| HTTP Basic | Sends a reusable password each request; needs password hashing; no benefit over bearer tokens. |
| Local-only token administration | Avoids HTTP admin surface, but forces SSH access for every rotation and cannot revoke from a laptop. Keep the local path as recovery, not the only path. |
| mTLS | Strong, but certificate lifecycle across laptops, Pi, and CI is heavy. Keep as a future high-assurance option. |
| OIDC/SSO | Requires an IdP, network dependency, and token introspection; overkill for a local dev tool. |
| JWT | Stateless verification but revocation needs a denylist or short TTL, plus key management and clock-skew handling. |
| Loopback/Unix socket only | Secure for local-only use, but the project explicitly supports remote probes and Pi deployments. Keep loopback as the default transport, not the only one. |
| SSH tunnel only | Excellent transport, and still recommended, but it does not distinguish local processes or provide app-level audit/scopes. |
| Tailscale ACLs only | Good network-layer identity, but app-level tokens are still useful for defense in depth and local use. |

## Future work

- **Admin token UX:** `boot admin` list/create/revoke exists in this design; future
  work can add `boot admin rotate-token`, token descriptions, and per-token last-use
  display.
- **Admin-specific rate limits and audit retention:** separate limits for `/v1/tokens`
  and a durable audit log for token creation/revocation.
- **mTLS:** optional client certificates for machine identity, combined with tokens.
- **OIDC device flow:** for teams that already have an IdP; map groups to scopes.
- **Per-resource scopes:** `probes:<selector>` or `serial:<port>` ACLs.
- **Short-lived tokens/refresh:** reduce the blast radius of theft.
- **Rate limiting and connection quotas:** per-IP failure limits, per-token
  WebSocket limits, idle timeouts.
- **Persisted `last_used_at`:** the admin writer connection makes this possible; use
  a batched or sampled write off the auth hot path instead of writing on every
  request.
- **Secret-scanning integration:** register the `boot_v1_` pattern with GitHub and
  add a gitleaks rule.
- **`boot init-tls`:** generate a self-signed CA/certificate for Pi deployments.
- **Database maintenance commands:** `boot backup-db` and `boot compact-db` built
  on SQLite's backup/VACUUM APIs.
- **Verb-noun aliases:** `run-probe`, `attach-probe`, `open-serial`, `describe-api`
  for the remaining legacy command names.
