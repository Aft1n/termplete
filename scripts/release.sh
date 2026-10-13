#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  printf 'Usage: %s MAJOR.MINOR.PATCH [git-remote]\n' "$0" >&2
  exit 2
fi

VERSION=$1
REMOTE=${2:-origin}
TAG=v$VERSION

python3 - "$VERSION" <<'PY'
import json
import re
import sys
from pathlib import Path

version = sys.argv[1]
if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
    raise SystemExit("Version must use MAJOR.MINOR.PATCH, for example 0.1.0")

root = Path.cwd()
manifest = re.search(r'(?m)^version = "([^"]+)"$', (root / "Cargo.toml").read_text())
lock = re.search(
    r'(?m)^\[\[package\]\]\nname = "termplete"\nversion = "([^"]+)"$',
    (root / "Cargo.lock").read_text(),
)
package = json.loads((root / "npm" / "package.json").read_text())
versions = [manifest.group(1) if manifest else None, lock.group(1) if lock else None, package.get("version")]
if versions != [version, version, version]:
    raise SystemExit(f"Release tag {version} must match Cargo.toml, Cargo.lock, and npm/package.json: {versions}")
PY

if [ "$(git branch --show-current)" != "main" ]; then
  printf 'Run the release from the main branch.\n' >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  printf 'Commit or remove working-tree changes before releasing.\n' >&2
  exit 1
fi
if git show-ref --verify --quiet "refs/tags/$TAG"; then
  printf 'Local tag %s already exists.\n' "$TAG" >&2
  exit 1
fi

REMOTE_MAIN_REF=$(git ls-remote "$REMOTE" refs/heads/main)
if [ -n "$REMOTE_MAIN_REF" ]; then
  git fetch --quiet "$REMOTE" refs/heads/main
  REMOTE_MAIN=$(git rev-parse --verify FETCH_HEAD)
  if ! git merge-base --is-ancestor "$REMOTE_MAIN" HEAD; then
    printf 'Local main must include the current %s/main before release.\n' "$REMOTE" >&2
    exit 1
  fi
fi
if [ -n "$(git ls-remote --tags "$REMOTE" "refs/tags/$TAG")" ]; then
  printf 'Remote tag %s already exists on %s.\n' "$TAG" "$REMOTE" >&2
  exit 1
fi

TAG_CREATED=0
PUSHED=0
cleanup() {
  if [ "$TAG_CREATED" -eq 1 ] && [ "$PUSHED" -eq 0 ]; then
    git tag -d "$TAG" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

git tag -a "$TAG" -m "TermPlete $TAG"
TAG_CREATED=1
git push --atomic "$REMOTE" HEAD:refs/heads/main "refs/tags/$TAG"
PUSHED=1
trap - EXIT

printf 'Pushed main and %s to %s. GitHub Actions will publish the release and sync the Homebrew formula.\n' "$TAG" "$REMOTE"
