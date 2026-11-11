#!/bin/sh
# install.sh — install the VOWP binary (and, from a checkout, the L8 watchdog).
#
# The default install downloads a prebuilt release archive for this machine and
# verifies its SHA256 before extracting anything. In a source checkout that
# already has a built binary, or with --bin PATH, that binary is installed
# instead; --download forces the release path.
#
# This is a per-user install; it never needs root and never uses sudo. From a
# checkout it also writes (when the templates are present):
#   $HOME/.local/bin/vowp-watchdog.sh       (checked by `vowp doctor`)
#   $HOME/.config/systemd/user/vowp.service (checked by `vowp doctor`)
# and adds one five-minute cron entry for the watchdog. A standalone download
# has no templates next to the script, so it installs the binary only.
#
# Spec: docs/design/loops.md §10 (L8 — watchdog); node paths:
# docs/design/architecture.md §3.

set -eu

PROGRAM=install.sh
# `$0` is the shell name when this script is piped from curl (`sh -s -- ...`),
# so it has no directory. Do not `cd .` in that case: the caller's cwd may be a
# login home the target user cannot search. A standalone install has no
# checkout templates next to it.
case $0 in
    */*) SCRIPT_DIR=$(CDPATH='' cd -- "${0%/*}" && pwd) ;;
    *) SCRIPT_DIR= ;;
esac
if [ -n "$SCRIPT_DIR" ]; then
    REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
else
    REPO_ROOT=
fi

GITHUB_REPO=syver777/vowp
DEFAULT_RELEASE_BASE_URL=https://github.com/syver777/vowp/releases/download

BIN_SOURCE=
BIN_DIR=
DRY_RUN=0
DOWNLOAD=0
WITH_SERVICE=1
WITH_CRON=1
WITH_WATCHDOG=1
WITH_LINGER=0
VERSION=${VOWP_VERSION:-}
RELEASE_BASE_URL=${VOWP_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE_URL}
DOWNLOAD_TMP=

CRON_MARKER='# vowp: 5-minute L8 watchdog check (managed by scripts/install.sh)'

usage() {
    cat <<'EOF'
Usage: install.sh [options]

Install the vowp binary for the current user. No root is required. By default
a prebuilt archive for this machine is downloaded from the latest GitHub
release and its SHA256 is verified before anything is extracted. In a source
checkout with a built binary that binary is installed instead; pass --download
to force the release.

Options:
  --version VERSION  release version to install, without the leading v
                     (default: $VOWP_VERSION, or the latest release)
  --download         install a prebuilt release archive even if a local
                     binary was found
  --bin PATH         install this local binary instead of downloading
  --prefix DIR       install the binary into DIR (default: ~/.local/bin)
  --base-url URL     release download base URL
                     (default: https://github.com/syver777/vowp/releases/download)
  --dry-run          print every action without changing files, services or cron
  --no-service       do not enable or start the systemd user unit
  --no-cron          do not change the crontab
  --no-watchdog      do not install the L8 watchdog script (implies --no-cron)
  --linger           run `loginctl enable-linger` for the current user
  -h, --help         show this help

Environment:
  VOWP_VERSION            same as --version
  VOWP_RELEASE_BASE_URL   same as --base-url
EOF
}

note() {
    printf '%s: %s\n' "$PROGRAM" "$*"
}

warn() {
    printf '%s: warning: %s\n' "$PROGRAM" "$*" >&2
}

die() {
    printf '%s: error: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version needs a version"
            VERSION=$2
            shift 2
            ;;
        --download)
            DOWNLOAD=1
            shift
            ;;
        --bin)
            [ "$#" -ge 2 ] || die "--bin needs a path"
            BIN_SOURCE=$2
            shift 2
            ;;
        --prefix)
            [ "$#" -ge 2 ] || die "--prefix needs a directory"
            BIN_DIR=$2
            shift 2
            ;;
        --base-url)
            [ "$#" -ge 2 ] || die "--base-url needs a URL"
            RELEASE_BASE_URL=$2
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --no-service)
            WITH_SERVICE=0
            shift
            ;;
        --no-cron)
            WITH_CRON=0
            shift
            ;;
        --no-watchdog)
            WITH_WATCHDOG=0
            shift
            ;;
        --linger)
            WITH_LINGER=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

[ "$#" -eq 0 ] || die "unexpected argument: $1"
[ -n "${HOME:-}" ] || die "HOME is not set"
[ -z "$BIN_SOURCE" ] || [ "$DOWNLOAD" -eq 0 ] || die "--bin and --download are mutually exclusive"
if [ "$WITH_WATCHDOG" -eq 0 ]; then
    WITH_CRON=0
fi
if [ -n "$BIN_SOURCE" ] && [ -n "$VERSION" ]; then
    warn "ignoring --version $VERSION because --bin was given; use --download to fetch a release"
fi

if [ -z "$BIN_DIR" ]; then
    BIN_DIR=$HOME/.local/bin
fi
UNIT_DIR=$HOME/.config/systemd/user
UNIT_PATH=$UNIT_DIR/vowp.service
WATCHDOG_PATH=$BIN_DIR/vowp-watchdog.sh
VOWP_PATH=$BIN_DIR/vowp

find_binary() {
    if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/release/vowp" ]; then
        printf '%s\n' "$CARGO_TARGET_DIR/release/vowp"
        return 0
    fi
    if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/debug/vowp" ]; then
        printf '%s\n' "$CARGO_TARGET_DIR/debug/vowp"
        return 0
    fi
    if [ -n "$REPO_ROOT" ] && [ -x "$REPO_ROOT/target/release/vowp" ]; then
        printf '%s\n' "$REPO_ROOT/target/release/vowp"
        return 0
    fi
    if [ -n "$REPO_ROOT" ] && [ -x "$REPO_ROOT/target/debug/vowp" ]; then
        printf '%s\n' "$REPO_ROOT/target/debug/vowp"
        return 0
    fi
    if command -v vowp >/dev/null 2>&1; then
        command -v vowp
        return 0
    fi
    return 1
}

# An explicit version or base URL means the caller wants a release download.
if [ -z "$BIN_SOURCE" ] && [ "$DOWNLOAD" -eq 0 ] && { [ -n "$VERSION" ] || [ "$RELEASE_BASE_URL" != "$DEFAULT_RELEASE_BASE_URL" ]; }; then
    DOWNLOAD=1
fi

SOURCE_BIN=
if [ -n "$BIN_SOURCE" ]; then
    [ -f "$BIN_SOURCE" ] || die "binary not found: $BIN_SOURCE"
    [ -x "$BIN_SOURCE" ] || die "binary is not executable: $BIN_SOURCE"
    SOURCE_BIN=$BIN_SOURCE
elif [ "$DOWNLOAD" -eq 0 ]; then
    if SOURCE_BIN=$(find_binary); then
        :
    else
        SOURCE_BIN=
        DOWNLOAD=1
    fi
fi

detect_target() {
    os=$(uname -s 2>/dev/null || true)
    arch=$(uname -m 2>/dev/null || true)
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64)
            printf '%s\n' x86_64-unknown-linux-gnu
            ;;
        Linux:aarch64|Linux:arm64)
            printf '%s\n' aarch64-unknown-linux-gnu
            ;;
        Darwin:x86_64|Darwin:amd64)
            printf '%s\n' x86_64-apple-darwin
            ;;
        Darwin:arm64|Darwin:aarch64)
            printf '%s\n' aarch64-apple-darwin
            ;;
        *)
            return 1
            ;;
    esac
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum or shasum is required to verify the download"
    fi
}

fetch() {
    src=$1
    dst=$2
    case "$src" in
        https://*) curl_proto='=https' ;;
        http://localhost|http://localhost:*|http://localhost/*) curl_proto='=http' ;;
        http://*) die "refusing an insecure release URL: $src" ;;
        *) die "invalid release URL: $src" ;;
    esac
    if command -v curl >/dev/null 2>&1; then
        if ! curl -fsSL --retry 2 --connect-timeout 10 --proto "$curl_proto" --proto-redir "$curl_proto" -o "$dst" "$src"; then
            die "download failed: $src"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ "$curl_proto" = "=https" ]; then
            if ! wget --https-only -q -O "$dst" "$src"; then
                die "download failed: $src"
            fi
        elif ! wget -q -O "$dst" "$src"; then
            die "download failed: $src"
        fi
    else
        die "curl or wget is required to download a release"
    fi
}

resolve_latest_version() {
    latest_url="https://github.com/${GITHUB_REPO}/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        effective=$(curl -fsSIL --connect-timeout 10 --proto '=https' --proto-redir '=https' -o /dev/null -w '%{url_effective}' "$latest_url") \
            || die "could not resolve the latest release from $latest_url"
    elif command -v wget >/dev/null 2>&1; then
        effective=$(wget --https-only --max-redirect=0 --spider -S "$latest_url" 2>&1 \
            | awk '/^[[:space:]]*Location:/{print $2}' | tr -d '\r' | tail -n 1)
        [ -n "$effective" ] || die "could not resolve the latest release from $latest_url"
    else
        die "curl or wget is required to resolve the latest release"
    fi
    case "$effective" in
        */releases/tag/*) ;;
        *) die "latest release did not redirect to a tag: $effective" ;;
    esac
    printf '%s\n' "${effective##*/}" | sed 's/^v//'
}

