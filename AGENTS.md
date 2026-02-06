# Codebase Overview

## Architecture
Rust cargo monorepo for embedded device flashing/debugging via HTTP API + CLI.

## Crates

### boot-api
- **Purpose**: Dropshot HTTP API + WebSocket server for probe-rs operations
- **Key deps**: dropshot, probe-rs, serialport, tokio-tungstenite
- **Features**: 
  - `default = ["run"]`
  - `run` = runtime deps (probe-rs, serialport, etc.)
- **API trait**: `BootApi` defines endpoints, impl in `run.rs`
- **Exports**: `openapi()` for spec generation, `run()` for server

### boot-api-client  
- **Purpose**: Auto-generated HTTP client from OpenAPI spec
- **Build-time**: `build.rs` calls `boot-api::openapi()` → progenitor codegen
- **Deps**: progenitor-client, reqwest, futures
- **Build deps**: boot-api (no features), progenitor, prettyplease, syn

### boot-cli
- **Purpose**: CLI tool (`boot` binary)
- **Deps**: boot-api (with "run" feature), boot-api-client
- **Functions**:
  - Server mgmt: start-server (systemd), stop-server, run-server
  - Remote ops: list-probes, list-serial-ports, run (flash), shell (serial WebSocket)
  - Config: add/remove servers via server_config.rs

## Dependency Graph
```
boot-cli
├── boot-api [features=run] (runtime: can run server locally)
└── boot-api-client
    └── boot-api (build-time only, no features)
```

## Purposes

### boot-api
- Define HTTP/WebSocket API for embedded device operations
- Provide server runtime for local probe-rs/serialport access
- Export OpenAPI spec for client generation

### boot-api-client
- Auto-generated type-safe HTTP client
- Consumed by CLI and other Rust clients

### boot-cli
- User-facing CLI for device flashing, debugging, serial access
- Manage local/remote server lifecycle (systemd integration)
- Multi-server config management

## Key Files
- `boot-api/src/lib.rs`: API trait + types, `openapi()` fn
- `boot-api/src/run.rs`: Server impl (ServerImpl), endpoint handlers
- `boot-api-client/src/lib.rs`: `include!` codegen
- `boot-api-client/build.rs`: OpenAPI → progenitor → Rust client
- `boot-cli/src/main.rs`: CLI commands, API client usage
