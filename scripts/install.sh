#!/bin/sh
set -eu

REPOSITORY='https://github.com/aft1n/termplete'
VERSION=${TERMPLETE_VERSION:-latest}
INSTALL_DIR=${TERMPLETE_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}

usage() {
    cat <<'EOF'
Install TermPlete from the official GitHub release.

Usage: install.sh [--version VERSION] [--install-dir DIRECTORY]

Environment:
  TERMPLETE_VERSION       Release version (default: latest)
  TERMPLETE_INSTALL_DIR   Destination directory (default: ~/.local/bin)
EOF
}

fail() {
    printf 'termplete installer: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail '--version requires a value'
            VERSION=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || fail '--install-dir requires a value'
            INSTALL_DIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || fail 'curl is required'
valid_version() {
    printf '%s\n' "$1" | awk '/^[0-9]+\.[0-9]+\.[0-9]+([+-][A-Za-z0-9.-]+)?$/ { ok = 1 } END { exit !ok }'
}

if [ "$VERSION" = latest ]; then
    LATEST_URL=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout 10 --max-time 60 -fSsL -o /dev/null -w '%{url_effective}' "$REPOSITORY/releases/latest") ||
        fail 'could not resolve the latest GitHub release'
    case "$LATEST_URL" in
        "$REPOSITORY/releases/tag/"*) TAG=${LATEST_URL##*/} ;;
        *) fail 'GitHub did not redirect to a published release tag' ;;
    esac
    case "$TAG" in
        v*) VERSION=${TAG#v} ;;
        *) fail 'GitHub returned an invalid release tag' ;;
    esac
    valid_version "$VERSION" || fail "invalid latest release tag '$TAG'"
else
    valid_version "${VERSION#v}" || fail "invalid version '$VERSION' (expected vMAJOR.MINOR.PATCH)"
    case "$VERSION" in
        v*) TAG=$VERSION ;;
        *) TAG=v$VERSION ;;
    esac
    VERSION=${TAG#v}
fi
RELEASE_URL="$REPOSITORY/releases/download/$TAG"

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS:$ARCH" in
    Darwin:arm64|Darwin:aarch64)
        TARGET=aarch64-apple-darwin
        ;;
    Darwin:x86_64|Darwin:amd64)
        TARGET=x86_64-apple-darwin
        ;;
    Linux:aarch64|Linux:arm64)
        TARGET=aarch64-unknown-linux-gnu
        ;;
    Linux:x86_64|Linux:amd64)
        TARGET=x86_64-unknown-linux-gnu
        ;;
    *)
        fail "unsupported platform $OS/$ARCH; see https://github.com/aft1n/termplete/releases"
        ;;
esac

