#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

fail() { printf 'termplete release: %s\n' "$*" >&2; exit 1; }

if [ "$#" -gt 1 ]; then
    fail "Usage: $0 [patch|minor|major|MAJOR.MINOR.PATCH]"
fi
REQUEST=${1:-patch}
for tool in git python3 gh; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done
gh auth status --hostname github.com >/dev/null 2>&1 || fail 'Run gh auth login first.'
[ "$(git branch --show-current)" = main ] || fail 'Run this command on main after merging your feature.'
[ -z "$(git status --porcelain)" ] || fail 'Commit your feature changes before publishing.'
REPOSITORY=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

# Include upstream version and formula updates before choosing the next version.
git pull --ff-only origin main
VERSION=$(python3 - "$REQUEST" <<'PY'
import re
import sys
from pathlib import Path

current = re.search(r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"$',
                    Path("Cargo.toml").read_text(), re.M)
if current is None:
    raise SystemExit("Cannot read the current package version")
parts = list(map(int, current.group(1).split(".")))
request = sys.argv[1]
if request in ("major", "minor", "patch"):
    index = ("major", "minor", "patch").index(request)
    parts[index] += 1
    parts[index + 1:] = [0] * (2 - index)
    version = ".".join(map(str, parts))
elif re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", request):
    if tuple(map(int, request.split("."))) < tuple(parts):
        raise SystemExit("Release version must not be older than the current version")
    version = request
else:
    raise SystemExit("Use patch, minor, major, or MAJOR.MINOR.PATCH")
print(version)
PY
)
TAG=v$VERSION
REMOTE_TAG=$(git ls-remote --tags origin "refs/tags/$TAG")
if git show-ref --verify --quiet "refs/tags/$TAG" ||
    [ -n "$REMOTE_TAG" ]; then
    fail "$TAG already exists. Check its Release workflow; use a new version for new code."
fi

CURRENT=$(python3 -c 'import re; from pathlib import Path; print(re.search(r"^version = \"([^\"]+)\"$", Path("Cargo.toml").read_text(), re.M).group(1))')
if [ "$VERSION" != "$CURRENT" ]; then
    "$ROOT/scripts/bump-version.sh" "$VERSION"
    git diff --check
    git add Cargo.toml Cargo.lock npm/package.json
    git commit -m "chore(release): prepare $TAG"
fi
"$ROOT/scripts/release.sh" "$VERSION"
COMMIT=$(git rev-parse HEAD)

RUN_ID=
ATTEMPT=0
while [ -z "$RUN_ID" ]; do
    RUN_ID=$(gh run list --repo "$REPOSITORY" --workflow release.yml \
        --branch "$TAG" --commit "$COMMIT" --event push --limit 1 \
        --json databaseId --jq '.[0].databaseId // empty')
    ATTEMPT=$((ATTEMPT + 1))
    [ -n "$RUN_ID" ] && break
    [ "$ATTEMPT" -lt 30 ] || fail "Tag pushed, but no Release run found. Check https://github.com/$REPOSITORY/actions."
    sleep 2
done
if ! gh run watch "$RUN_ID" --repo "$REPOSITORY" --interval 15 --exit-status; then
    fail "Release failed. Inspect https://github.com/$REPOSITORY/actions/runs/$RUN_ID and rerun failed jobs; keep $TAG."
fi

# The successful workflow has committed the generated Homebrew formula to main.
git pull --ff-only origin main
gh release view "$TAG" --repo "$REPOSITORY" --json url --jq .url
printf 'Published %s and pulled the Homebrew formula.\n' "$TAG"
