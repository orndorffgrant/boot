#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Build a deterministic source archive from a clean tagged commit.
#
# Local builds use boot-api/build.rs fallback generation instead. This script
# is the release path and is also used by the release workflow.

set -euo pipefail

cd "$(dirname "$0")/.."

output="target/source/boot-source.tar"

usage() {
    cat <<'EOF'
Usage: tools/source-archive.sh [--release] [--output PATH]

Creates a source tar from HEAD. The working tree must be clean.

Options:
  --release       Explicitly select release mode (the default behavior).
  --output PATH   Output tar path. Default: target/source/boot-source.tar.
  -h, --help      Show this help.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --release)
            shift
            ;;
        --output)
            if [ "$#" -lt 2 ]; then
                echo "error: --output requires a path" >&2
                exit 1
            fi
            output="$2"
            shift 2
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

if [ -n "$(git status --porcelain)" ]; then
    echo "error: the working tree is not clean; release source archives must" >&2
    echo "       be generated from a committed tree" >&2
    exit 1
fi

for required in LICENSE NOTICE Cargo.toml Cargo.lock; do
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

mkdir -p "$(dirname "$output")"
listing=$(mktemp)
meta="${output%.tar}.meta"
trap 'rm -f "$listing"' EXIT

git archive --format=tar --prefix="boot-$version/" -o "$output" HEAD
tar -tf "$output" > "$listing"

for required in LICENSE NOTICE Cargo.toml Cargo.lock boot-api/build.rs boot-cli/src/main.rs; do
    if ! grep -Fxq "boot-$version/$required" "$listing"; then
        echo "error: source archive is missing boot-$version/$required" >&2
        exit 1
    fi
done

if grep -Eq '(^|/)(target|\.git|dist|ext)/|(^|/)\.env|(^|/)(boot\.sqlite3|boot-cli\.sqlite3)|\.(sqlite3|sqlite|db|key|pem|crt|log|cast)(-wal|-shm)?$' "$listing"; then
    echo "error: source archive contains a forbidden path" >&2
    grep -E '(^|/)(target|\.git|dist|ext)/|(^|/)\.env|(^|/)(boot\.sqlite3|boot-cli\.sqlite3)|\.(sqlite3|sqlite|db|key|pem|crt|log|cast)(-wal|-shm)?$' "$listing" >&2
    exit 1
fi

missing=$(comm -23 \
    <(git ls-files | LC_ALL=C sort) \
    <(sed "s|^boot-$version/||" "$listing" | grep -v '/$' | grep -v '^pax_global_header$' | LC_ALL=C sort))
if [ -n "$missing" ]; then
    echo "error: source archive is missing tracked files:" >&2
    echo "$missing" >&2
    exit 1
fi

sha256=$(sha256sum "$output" | awk '{print $1}')
commit=$(git rev-parse HEAD)

cat > "$meta" <<EOF
version=$version
commit=$commit
dirty=false
sha256=$sha256
EOF

sha256sum "$output" > "$output.sha256"

echo "Source archive: $output"
echo "Metadata:       $meta"
echo "SHA-256:        $sha256"
