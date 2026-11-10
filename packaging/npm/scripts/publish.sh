#!/bin/sh
# publish.sh — verify the GitHub release and publish this npm package.
#
# Dry run by default: it checks the release assets and runs
# `npm publish --dry-run`. Nothing is published unless --execute is given.
#
# With --execute the npm token is read only from NODE_AUTH_TOKEN, written to a
# temporary .npmrc with mode 0600 for the duration of the command, and removed
# by the exit trap. The token is never printed and never written to this
# directory.
#
# Usage: scripts/publish.sh [--execute]
#
# Environment:
#   NODE_AUTH_TOKEN          npm token, required only by --execute
#   VOWP_RELEASE_BASE_URL    release download base URL
#                            (default: https://github.com/syver777/vowp/releases/download)

set -eu

PROGRAM=publish.sh
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PACKAGE_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
DEFAULT_RELEASE_BASE_URL=https://github.com/syver777/vowp/releases/download
TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"

RELEASE_BASE_URL=${VOWP_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE_URL}
EXECUTE=0
TMP_DIR=

usage() {
    cat <<'EOF'
Usage: publish.sh [--execute]

Verify that the GitHub release for the package version has all four archives,
their .sha256 files and SHA256SUMS, and that every archive matches its checksum.
Then run `npm publish --dry-run`.

Options:
  --execute   publish with `npm publish --access public`; requires
              NODE_AUTH_TOKEN. Without this flag nothing is published.
  -h, --help  show this help

The GitHub release must already be public; the assets of a draft release are
not visible without authentication.

Environment:
  NODE_AUTH_TOKEN          npm token (only with --execute)
  VOWP_RELEASE_BASE_URL    release download base URL
EOF
}

die() {
    printf '%s: error: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "$PROGRAM" "$*"
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
esac

while [ "$#" -gt 0 ]; do
    case "$1" in
        --execute)
            EXECUTE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

command -v npm >/dev/null 2>&1 || die "npm is required"

NAME=$(node -e 'process.stdout.write(String(require(process.argv[1]).name))' \
    "$PACKAGE_DIR/package.json" 2>/dev/null) || die "cannot read package.json"
VERSION=$(node -e 'process.stdout.write(String(require(process.argv[1]).version))' \
    "$PACKAGE_DIR/package.json" 2>/dev/null) || die "cannot read package.json"
[ "$NAME" = "vowp" ] || die "unexpected package name: $NAME"
if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
    die "package version is not semver-like: $VERSION"
fi

if [ "$EXECUTE" -eq 1 ] && [ -z "${NODE_AUTH_TOKEN:-}" ]; then
    die "NODE_AUTH_TOKEN is not set; refusing to publish"
fi

RELEASE_BASE_URL=${RELEASE_BASE_URL%/}
case "$RELEASE_BASE_URL" in
    https://*)
        proto='=https'
        ;;
    http://localhost|http://localhost:*|http://localhost/*|\
    http://127.0.0.1|http://127.0.0.1:*|http://127.0.0.1/*|\
    http://\[::1\]|http://\[::1\]:*|http://\[::1\]/*)
        proto='=http'
        ;;
    http://*)
        die "refusing an insecure release URL: $RELEASE_BASE_URL"
        ;;
    *)
        die "invalid release URL: $RELEASE_BASE_URL"
        ;;
esac

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/vowp-npm-publish.XXXXXX") || die "mktemp failed"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup 0 1 2 15

fetch() {
    src=$1
    dst=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 --connect-timeout 10 --max-time 300 \
            --proto "$proto" --proto-redir "$proto" -o "$dst" "$src" \
            || die "download failed: $src"
    elif command -v wget >/dev/null 2>&1; then
        if [ "$proto" = "=https" ]; then
            wget --https-only -q -O "$dst" "$src" \
                || die "download failed: $src"
        else
            wget -q -O "$dst" "$src" \
                || die "download failed: $src"
        fi
    else
        die "curl or wget is required to verify the release"
    fi
}

