# boot

`boot` flashes, debugs, and observes embedded targets through a local or remote
server and a command-line client.

The workspace contains:

- `boot-protocol` — shared wire types.
- `boot-api` — HTTP/WebSocket API, runtime, store, and hardware workers.
- `boot-api-client` — generated typed HTTP client.
- `boot-cli` — the `boot` command-line client and local server launcher.

## License

This project is licensed under the GNU Affero General Public License, version 3
or later. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

Every built `boot` binary embeds a tar archive of the first-party source used
to build that exact binary. A running server offers the two-level source offer:

```text
GET /v1/source                 first-party source tar
GET /v1/source/dependencies    redirect to the dependency source bundle
GET /v1/source/manifest        dependency bundle metadata
```

These endpoints are public and do not require an API token. They are the
network source offer required by AGPLv3 section 13. Release source archives and
their SHA-256 checksums are also attached to each GitHub Release.

To download and rebuild the source offered by a running server:

```bash
curl -fSsOJ http://127.0.0.1:46673/v1/source
sha256sum boot-*-source.tar
tar -xf boot-*-source.tar
cd boot-*/
cargo build --release --locked
```

The dependency bundle is xz-compressed and available from
`GET /v1/source/dependencies` or the release assets.

## Development

```bash
cargo test --workspace
cargo fmt --check
cargo deny check licenses
```

Tests do not require real hardware. See [AGENTS.md](AGENTS.md) for the
architecture and contribution rules.