if [ "$OS" = Linux ] && command -v ldd >/dev/null 2>&1; then
    if ldd --version 2>&1 | head -n 1 | grep -qi musl; then
        fail 'Linux musl is not supported; install a GNU libc build or compile from source'
    fi
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256_file() { sha256sum "$1" | awk '{ print $1 }'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_file() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
    fail 'sha256sum or shasum is required'
fi

ARCHIVE="termplete-$VERSION-$TARGET.tar.gz"
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/termplete-install.XXXXXX") || fail 'could not create a temporary directory'
STAGE=
INSTALL_STARTED=0
INSTALLED=0
HAD_TERM_PLETE=0
HAD_TP=0
LOCK_HELD=0
LOCK_DIR=

cleanup() {
    STATUS=$?
    trap - EXIT HUP INT TERM
    if [ "$INSTALL_STARTED" -eq 1 ] && [ "$INSTALLED" -eq 0 ]; then
        if [ "$HAD_TERM_PLETE" -eq 1 ]; then
            if cp "$WORK_DIR/old-termplete" "$STAGE/restore-termplete" && mv "$STAGE/restore-termplete" "$INSTALL_DIR/termplete"; then
                :
            else
                printf 'termplete installer: failed to restore the previous termplete binary\n' >&2
            fi
        else
            rm -f "$INSTALL_DIR/termplete" || printf 'termplete installer: failed to remove partial termplete binary\n' >&2
        fi
        if [ "$HAD_TP" -eq 1 ]; then
            if cp "$WORK_DIR/old-tp" "$STAGE/restore-tp" && mv "$STAGE/restore-tp" "$INSTALL_DIR/tp"; then
                :
            else
                printf 'termplete installer: failed to restore the previous tp binary\n' >&2
            fi
        else
            rm -f "$INSTALL_DIR/tp" || printf 'termplete installer: failed to remove partial tp binary\n' >&2
        fi
    fi
    [ -z "$STAGE" ] || rm -rf "$STAGE"
    if [ "$LOCK_HELD" -eq 1 ]; then rmdir "$LOCK_DIR" || :; fi
    rm -rf "$WORK_DIR"
    exit "$STATUS"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

download() {
    URL=$1
    DESTINATION=$2
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout 10 --max-time 180 --max-filesize 134217728 -fSsL -o "$DESTINATION" -- "$URL" ||
        fail "download failed: $URL"
}

download "$RELEASE_URL/$ARCHIVE" "$WORK_DIR/$ARCHIVE"
download "$RELEASE_URL/SHA256SUMS" "$WORK_DIR/SHA256SUMS"

EXPECTED=$(awk -v name="$ARCHIVE" '$2 == name { print $1; count++ } END { if (count != 1) exit 1 }' "$WORK_DIR/SHA256SUMS") ||
    fail "SHA256SUMS has no unique entry for $ARCHIVE"
case "$EXPECTED" in
    *[!0-9a-f]*) fail "invalid checksum for $ARCHIVE" ;;
esac
[ "${#EXPECTED}" -eq 64 ] || fail "invalid checksum for $ARCHIVE"
ACTUAL=$(sha256_file "$WORK_DIR/$ARCHIVE")
[ "$ACTUAL" = "$EXPECTED" ] || fail "checksum verification failed for $ARCHIVE"

MEMBERS=$(tar -tzf "$WORK_DIR/$ARCHIVE") || fail "could not read $ARCHIVE"
EXPECTED_MEMBERS=$(printf 'termplete\ntp\n' | LC_ALL=C sort)
ACTUAL_MEMBERS=$(printf '%s\n' "$MEMBERS" | LC_ALL=C sort)
[ "$ACTUAL_MEMBERS" = "$EXPECTED_MEMBERS" ] ||
    fail 'archive must contain exactly the root files termplete and tp'
TAR_DETAILS=$(LC_ALL=C tar -tvzf "$WORK_DIR/$ARCHIVE") || fail "could not inspect $ARCHIVE"
ENTRY_COUNT=$(printf '%s\n' "$TAR_DETAILS" | wc -l | tr -d ' ')
[ "$ENTRY_COUNT" -eq 2 ] || fail 'archive must contain exactly two entries'
printf '%s\n' "$TAR_DETAILS" | awk 'substr($0, 1, 1) != "-" { bad = 1 } END { exit bad }' ||
    fail 'archive entries must be regular files'

mkdir -p "$INSTALL_DIR" || fail "could not create install directory: $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || fail "install directory is not writable: $INSTALL_DIR"
LOCK_DIR="$INSTALL_DIR/.termplete-install.lock"
mkdir "$LOCK_DIR" || fail "another installation or a leftover lock exists: $LOCK_DIR; retry after checking no installer is running"
LOCK_HELD=1
for binary in termplete tp; do
    if [ -L "$INSTALL_DIR/$binary" ]; then
        fail "refusing to replace symlink: $INSTALL_DIR/$binary"
    fi
    if [ -e "$INSTALL_DIR/$binary" ]; then
        [ -f "$INSTALL_DIR/$binary" ] || fail "refusing to replace non-file: $INSTALL_DIR/$binary"
        cp -p "$INSTALL_DIR/$binary" "$WORK_DIR/old-$binary" || fail "could not back up $binary"
        if [ "$binary" = termplete ]; then HAD_TERM_PLETE=1; else HAD_TP=1; fi
    fi
done

STAGE_PATH="$INSTALL_DIR/.termplete-install.$$"
mkdir "$STAGE_PATH" || fail "could not stage files in $INSTALL_DIR"
STAGE=$STAGE_PATH
tar -xzf "$WORK_DIR/$ARCHIVE" -C "$STAGE" || fail "could not extract $ARCHIVE"
for binary in termplete tp; do
    [ -f "$STAGE/$binary" ] && [ ! -L "$STAGE/$binary" ] || fail "archive did not provide a regular $binary binary"
    chmod 755 "$STAGE/$binary" || fail "could not set executable permissions on $binary"
done

INSTALL_STARTED=1
mv "$STAGE/termplete" "$INSTALL_DIR/termplete" || fail 'could not install termplete'
mv "$STAGE/tp" "$INSTALL_DIR/tp" || fail 'could not install tp'
INSTALLED=1
printf 'Installed TermPlete %s (%s) in %s\n' "$VERSION" "$TARGET" "$INSTALL_DIR"
printf 'Add %s to PATH if it is not already present.\n' "$INSTALL_DIR"
printf 'Run tp setup to configure your provider and shell, then open a new terminal.\n'
