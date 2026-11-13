# Release and Update Mechanism — Design

Status: implemented
Decisions resolved: 2026-11-04
Implemented: 2026-11-05

## Summary

Publish `boot` binaries from Git tags using GitHub Actions and GitHub
Releases, then let an installed `boot` replace itself with
`boot update`.

The design covers:

- calendar versioning for the Rust workspace;
- a tag-triggered release pipeline;
- release asset naming, checksums, and integrity guarantees;
- update discovery, replacement, service restart, and rollback behavior;
- operational and security caveats.

The release workflow, update command, and release helper are implemented. See
[releases.md](releases.md) for user-facing instructions.

## Goals

- One command cuts a release and one tag push publishes it.
- Releases are reproducible enough to rebuild or audit from source.
- `boot update` is safe by default: it verifies the download, validates
  that the new binary can run, and rolls back if the service fails to restart.
- Calendar versions are readable and sort correctly.
- The release process needs no long-lived repository secrets beyond the
  automatically provided GitHub token.
- The updater works on the two deployment targets already used by the project:
  `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.

## Non-goals

- Updating remote servers automatically. Each host runs its own update.
- Package-manager integration (`.deb`, `.rpm`, Homebrew, crates.io).
- Delta updates.
- Automatic background update checks or unattended upgrades.
- Windows, macOS, musl, or non-glibc targets.
- Replacing or changing token administration behavior.

## Versioning

### Proposed scheme

```
YYYY.MMDD.N
```

Examples:

| Date | First release | Second release |
| --- | --- | --- |
| 2026-11-03 | `2026.1103.0` | `2026.1103.1` |
| 2026-11-04 | `2026.1104.0` | — |

- `YYYY` is the UTC year.
- `MMDD` is the UTC month and day encoded as an integer, without leading
  zeros when the number is small.
- `N` is a same-day release counter starting at zero.

This keeps the three numeric components Cargo/semver requires while remaining
human-readable and chronologically sortable.

### Why not `2026.11.3.1`

Cargo package versions must be valid semver. Semver allows exactly
`major.minor.patch` plus optional pre-release/build metadata, so a fourth
numeric component is rejected. Build metadata such as `2026.11.3+1` is valid
but is ignored for ordering, which makes it unsuitable for update checks.

`YYYY.M.D` is also valid and readable, but it cannot represent a second
release on the same day without either changing the day component or relying on
non-ordering metadata. `YYYY.MMDD.N` handles that cleanly.

### Source of truth

The version should live in one place, `[workspace.package]` in the root
`Cargo.toml`. All workspace crates inherit it. Release tags are always
`v<version>`, for example `v2026.1103.0`.

### Migration

The current workspace version is `2026.1.0`. The first CalVer release
(`2026.1103.0` or later) compares greater, so no special migration logic is
needed.

## Release pipeline

### Trigger

Push a tag matching `v<semver>`, for example:

```bash
git tag -a v2026.1103.0 -m "Release v2026.1103.0"
git push origin v2026.1103.0
```

The tag is the only release trigger. Ordinary branch pushes do not publish.

### Stages

1. **Validate**
   - Parse the tag as semver.
   - Confirm it exactly matches the workspace version.
   - Determine whether it is a pre-release (contains a semver pre-release
     suffix).
   - Require the tagged commit to be reachable from the default branch.

2. **Test**
   - Run `cargo test --workspace --locked` on Linux.
   - Install native build dependencies (`libudev-dev`, `pkg-config`).

3. **Build**
   - Cross-compile `boot --release --locked` for:
     - `x86_64-unknown-linux-gnu`
     - `aarch64-unknown-linux-gnu`
   - Use `cross` for both targets so the build environment is consistent and
     libudev/pkg-config are available per architecture.

4. **Package**
   - Copy each release binary to a stable asset name.
   - Generate a SHA-256 checksum file next to each binary.

5. **Publish**
   - Create the GitHub Release.
   - Attach all binaries and checksum files.
   - Use generated release notes.
   - Mark tags with a pre-release suffix as GitHub pre-releases.
   - On a re-run, upload assets to an existing release with overwrite instead
     of failing.

### Release assets

| Asset | Content |
| --- | --- |
| `boot-x86_64-unknown-linux-gnu` | raw release binary |
| `boot-x86_64-unknown-linux-gnu.sha256` | `sha256sum` output |
| `boot-aarch64-unknown-linux-gnu` | raw release binary |
| `boot-aarch64-unknown-linux-gnu.sha256` | `sha256sum` output |

Raw binaries are preferred over tarballs because:

- the updater avoids adding archive/decompression dependencies;
- manual installation is straightforward;
- checksums cover exactly the installed bytes.

If tarballs are desired later, the updater would need archive extraction and
the asset naming should include the version.

### Permissions and secrets

- Workflow permission: `contents: write`.
- Publish with the built-in `GITHUB_TOKEN`. No new repository secrets are
  required.
- Optionally set `GITHUB_TOKEN` in the *updater's* environment to avoid
  GitHub API rate limits. This is separate from the publish workflow.

### Reproducibility and auditability

- Build with `--locked`.
- Tagged commit and release assets are linked by GitHub.
- Checksums are published in the same release.
- Artifact attestations (SLSA/GitHub) are out of scope for this design.
  Integrity relies on HTTPS, GitHub's release records, and SHA-256 checksums.

## Update command

### Command surface

Location: `boot update`, since it operates on the local installation.

```text
boot update [--check] [--version TAG] [--force] [--yes]
            [--no-restart] [--repo OWNER/REPO]