setup_download() {
    TARGET=$(detect_target) || die "unsupported platform: $(uname -s 2>/dev/null || echo unknown) $(uname -m 2>/dev/null || echo unknown); supported: x86_64/aarch64 Linux, x86_64/arm64 macOS"
    RELEASE_BASE_URL=${RELEASE_BASE_URL%/}
    if [ -z "$VERSION" ]; then
        VERSION=$(resolve_latest_version)
    fi
    case "$VERSION" in
        ''|*[!0-9A-Za-z.+-]*)
            die "invalid version: '$VERSION'"
            ;;
    esac
    if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
        die "version is not semver-like: '$VERSION'"
    fi
    ARCHIVE="vowp-${VERSION}-${TARGET}.tar.gz"
    ARCHIVE_URL="${RELEASE_BASE_URL}/v${VERSION}/${ARCHIVE}"
    SHA_URL="${ARCHIVE_URL}.sha256"
}

download_archive() {
    setup_download
    DOWNLOAD_TMP=$(mktemp -d "${TMPDIR:-/tmp}/vowp-install.XXXXXX") || die "mktemp failed"
    trap 'if [ -n "$DOWNLOAD_TMP" ]; then rm -rf "$DOWNLOAD_TMP"; fi' 0 1 2 15

    note "downloading $ARCHIVE_URL"
    fetch "$ARCHIVE_URL" "$DOWNLOAD_TMP/$ARCHIVE"
    fetch "$SHA_URL" "$DOWNLOAD_TMP/$ARCHIVE.sha256"

    expected=$(awk 'NR == 1 {print $1}' "$DOWNLOAD_TMP/$ARCHIVE.sha256")
    case "$expected" in
        ''|*[!0-9a-fA-F]*)
            die "invalid SHA256 file for $ARCHIVE"
            ;;
    esac
    if [ "${#expected}" -ne 64 ]; then
        die "invalid SHA256 length in the checksum file for $ARCHIVE"
    fi
    expected=$(printf '%s\n' "$expected" | tr 'A-F' 'a-f')
    actual=$(sha256_file "$DOWNLOAD_TMP/$ARCHIVE" | tr 'A-F' 'a-f')
    if [ "$actual" != "$expected" ]; then
        die "SHA256 mismatch for $ARCHIVE (expected $expected, got $actual)"
    fi
    note "verified SHA256 $actual"

    if tar -tzf "$DOWNLOAD_TMP/$ARCHIVE" | grep -Eq '(^/|(^|/)\.\.(/|$))'; then
        die "archive contains an unsafe path: $ARCHIVE"
    fi
    mkdir -p "$DOWNLOAD_TMP/extract"
    tar -xzf "$DOWNLOAD_TMP/$ARCHIVE" -C "$DOWNLOAD_TMP/extract"
    SOURCE_BIN="$DOWNLOAD_TMP/extract/vowp-${VERSION}-${TARGET}/vowp"
    [ -f "$SOURCE_BIN" ] || die "archive does not contain $ARCHIVE/vowp"
    chmod 0755 "$SOURCE_BIN"
    # Run the probe from the temp dir: the caller's cwd may be a login home the
    # target user cannot search, and shells warn when getcwd() fails.
    DOWNLOADED_VERSION=$(cd "$DOWNLOAD_TMP" && "$SOURCE_BIN" --version 2>&1) \
        || die "downloaded binary failed to run: $SOURCE_BIN --version"
    case "$DOWNLOADED_VERSION" in
        "vowp ${VERSION}"*) ;;
        *) die "downloaded binary reports '$DOWNLOADED_VERSION', expected 'vowp ${VERSION}'" ;;
    esac
}

