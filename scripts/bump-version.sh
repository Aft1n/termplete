#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  printf 'Usage: %s MAJOR.MINOR.PATCH [--reset]\n' "$0" >&2
  exit 2
fi

reset=0
if [ "$#" -eq 2 ]; then
  if [ "$2" != "--reset" ]; then
    printf 'Usage: %s MAJOR.MINOR.PATCH [--reset]\n' "$0" >&2
    exit 2
  fi
  reset=1
fi

if [ -n "$(git status --porcelain)" ]; then
  printf 'Commit or remove working-tree changes before bumping the release version.\n' >&2
  exit 1
fi

python3 - "$1" "$reset" <<'PY'
import json
import re
import sys
from pathlib import Path

version = sys.argv[1]
allow_reset = sys.argv[2] == "1"
match = re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version)
if not match:
    raise SystemExit("Version must use MAJOR.MINOR.PATCH, for example 0.1.1")

root = Path.cwd()
manifest_path = root / "Cargo.toml"
lock_path = root / "Cargo.lock"
package_path = root / "npm" / "package.json"
manifest = manifest_path.read_text()
lock = lock_path.read_text()
package = json.loads(package_path.read_text())

manifest_match = re.search(r'(?m)^version = "([^"]+)"$', manifest)
lock_match = re.search(
    r'(?m)^\[\[package\]\]\nname = "termplete"\nversion = "([^"]+)"$', lock
)
if not manifest_match or not lock_match or package.get("name") != "termplete":
    raise SystemExit("Could not find the termplete version in all three package files")

current = manifest_match.group(1)
if current != package.get("version") or current != lock_match.group(1):
    raise SystemExit("Cargo.toml, Cargo.lock, and npm/package.json versions are out of sync")
if not allow_reset and tuple(map(int, match.groups())) <= tuple(map(int, current.split("."))):
    raise SystemExit(f"New version {version} must be greater than current version {current}")

updated_manifest = manifest[:manifest_match.start(1)] + version + manifest[manifest_match.end(1):]
updated_lock = lock[:lock_match.start(1)] + version + lock[lock_match.end(1):]
package["version"] = version

manifest_path.write_text(updated_manifest)
lock_path.write_text(updated_lock)
package_path.write_text(json.dumps(package, indent=2) + "\n")
print(f"Updated Cargo.toml, Cargo.lock, and npm/package.json to {version}.")
PY

printf 'Review the diff, then commit it before running scripts/release.sh %s.\n' "$1"
