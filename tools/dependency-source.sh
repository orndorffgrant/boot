#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Build the second-level source offer: a full Linux dependency source bundle.
#
# The bundle contains the normal and build dependency graph for the Linux
# release targets, compressed with xz, plus a JSON manifest and notices.

set -euo pipefail

cd "$(dirname "$0")/.."

output=""
url="${BOOT_DEPENDENCY_SOURCE_URL:-}"
targets_csv="x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu"
xz_level=6
keep=0

usage() {
    cat <<'EOF'
Usage: tools/dependency-source.sh [OPTIONS]

Build boot-<version>-dependencies.tar.xz from Cargo.lock.

Options:
  --output PATH       Output tar.xz path.
                      Default: target/source/boot-<version>-dependencies.tar.xz.
  --url URL           Immutable public URL for the bundle. In GitHub Actions,
                      this is inferred from GITHUB_REPOSITORY and GITHUB_REF_NAME
                      when omitted.
  --targets LIST      Comma-separated targets. Default: x86_64 and aarch64 Linux.
  --xz-level LEVEL    xz compression level (default: 6).
  --keep              Keep the staging directory under target/dependency-source.
  -h, --help          Show this help.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            [ "$#" -ge 2 ] || { echo "error: --output requires a path" >&2; exit 1; }
            output="$2"
            shift 2
            ;;
        --url)
            [ "$#" -ge 2 ] || { echo "error: --url requires a URL" >&2; exit 1; }
            url="$2"
            shift 2
            ;;
        --targets)
            [ "$#" -ge 2 ] || { echo "error: --targets requires a list" >&2; exit 1; }
            targets_csv="$2"
            shift 2
            ;;
        --xz-level)
            [ "$#" -ge 2 ] || { echo "error: --xz-level requires a value" >&2; exit 1; }
            xz_level="$2"
            shift 2
            ;;
        --keep)
            keep=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

for required in Cargo.toml Cargo.lock LICENSE NOTICE; do
    if [ ! -f "$required" ]; then
        echo "error: required source file is missing: $required" >&2
        exit 1
    fi
done

version=$(python3 - <<'PY'
import tomllib

with open("Cargo.toml", "rb") as handle:
    print(tomllib.load(handle)["workspace"]["package"]["version"])
PY
)

if [ -z "$output" ]; then
    output="target/source/boot-${version}-dependencies.tar.xz"
fi
case "$output" in
    *.tar.xz) ;;
    *)
        echo "error: dependency source output must end in .tar.xz" >&2
        exit 1
        ;;
esac

filename=$(basename "$output")
if [ -z "$url" ] && [ -n "${GITHUB_REPOSITORY:-}" ] && [ -n "${GITHUB_REF_NAME:-}" ]; then
    url="https://github.com/${GITHUB_REPOSITORY}/releases/download/${GITHUB_REF_NAME}/${filename}"
fi

mkdir -p "$(dirname "$output")"

work="target/dependency-source"
if [ "$keep" -eq 0 ]; then
    trap 'rm -rf "$work"' EXIT
fi
rm -rf "$work"
mkdir -p "$work"

echo "Vendoring dependencies..." >&2
if ! cargo vendor --locked --versioned-dirs "$work/vendor" > "$work/vendor.log" 2>&1; then
    cat "$work/vendor.log" >&2
    exit 1
fi
cargo metadata --locked --format-version 1 > "$work/metadata.json"