if [ "$DOWNLOAD" -eq 1 ]; then
    setup_download
fi

if [ "$DOWNLOAD" -eq 1 ] && [ "$DRY_RUN" -eq 0 ]; then
    download_archive
fi

if [ -z "$SOURCE_BIN" ] && [ "$DRY_RUN" -eq 0 ]; then
    die "no vowp binary found; build it with 'cargo build --release -p vowp-cli --bin vowp' or pass --bin PATH"
fi
if [ -z "$SOURCE_BIN" ] && [ "$DRY_RUN" -eq 1 ] && [ "$DOWNLOAD" -eq 0 ]; then
    warn "no vowp binary found; a real install would download a release or stop here"
fi

HAVE_TEMPLATES=0
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/vowp-watchdog.sh" ] && [ -f "$SCRIPT_DIR/vowp.service" ]; then
    HAVE_TEMPLATES=1
fi

if [ "$HAVE_TEMPLATES" -eq 0 ] || [ "$BIN_DIR" != "$HOME/.local/bin" ]; then
    if [ "$WITH_SERVICE" -eq 1 ] || [ "$WITH_CRON" -eq 1 ]; then
        if [ "$HAVE_TEMPLATES" -eq 0 ]; then
            note "watchdog and service templates are not next to this installer; installing the binary only"
        else
            note "service and cron templates require the default ~/.local/bin prefix; installing the binary only"
        fi
    fi
    WITH_SERVICE=0
    WITH_CRON=0
