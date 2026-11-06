# Releases and updates

`boot` is released from Git tags and published as GitHub Releases. Installed
copies can update themselves with `boot admin update`.

## Versioning

Versions use calendar versioning:

```text
YYYY.MMDD.N
```

- `YYYY` is the UTC year.
- `MMDD` is the UTC month and day encoded as an integer (for example,
  `2026.1103.0` for 3 November 2026).
- `N` is a same-day release counter starting at zero.

The version lives in `[workspace.package]` in the root `Cargo.toml`. Release
tags are always `v<version>`, for example `v2026.1103.0`.

## Cutting a release

1. Make sure the working tree is clean and the default branch is up to date.
2. Run the release helper:

   ```bash
   tools/release.sh
   ```

   The helper computes the next CalVer version, updates `Cargo.toml` and
   `Cargo.lock`, commits the change, and creates an annotated tag.

3. Push the commit and the tag:

   ```bash
   git push origin main
   git push origin v2026.1103.0
   ```

Pushing the tag starts the release workflow. Ordinary branch pushes do not
publish anything.

The workflow:

1. validates that the tag is semver, matches the workspace version, and points
   at a commit reachable from the default branch;
2. runs `cargo test --workspace --locked`;
3. cross-compiles `boot` for `x86_64-unknown-linux-gnu` and
   `aarch64-unknown-linux-gnu`;
4. publishes the raw binaries and their SHA-256 checksums to a GitHub Release.

Tags with a semver pre-release suffix, such as `v2026.1103.0-rc.1`, are
published as GitHub pre-releases.

### Release assets

| Asset | Content |
| --- | --- |
| `boot-x86_64-unknown-linux-gnu` | raw release binary |
| `boot-x86_64-unknown-linux-gnu.sha256` | `sha256sum` output |
| `boot-aarch64-unknown-linux-gnu` | raw release binary |
| `boot-aarch64-unknown-linux-gnu.sha256` | `sha256sum` output |

## Updating an installation

Run the update on each host that runs `boot`:

```bash
sudo boot admin update
```

The command queries GitHub Releases, verifies the download against the
published SHA-256 checksum, runs the new binary once with `--version`, and only
then replaces the installed executable. If `boot-api` is active, it is
restarted after the swap. If the restart fails, the previous binary is restored
and the old service is restarted.

### Options

```text
boot admin update [--check] [--version TAG] [--force] [--yes]
                  [--no-restart] [--repo OWNER/REPO]
```

| Flag | Behavior |
| --- | --- |
| `--check` | Print installed and available versions, then exit without changes |
| `--version TAG` | Install a specific tag instead of the latest release |
| `--force` | Reinstall the same version or allow a downgrade |
| `-y`, `--yes` | Skip the confirmation prompt |
| `--no-restart` | Replace the binary but leave the running service on the old process |
| `--repo OWNER/REPO` | Use a fork or mirror; also configurable with `BOOT_RELEASE_REPO` |

The default repository is `orndorffgrant/boot`.

### Examples

Check whether an update is available:

```bash
boot admin update --check
```

Install a specific release:

```bash
sudo boot admin update --version v2026.1103.0
```

Replace the binary without interrupting a running service:

```bash
sudo boot admin update --no-restart
```

### GitHub API rate limits

Unauthenticated GitHub API requests are limited. If checks fail with a rate
limit error, set a token for the updater:

```bash
export GITHUB_TOKEN=...
```

The token is only sent to the GitHub API. Asset downloads redirect to a
different host and do not carry it.

## Operational notes

- `boot admin update` updates the local host only. Run it on every host.
- Invoke the same binary path that systemd uses. If the unit runs a different
  path, the update warns that the service may not use the replaced file.
- A service restart interrupts in-flight flash, debug, and serial operations.
  Avoid updating during active sessions.
- The updater never stops the service before replacing the binary. Replacing a
  running executable by rename is safe on Linux.
- If the update process dies after the swap but before the restart, the old
  process keeps running; the next manual restart activates the new binary.
- Running as root is only required when the install directory is root-owned.
  The updater does not escalate privileges on its own.

## Manual recovery

If an update fails during the service restart, the command restores the
previous binary automatically. If rollback itself fails, the error message
prints the path of the saved backup. Restore it manually:

```bash
sudo mv /path/to/.boot-update-backup-... /usr/local/bin/boot
sudo systemctl restart boot-api
```
