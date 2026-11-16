# License and source offer

`boot` is licensed under the GNU Affero General Public License, version 3 or
later (`AGPL-3.0-or-later`). See [../LICENSE](../LICENSE) and
[../NOTICE](../NOTICE).

## Two-level source offer

A running server exposes two source levels:

```text
GET /v1/source                 first-party source tar
GET /v1/source/dependencies    redirect to the dependency source bundle
GET /v1/source/manifest        dependency bundle metadata
```

These endpoints are public and intentionally unauthenticated. Do not put them
behind an auth proxy: AGPLv3 section 13 requires the offer to reach every user
interacting with the server over the network.

### First-party source

Every built binary embeds a plain tar archive of the first-party source used to
build that exact binary. `GET /v1/source` returns:

- `Content-Type: application/x-tar`
- `Content-Disposition: attachment; filename="boot-<version>-source.tar"`
- an `ETag` equal to the archive SHA-256
- `X-Boot-Source-Sha256` and `X-Boot-Source-Revision` headers

### Dependency source

The dependency source bundle is a full Linux normal-and-build dependency
vendor tree for both release targets, compressed with xz. It is published as a
GitHub Release asset and offered through:

- `GET /v1/source/dependencies` — redirects to the immutable release asset;
- `GET /v1/source/manifest` — JSON with the URL, SHA-256, filename, targets,
  and package count.

The bundle contains:

```text
boot-<version>-dependencies/
  Cargo.toml
  Cargo.lock
  LICENSE
  NOTICE
  DEPENDENCY-SOURCES.json
  THIRD_PARTY_NOTICES.md
  SOURCE-OFFER.md
  vendor/...
```

`GET /v1/health` and `GET /v1/server` report the license, first-party source
hash, dependency bundle URL/hash, and dependency package count.

## Release assets

Each GitHub Release includes:

```text
boot-<version>-source.tar
boot-<version>-source.tar.sha256
boot-<version>-dependencies.tar.xz
boot-<version>-dependencies.tar.xz.sha256
```

The source archive is byte-for-byte the archive embedded in both release
binaries. The dependency bundle is the same bytes offered by
`/v1/source/dependencies`.

## Rebuilding first-party source

```bash
curl -fSsOJ http://127.0.0.1:46673/v1/source
sha256sum boot-<version>-source.tar
tar -xf boot-<version>-source.tar
cd boot-<version>
cargo build --release --locked
```

The archive contains the Cargo manifests, `Cargo.lock`, build scripts,
migrations, SQLx query cache, documentation, and release tooling. Cargo
dependencies are resolved from `Cargo.lock`.

## Rebuilding with dependency source

```bash
curl -fSsOJ http://127.0.0.1:46673/v1/source/dependencies
sha256sum boot-<version>-dependencies.tar.xz
tar -xJf boot-<version>-dependencies.tar.xz
```

To build offline from the vendored dependency tree, point Cargo at the
extracted `vendor/` directory:

```toml
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "/path/to/boot-<version>-dependencies/vendor"
```

## Local and modified builds

Release archives are generated from a clean tagged commit with
`tools/source-archive.sh` and `tools/dependency-source.sh`. Local builds use
the `boot-api/build.rs` working-tree fallback so a locally modified binary
offers the first-party source that was actually compiled.

A local server publishes its workspace source to anyone who can reach
`/v1/source`. The archive excludes `.git/`, `target/`, `dist/`, `ext/`, local
databases, `.env` files, TLS private keys, logs, and similar local state.

For a local dependency-source offer, generate the bundle and point the build at
its metadata:

```bash
tools/dependency-source.sh \
  --output target/source/boot-<version>-dependencies.tar.xz \
  --url https://example.invalid/boot-<version>-dependencies.tar.xz

BOOT_DEPENDENCY_SOURCE_META=target/source/boot-dependencies.meta \
  cargo build --release
```

If the bundle URL is empty, `/v1/source/dependencies` returns `503` and the
manifest reports `available: false`.

If you patch a deployed binary without rebuilding it, or run a source tree other
than the one embedded in the binary, the automatic offer no longer describes
the running version. Rebuild from the modified source instead.

## Updates and rollback

`boot update` replaces the binary; after the service restarts, the new binary
serves its own embedded source and dependency metadata. With `--no-restart`,
the old process continues serving the old source, which still matches the
running binary. A failed update or rollback leaves the old binary and old
source in place.

## Dependency licenses

`deny.toml` records the dependency license policy. CI runs:

```bash
cargo deny check licenses
```

The allowlist covers permissive licenses, MPL-2.0, GPLv3, AGPLv3, and
GPL-2.0-or-later. `GPL-2.0-only` is deliberately denied because it is not
compatible with AGPLv3. LGPL and other weak-copyleft licenses require a
relinking/source review and should be added as per-crate exceptions.

The full Linux dependency source bundle is generated for both release targets,
so GPLv3/AGPLv3-compatible dependencies can be accepted without changing the
source-offer mechanism. Adding a dependency must still keep
`THIRD_PARTY_NOTICES.md`, `DEPENDENCY-SOURCES.json`, and the bundle in sync.