: > "$work/packages.txt"
for target in ${targets_csv//,/ }; do
    cargo tree \
        --locked \
        --edges normal,build \
        --target "$target" \
        --prefix none \
        --format '{p}' >> "$work/packages.txt"
done

package_count=$(python3 - "$version" "$targets_csv" "$work" <<'PY'
import json
import re
import shutil
import sys
import tomllib
from pathlib import Path

version = sys.argv[1]
targets_csv = sys.argv[2]
work = Path(sys.argv[3])
vendor = work / "vendor"
stage = work / "stage" / f"boot-{version}-dependencies"

selected = set()
for line in (work / "packages.txt").read_text().splitlines():
    line = line.strip()
    if not line or "(/" in line:
        continue
    match = re.match(r"^(\S+) v([^ ]+)", line)
    if not match:
        continue
    selected.add(match.groups())

metadata = json.loads((work / "metadata.json").read_text())
with open("Cargo.lock", "rb") as handle:
    lock = tomllib.load(handle)

metadata_by = {
    (package["name"], package["version"]): package
    for package in metadata["packages"]
}
lock_by = {
    (package["name"], package.get("version")): package
    for package in lock.get("package", [])
}

if stage.exists():
    shutil.rmtree(stage)
(stage / "vendor").mkdir(parents=True)
for name, package_version in sorted(selected):
    source = vendor / f"{name}-{package_version}"
    if not source.is_dir():
        print(f"error: vendored crate is missing: {source}", file=sys.stderr)
        sys.exit(1)
    shutil.copytree(source, stage / "vendor" / source.name, symlinks=True)

for name in ("Cargo.toml", "Cargo.lock", "LICENSE", "NOTICE"):
    source = Path(name)
    if source.is_file():
        shutil.copy2(source, stage / name)

packages = []
for name, package_version in sorted(selected):
    package_meta = metadata_by.get((name, package_version), {})
    package_lock = lock_by.get((name, package_version), {})
    vendor_dir = stage / "vendor" / f"{name}-{package_version}"
    license_files = []
    for candidate in sorted(vendor_dir.rglob("*")):
        if not candidate.is_file():
            continue
        upper = candidate.name.upper()
        if upper.startswith(("LICENSE", "COPYING", "NOTICE", "UNLICENSE")):
            license_files.append(str(candidate.relative_to(vendor_dir)))
    source = package_meta.get("source") or package_lock.get("source") or ""
    if source.startswith("registry+"):
        download_url = f"https://crates.io/api/v1/crates/{name}/{package_version}/download"
    else:
        download_url = ""
    packages.append({
        "name": name,
        "version": package_version,
        "license": package_meta.get("license") or "UNKNOWN",
        "repository": package_meta.get("repository") or "",
        "source": source,
        "checksum": package_lock.get("checksum") or "",
        "download_url": download_url,
        "vendor_path": f"vendor/{name}-{package_version}",
        "license_files": license_files,
    })

manifest = {
    "format": 1,
    "boot_version": version,
    "targets": [target.strip() for target in targets_csv.split(",") if target.strip()],
    "package_count": len(packages),
    "packages": packages,
}
(stage / "DEPENDENCY-SOURCES.json").write_text(
    json.dumps(manifest, indent=2, sort_keys=True) + "\n"
)

notices = [
    "# Third-party dependency notices",
    "",
    f"boot {version}",
    "",
    "This archive contains the source for the third-party Rust crates used by",
    "the Linux release builds. The authoritative license text for each crate is",
    "included in that crate's source directory under `vendor/`.",
    "",
]
for package in packages:
    notices.extend([
        f"## {package['name']} {package['version']}",
        "",
        f"License: {package['license']}",
        f"Repository: {package['repository'] or 'unknown'}",
        f"Source: {package['download_url'] or package['source'] or 'unknown'}",
    ])
    if package["license_files"]:
        notices.append(
            "License files: " + ", ".join(package["license_files"])
        )
    notices.append("")
(stage / "THIRD_PARTY_NOTICES.md").write_text("\n".join(notices) + "\n")

(stage / "SOURCE-OFFER.md").write_text(
    f"""# Dependency source offer

This archive is the dependency source bundle for `boot {version}`.

- First-party source: `GET /v1/source`
- Dependency source manifest: `GET /v1/source/manifest`
- Dependency source redirect: `GET /v1/source/dependencies`

The bundle contains the normal and build dependency graph for:

{chr(10).join(f'- `{target.strip()}`' for target in targets_csv.split(',') if target.strip())}

`DEPENDENCY-SOURCES.json` records the exact versions, license expressions,
checksums, upstream download URLs, and vendor paths. The `vendor/` directory
contains the complete crate source and its license files.
"""
)

print(len(packages))
PY
)

echo "Compressing dependency source bundle..." >&2
tar \
    --sort=name \
    --mtime=@0 \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$work/stage" \
    -cf - "boot-${version}-dependencies" |
    xz -"$xz_level" -T0 -c > "$output"

sha256=$(sha256sum "$output" | awk '{print $1}')
sha256sum "$output" > "$output.sha256"

meta="target/source/boot-dependencies.meta"
mkdir -p "$(dirname "$meta")"
cat > "$meta" <<EOF
version=${version}
filename=${filename}
url=${url}
sha256=${sha256}
targets=${targets_csv}
packages=${package_count}
EOF

echo "Dependency source: $output"
echo "Metadata:          $meta"
echo "Packages:          $package_count"
echo "SHA-256:           $sha256"