fi

USE_TEMPLATES=0
if [ "$HAVE_TEMPLATES" -eq 1 ] && [ "$BIN_DIR" = "$HOME/.local/bin" ]; then
    USE_TEMPLATES=1
fi

cron_line() {
    printf '*/5 * * * * "%s" >/dev/null 2>&1\n' "$WATCHDOG_PATH"
}

if [ "$DRY_RUN" -eq 1 ]; then
    note "dry-run: no files, services or crontabs will be changed"
    if [ "$DOWNLOAD" -eq 1 ]; then
        note "would download $ARCHIVE_URL"
        note "would verify it against $SHA_URL"
        note "would install binary for $TARGET -> $VOWP_PATH"
    elif [ -n "$SOURCE_BIN" ]; then
        note "would install binary $SOURCE_BIN -> $VOWP_PATH"
    else
        note "would install binary <not found> -> $VOWP_PATH"
    fi
    if [ "$USE_TEMPLATES" -eq 1 ]; then
        if [ "$WITH_WATCHDOG" -eq 1 ]; then
            note "would install watchdog $SCRIPT_DIR/vowp-watchdog.sh -> $WATCHDOG_PATH"
        else
            note "would skip the watchdog script (--no-watchdog)"
        fi
        note "would install unit $SCRIPT_DIR/vowp.service -> $UNIT_PATH"
    fi
    if [ "$WITH_SERVICE" -eq 1 ]; then
        note "would run: systemctl --user daemon-reload"
        note "would run: systemctl --user enable --now vowp.service"
    else
        note "would skip enabling the systemd user unit (--no-service)"
    fi
    if [ "$WITH_LINGER" -eq 1 ]; then
        note "would run: loginctl enable-linger $(id -un)"
    fi
    if [ "$WITH_CRON" -eq 1 ]; then
        note "would install crontab entry: $(cron_line)"
    else
        note "would skip the crontab update (--no-cron)"
    fi
    note "next: vowp doctor"
    exit 0
fi

install_file() {
    src=$1
    dst=$2
    mode=$3
    if command -v install >/dev/null 2>&1; then
        install -m "$mode" "$src" "$dst"
    else
        cp "$src" "$dst"
        chmod "$mode" "$dst"
    fi
}

mkdir -p "$BIN_DIR"
if [ "$USE_TEMPLATES" -eq 1 ]; then
    mkdir -p "$UNIT_DIR"
fi

if [ "$SOURCE_BIN" != "$VOWP_PATH" ]; then
    install_file "$SOURCE_BIN" "$VOWP_PATH" 0755
    note "installed $VOWP_PATH"