```

| Flag | Behavior |
| --- | --- |
| `--check` | Query the selected release, print installed/available versions, exit without changes |
| `--version TAG` | Install a specific tag instead of `/releases/latest` |
| `--force` | Reinstall the same version or allow a downgrade |
| `-y`, `--yes` | Skip the confirmation prompt |
| `--no-restart` | Replace the binary but leave the running service on the old process |
| `--repo OWNER/REPO` | Use a fork or mirror; also configurable with `BOOT_RELEASE_REPO` |

Default repository: `orndorffgrant/boot`.

### Update algorithm

1. Resolve the repository and optional requested tag.
2. Query GitHub Releases:
   - `/releases/latest` by default;
   - `/releases/tags/<tag>` when `--version` is supplied.
3. Parse the current compiled version and the release tag as semver.
4. Print installed and available versions, including the release URL.
5. If `--check`, report status and exit.
6. Refuse downgrades unless `--force`; refuse same-version reinstall unless
   `--force`.
7. Determine the running executable path and confirm its directory is
   writable. Fail early with a clear "run with sudo" message if not.
8. Select the asset matching the running target triple.
9. Determine whether the local `boot-api` service is active and, if possible,
   which executable path the unit uses.
10. Warn about the replacement and service restart. Require confirmation
    unless `--yes`.
11. Download the checksum asset, then the binary. Reject non-HTTPS URLs and
    oversized assets.
12. Verify the SHA-256 digest. Abort without changing anything on mismatch.
13. Write the binary to a temporary file in the same directory.
14. Execute the temporary binary with `--version` and confirm it reports the
    expected release version. This catches wrong architecture, missing runtime
    dependencies, and asset/tag mix-ups before installation.
15. Preserve the old binary's permissions on the new file.
16. Back up the old binary to a temporary file in the same directory.
17. Atomically rename the new binary over the installed path.
18. If the service is active and restarts are enabled, restart `boot-api`.
19. If the restart fails, restore the backup, restart the old service, and
    report exactly what happened.
20. Remove the temporary backup after success and print the new version.

### Warning requirements

Before making changes, the command must warn that:

- the current executable will be replaced;
- `boot-api` will restart and in-flight flash, debug, and serial operations
  will be interrupted;
- when `--no-restart` is used, the service keeps running the old binary until
  a manual restart;
- when the service's configured executable differs from the invoking binary,
  the service may not actually be using the replaced file.

### Service handling

- Detect activity with `systemctl is-active --quiet boot-api`.
- Treat systemd as unavailable when `systemctl` is not installed; this allows
  dry runs or non-systemd environments to update the binary.
- Restart with the existing `boot restart-server` semantics.
- Never stop the service before the replacement. Replacing a running
  executable via rename is safe on Linux, and leaving the service running
  minimizes the failure window.
- If the update process dies after the swap but before the restart, the old
  process continues running; the next manual restart activates the new binary.

### Failure and rollback matrix

| Failure | Result |
| --- | --- |
| GitHub API unavailable / rate limited | Abort before download; suggest `GITHUB_TOKEN` for rate limits |
| Requested tag missing | Abort; list no available assets |
| Target asset missing | Abort and list available asset names |
| Checksum missing or malformed | Abort before touching the executable |
| Checksum mismatch | Abort; nothing changed |
| Directory not writable | Abort before download; suggest sudo |
| New binary fails `--version` | Delete temporary file; old binary untouched |
| Rename fails | Delete temporary file and backup; old binary untouched |
| Service restart fails | Restore backup, restart old service, report both outcomes |
| Rollback fails | Report the backup path and manual recovery steps |

### Version comparison

- Use semver ordering.
- Pre-releases are skipped by the default `/releases/latest` lookup.
- Explicit `--version` may install a pre-release, with a warning.
- Equal versions are a no-op unless `--force`.
- Older versions require `--force`.

## Security considerations

- All API and asset requests must use HTTPS; asset URLs from the API are
  checked before download.
- GitHub API authentication is optional and should only be attached to API
  requests. Asset downloads redirect to a different host, so the download
  client must not carry the GitHub token.
- Integrity relies on GitHub over TLS plus the published SHA-256. This
  protects against corruption and CDN issues, not a compromised release.
- The updater executes the downloaded binary once with `--version` after
  checksum verification. This is no more trust than installing it, and it
  prevents installing an unusable binary.
- Running as root is required only when the install directory is root-owned.
  The updater must not silently escalate privileges.
- Private repositories are not supported by this design unless asset
  authentication is redesigned. Releases are assumed to be publicly
  downloadable.

## Operational considerations

- `boot update` updates the local host only. Run it on every host.
- Invoke the installed binary path that systemd uses; otherwise the update may
  replace a different copy.
- Keep the release workflow and updater target list in sync. Adding a target
  requires a workflow matrix entry and updater target detection.
- Unauthenticated GitHub API access is limited (typically 60 requests/hour per
  IP). This is adequate for occasional checks; `GITHUB_TOKEN` raises the
  limit.
- The service restart is the main user-visible interruption. Operators should
  avoid updating during flash/debug/serial sessions.

## Alternatives considered

| Alternative | Why not chosen |
| --- | --- |
| Pure semver (`1.2.3`) | Less informative for a date-oriented project; CalVer requested |
| `YYYY.M.D.1` | Invalid semver |
| Build metadata for same-day releases | Valid but ignored in ordering |
| `cargo-release` | Useful, but adds configuration and still needs a custom CalVer helper |
| `cargo-dist` | Strong release tooling, but heavier than needed for two Linux binaries |
| Tarball assets | More conventional, but adds archive dependencies to the updater |
| `gh release create` in a manual script | Works, but duplicates validation and publishing logic outside CI |
| Updating all configured servers over HTTP | Expands scope and requires remote privilege/authentication design |
| In-place write to the executable | Can fail with `ETXTBSY`; rename is safer |

## Implementation outline

The implementation lives in:

```text
.github/workflows/release.yml       tag validation, tests, builds, publish
tools/release.sh                    optional CalVer bump/commit/tag helper
boot-cli/src/update.rs              update command implementation
boot-cli/src/systemd_service.rs     service activity and executable discovery
boot-cli/src/main.rs                wire `boot update` into the CLI
docs/releases.md                    user-facing release/update instructions
```

Supporting changes:

- move the workspace version into `[workspace.package]`;
- add `semver` and `sha2` to the CLI dependencies;
- add the x86_64 target to `Cross.toml`;
- add `version` to the clap command so `boot --version` works.

## Resolved decisions

| Topic | Decision |
| --- | --- |
| Tag branch policy | Tags must point at a commit reachable from the default branch. |
| Backup retention | Keep the previous binary only during the swap; remove it after success. |
| Update scope | `boot update` updates the local host only. |
| Default repository | Default to `orndorffgrant/boot`, overridable with `--repo` or `BOOT_RELEASE_REPO`. |
| Artifact attestations | Use SHA-256 checksums only for now; no attestations. |
| CalVer boundary | Evaluate the date and same-day counter in UTC. |

## Rollout plan

1. Implement the agreed CalVer scheme and release policy.
2. Add the release workflow and verify it with a pre-release tag such as
   `v2026.1103.0-rc.1`.
3. Verify both assets and checksums can be downloaded manually.
4. Implement `boot update --check` and validate version discovery.
5. Implement the install path, then test successful update, checksum failure,
   non-writable directory, and restart-failure rollback on a disposable host.
6. Publish the first stable release and document it.