head_asset() {
    url=$1
    if command -v curl >/dev/null 2>&1; then
        curl -fsSIL --connect-timeout 10 --max-time 30 \
            --proto "$proto" --proto-redir "$proto" -o /dev/null "$url" \
            || die "release asset is not available: $url"
    elif command -v wget >/dev/null 2>&1; then
        wget --spider -q --timeout=30 "$url" \
            || die "release asset is not available: $url"
    else
        die "curl or wget is required to verify the release"
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum or shasum is required to verify the release"
    fi
}

# Print the hash for $2 in checksum file $1. A line with only a hash is
# accepted; otherwise the file name must match exactly.
hash_from_checksum_file() {
    file=$1
    archive=$2
    awk -v archive="$archive" '
        {
            name = $2
            sub(/^\*/, "", name)
            if (!found && $1 ~ /^[0-9a-fA-F]{64}$/ &&
                (name == "" || name == archive)) {
                print tolower($1)
                found = 1
            }
        }
        END { exit(found ? 0 : 1) }
    ' "$file"
}

note "checking release assets for v$VERSION in $RELEASE_BASE_URL"
head_asset "$RELEASE_BASE_URL/v$VERSION/SHA256SUMS"
fetch "$RELEASE_BASE_URL/v$VERSION/SHA256SUMS" "$TMP_DIR/SHA256SUMS"

for target in $TARGETS; do
    archive="vowp-${VERSION}-${target}.tar.gz"
    archive_url="$RELEASE_BASE_URL/v$VERSION/$archive"

    head_asset "$archive_url"
    head_asset "$archive_url.sha256"
    fetch "$archive_url.sha256" "$TMP_DIR/$archive.sha256"

    asset_hash=$(hash_from_checksum_file "$TMP_DIR/$archive.sha256" "$archive") \
        || die "no SHA256 for $archive in its .sha256 file"
    sums_hash=$(hash_from_checksum_file "$TMP_DIR/SHA256SUMS" "$archive") \
        || die "no SHA256 for $archive in SHA256SUMS"
    [ "${#asset_hash}" -eq 64 ] || die "invalid SHA256 length for $archive"
    [ "$asset_hash" = "$sums_hash" ] \
        || die "checksum files disagree for $archive"

    fetch "$archive_url" "$TMP_DIR/$archive"
    actual=$(sha256_file "$TMP_DIR/$archive" | tr 'A-F' 'a-f')
    [ "$actual" = "$asset_hash" ] \
        || die "SHA256 mismatch for $archive (expected $asset_hash, got $actual)"
    note "verified $archive"
done

NPMRC=$TMP_DIR/npmrc
if [ "$EXECUTE" -eq 1 ]; then
    (umask 077 && printf 'registry=https://registry.npmjs.org/\n//registry.npmjs.org/:_authToken=%s\n' \
        "$NODE_AUTH_TOKEN" > "$NPMRC") \
        || die "cannot write the temporary npm configuration"
    note "publishing $NAME@$VERSION to npm"
    (
        cd "$PACKAGE_DIR" &&
            npm_config_userconfig="$NPMRC" \
            npm_config_cache="$TMP_DIR/npm-cache" \
            npm publish --access public
    ) || die "npm publish failed"
    note "published $NAME@$VERSION"
else
    (umask 077 && printf 'registry=https://registry.npmjs.org/\n' > "$NPMRC") \
        || die "cannot write the temporary npm configuration"
    note "running npm publish --dry-run (nothing is published)"
    (
        cd "$PACKAGE_DIR" &&
            npm_config_userconfig="$NPMRC" \
            npm_config_cache="$TMP_DIR/npm-cache" \
            npm publish --dry-run
    ) || die "npm publish --dry-run failed"
    note "dry run only; nothing was published"
fi