else
    note "binary already at $VOWP_PATH"
fi

if [ "$DOWNLOAD" -eq 1 ]; then
    note "installed: $DOWNLOADED_VERSION"
fi

if [ -n "$DOWNLOAD_TMP" ]; then
    rm -rf "$DOWNLOAD_TMP"
    DOWNLOAD_TMP=
    trap - 0 1 2 15
fi

if [ "$USE_TEMPLATES" -eq 1 ]; then
    if [ "$WITH_WATCHDOG" -eq 1 ]; then
        install_file "$SCRIPT_DIR/vowp-watchdog.sh" "$WATCHDOG_PATH" 0755
        note "installed $WATCHDOG_PATH"
    else
        note "skipped the watchdog script (--no-watchdog)"
    fi

    install_file "$SCRIPT_DIR/vowp.service" "$UNIT_PATH" 0644
    note "installed $UNIT_PATH"
fi

if [ "$WITH_SERVICE" -eq 1 ]; then
    if command -v systemctl >/dev/null 2>&1; then
        if systemctl --user daemon-reload; then
            :
        else
            warn "systemctl --user daemon-reload failed; run it after your user manager is available"
        fi
        if systemctl --user enable --now vowp.service; then
            note "enabled and started vowp.service"
        else
            warn "could not enable/start vowp.service; run 'systemctl --user enable --now vowp.service' after logging in"
        fi
    else
        warn "systemctl not found; the unit is installed but not enabled"
    fi
else
    if [ "$USE_TEMPLATES" -eq 0 ]; then
        note "systemd user unit left disabled (no default-prefix templates)"
    else
        note "systemd user unit left disabled (--no-service)"
    fi
fi

if [ "$WITH_LINGER" -eq 1 ]; then
    if command -v loginctl >/dev/null 2>&1; then
        if loginctl enable-linger "$(id -un)"; then
            note "enabled linger for $(id -un)"
        else
            warn "loginctl enable-linger failed; the supervisor will only run while you are logged in"
        fi
    else
        warn "loginctl not found; linger was not enabled"
    fi
fi

install_cron() {
    if ! command -v crontab >/dev/null 2>&1; then
        warn "crontab not found; add this entry manually:"
        printf '  %s\n  %s\n' "$CRON_MARKER" "$(cron_line)" >&2
        return 0
    fi

    tmp=$(mktemp "${TMPDIR:-/tmp}/vowp-crontab.XXXXXX") || die "mktemp failed"
    trap 'rm -f "$tmp" "$tmp.filtered" "$tmp.err"' 0 1 2 15

    if ! crontab -l > "$tmp" 2> "$tmp.err"; then
        if grep -qi 'no crontab' "$tmp.err"; then
            : > "$tmp"
        else
            detail=$(head -n 1 "$tmp.err" 2>/dev/null || true)
            [ -n "$detail" ] || detail="unknown error"
            warn "could not read the crontab ($detail); leaving it unchanged"
            warn "add this entry manually:"
            printf '  %s\n  %s\n' "$CRON_MARKER" "$(cron_line)" >&2
            rm -f "$tmp" "$tmp.filtered" "$tmp.err"
            return 0
        fi
    fi
    awk -v marker="$CRON_MARKER" '
        $0 == marker { next }
        $0 ~ /^\*\/5 \* \* \* \* ".*\/vowp-watchdog\.sh" >\/dev\/null 2>&1$/ { next }
        { print }
    ' "$tmp" > "$tmp.filtered"
    {
        printf '%s\n' "$CRON_MARKER"
        cron_line
    } >> "$tmp.filtered"

    if crontab "$tmp.filtered"; then
        note "installed crontab entry for $WATCHDOG_PATH (every 5 minutes)"
    else
        warn "could not update the crontab; add this entry manually:"
        printf '  %s\n  %s\n' "$CRON_MARKER" "$(cron_line)" >&2
    fi

    rm -f "$tmp" "$tmp.filtered" "$tmp.err"
}

if [ "$WITH_CRON" -eq 1 ]; then
    install_cron
else
    if [ "$USE_TEMPLATES" -eq 0 ]; then
        note "crontab left unchanged (no default-prefix templates)"
    else
        note "crontab left unchanged (--no-cron)"
    fi
fi

note "next: run 'vowp doctor' and check the systemd and watchdog checks"
