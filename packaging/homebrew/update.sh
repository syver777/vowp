#!/bin/sh
# update.sh — fill packaging/homebrew/vowp.rb from a release SHA256SUMS file.
#
# Usage: update.sh <version> [SHA256SUMS] [formula]
#
# With no SHA256SUMS path, the file is downloaded from
# https://github.com/syver777/vowp/releases/download/v<version>/SHA256SUMS
# (override the base with VOWP_RELEASE_BASE_URL). The formula path defaults to
# vowp.rb next to this script. Nothing is published; commit the tap separately.

set -eu

PROGRAM=update.sh
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DEFAULT_RELEASE_BASE_URL=https://github.com/syver777/vowp/releases/download
FORMULA=$SCRIPT_DIR/vowp.rb
CHECKSUMS=
TMP_DIR=

usage() {
    cat <<'EOF'
Usage: update.sh <version> [SHA256SUMS] [formula]

Fill the Homebrew formula with the version and the four archive SHA256 hashes
from a release SHA256SUMS file. When SHA256SUMS is omitted it is downloaded
from the GitHub release for <version>. The formula defaults to vowp.rb next to
this script.

Environment:
  VOWP_RELEASE_BASE_URL   release download base URL
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

[ "$#" -ge 1 ] || {
    usage >&2
    exit 2
}
VERSION=$1
if [ "$#" -ge 2 ]; then
    CHECKSUMS=$2
fi
if [ "$#" -ge 3 ]; then
    FORMULA=$3
fi
[ "$#" -le 3 ] || die "too many arguments (try --help)"

case "$VERSION" in
    ''|*[!0-9A-Za-z.+-]*)
        die "invalid version: '$VERSION'"
        ;;
esac
if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
    die "version is not semver-like: '$VERSION'"
fi

if [ -z "$CHECKSUMS" ]; then
    base=${VOWP_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE_URL}
    base=${base%/}
    case "$base" in
        https://*) curl_proto='=https' ;;
        http://localhost|http://localhost:*|http://localhost/*) curl_proto='=http' ;;
        http://*) die "refusing an insecure release URL: $base" ;;
        *) die "invalid release URL: $base" ;;
    esac
    TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/vowp-homebrew.XXXXXX") || die "mktemp failed"
    trap 'rm -rf "$TMP_DIR"' 0 1 2 15
    url="$base/v$VERSION/SHA256SUMS"
    note "downloading $url"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 --connect-timeout 10 --proto "$curl_proto" --proto-redir "$curl_proto" \
            -o "$TMP_DIR/SHA256SUMS" "$url" \
            || die "download failed: $url"
    elif command -v wget >/dev/null 2>&1; then
        if [ "$curl_proto" = "=https" ]; then
            wget --https-only -q -O "$TMP_DIR/SHA256SUMS" "$url" \
                || die "download failed: $url"
        else
            wget -q -O "$TMP_DIR/SHA256SUMS" "$url" \
                || die "download failed: $url"
        fi
    else
        die "curl or wget is required to download SHA256SUMS"
    fi
    CHECKSUMS=$TMP_DIR/SHA256SUMS
fi

[ -f "$CHECKSUMS" ] || die "SHA256SUMS not found: $CHECKSUMS"
[ -f "$FORMULA" ] || die "formula not found: $FORMULA"

hash_for() {
    archive=$1
    awk -v archive="$archive" '
        $2 == archive { print $1; found = 1 }
        END { exit(found ? 0 : 1) }
    ' "$CHECKSUMS"
}

checked_hash() {
    target=$1
    archive="vowp-${VERSION}-${target}.tar.gz"
    hash=$(hash_for "$archive") || die "no SHA256 for $archive in $CHECKSUMS"
    case "$hash" in
        ''|*[!0-9a-fA-F]*) die "invalid SHA256 for $archive" ;;
    esac
    if [ "${#hash}" -ne 64 ]; then
        die "invalid SHA256 length for $archive"
    fi
    printf '%s\n' "$hash" | tr 'A-F' 'a-f'
}

hash_aarch64_apple=$(checked_hash aarch64-apple-darwin)
hash_x86_64_apple=$(checked_hash x86_64-apple-darwin)
hash_aarch64_linux=$(checked_hash aarch64-unknown-linux-gnu)
hash_x86_64_linux=$(checked_hash x86_64-unknown-linux-gnu)

out=$(mktemp "${TMPDIR:-/tmp}/vowp-formula.XXXXXX") || die "mktemp failed"
trap 'rm -f "$out"; if [ -n "$TMP_DIR" ]; then rm -rf "$TMP_DIR"; fi' 0 1 2 15

# Replace the version and each platform sha256 by reading the surrounding
# formula, not by requiring placeholders. This works on the initial template
# and on a formula that a previous run already filled.
if ! awk \
    -v version="$VERSION" \
    -v h_aa="$hash_aarch64_apple" \
    -v h_xa="$hash_x86_64_apple" \
    -v h_al="$hash_aarch64_linux" \
    -v h_xl="$hash_x86_64_linux" '
    function indent(line) {
        match(line, /^[[:space:]]*/)
        return substr(line, 1, RLENGTH)
    }
    /^[[:space:]]*version "[^"]*"[[:space:]]*$/ {
        print indent($0) "version \"" version "\""
        versions++
        next
    }
    /^[[:space:]]*url ".*aarch64-apple-darwin\.tar\.gz"/ {
        target = "aa"; print; next
    }
    /^[[:space:]]*url ".*x86_64-apple-darwin\.tar\.gz"/ {
        target = "xa"; print; next
    }
    /^[[:space:]]*url ".*aarch64-unknown-linux-gnu\.tar\.gz"/ {
        target = "al"; print; next
    }
    /^[[:space:]]*url ".*x86_64-unknown-linux-gnu\.tar\.gz"/ {
        target = "xl"; print; next
    }
    /^[[:space:]]*sha256 "[^"]*"[[:space:]]*$/ {
        if (target == "aa") hash = h_aa
        else if (target == "xa") hash = h_xa
        else if (target == "al") hash = h_al
        else if (target == "xl") hash = h_xl
        else { print; next }
        print indent($0) "sha256 \"" hash "\""
        target = ""
        hashes++
        next
    }
    { print }
    END {
        if (versions != 1) exit 3
        if (hashes != 4) exit 4
    }
' "$FORMULA" > "$out"; then
    die "formula must have one version line and four platform sha256 lines: $FORMULA"
fi

if grep -Eq '__VOWP_|__SHA256_' "$out"; then
    die "formula still contains placeholders after the update"
fi
if ! grep -q "^[[:space:]]*version \"$VERSION\"[[:space:]]*$" "$out"; then
    die "formula does not carry version $VERSION after the update"
fi

chmod 0644 "$out"
mv "$out" "$FORMULA"
note "updated $FORMULA for v$VERSION"
