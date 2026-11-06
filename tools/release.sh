#!/usr/bin/env bash
# Bump the workspace CalVer version, commit it, and create an annotated tag.
#
# Usage: tools/release.sh
#
# The tag is not pushed automatically. After this script succeeds, run:
#   git push origin main
#   git push origin v<version>
set -euo pipefail

cd "$(dirname "$0")/.."

if [ -n "$(git status --porcelain)" ]; then
    echo "error: the working tree is not clean; commit or stash changes first" >&2
    exit 1
fi

current=$(python3 - <<'PY'
import tomllib

with open("Cargo.toml", "rb") as handle:
    print(tomllib.load(handle)["workspace"]["package"]["version"])
PY
)

next=$(python3 - "$current" <<'PY'
import datetime
import sys

current = sys.argv[1]
now = datetime.datetime.now(datetime.timezone.utc)
prefix = f"{now.year}.{now.month}{now.day:02d}"

parts = current.split(".")
if len(parts) == 3 and f"{parts[0]}.{parts[1]}" == prefix:
    counter = int(parts[2]) + 1
else:
    counter = 0

print(f"{prefix}.{counter}")
PY
)

tag="v$next"
if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
    echo "error: tag $tag already exists" >&2
    exit 1
fi

echo "Bumping workspace version: $current -> $next"
python3 - "$next" <<'PY'
import re
import sys

version = sys.argv[1]
path = "Cargo.toml"
with open(path, "r", encoding="utf-8") as handle:
    lines = handle.readlines()

in_workspace_package = False
replaced = False
for index, line in enumerate(lines):
    if line.strip() == "[workspace.package]":
        in_workspace_package = True
        continue
    if in_workspace_package and line.lstrip().startswith("["):
        break
    if in_workspace_package and re.match(r"^version\s*=", line):
        lines[index] = f'version = "{version}"\n'
        replaced = True
        break

if not replaced:
    raise SystemExit("error: could not find version in [workspace.package]")

with open(path, "w", encoding="utf-8") as handle:
    handle.writelines(lines)
PY

# Refresh Cargo.lock with the new workspace versions.
cargo metadata --format-version 1 >/dev/null

git add Cargo.toml Cargo.lock
git commit -m "Release $tag"
git tag -a "$tag" -m "Release $tag"

echo
echo "Created $tag."
echo "Publish it with:"
echo "  git push origin $(git branch --show-current)"
echo "  git push origin $tag"
