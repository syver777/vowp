#!/bin/sh
# uninstall.sh — remove the VOWP binary, systemd user unit and cron watchdog.
#
# This removes only what scripts/install.sh installed. The node state in
# `$VOWP_HOME` (default ~/.vowp) is deliberately left alone; delete it by hand
# if you really want to discard the database, session logs and backups.
#
# Spec: docs/design/loops.md §10 (L8 — watchdog); node paths:
# docs/design/architecture.md §3.

set -eu

PROGRAM=uninstall.sh

DRY_RUN=0
KEEP_BINARY=0

CRON_MARKER='# vowp: 5-minute L8 watchdog check (managed by scripts/install.sh)'

usage() {
    cat <<'EOF'
Usage: uninstall.sh [options]

Stop and remove the VOWP systemd user unit, the cron watchdog and the
installed binaries. No root is required. Node state in $VOWP_HOME is kept.

Options:
  --keep-binary   leave $HOME/.local/bin/vowp in place
  --dry-run       print every action without changing files, services or cron
  -h, --help      show this help
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
        --keep-binary)
            KEEP_BINARY=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
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

BIN_DIR=$HOME/.local/bin
UNIT_DIR=$HOME/.config/systemd/user
UNIT_PATH=$UNIT_DIR/vowp.service
WATCHDOG_PATH=$BIN_DIR/vowp-watchdog.sh
VOWP_PATH=$BIN_DIR/vowp
VOWP_HOME=${VOWP_HOME:-"$HOME/.vowp"}

if [ "$DRY_RUN" -eq 1 ]; then
    note "dry-run: no files, services or crontabs will be changed"
    note "would run: systemctl --user disable --now vowp.service"
    note "would remove $UNIT_PATH"
    note "would run: systemctl --user daemon-reload"
    note "would remove the vowp watchdog crontab entry"
    note "would remove $WATCHDOG_PATH"
    if [ "$KEEP_BINARY" -eq 1 ]; then
        note "would keep $VOWP_PATH (--keep-binary)"
    else
        note "would remove $VOWP_PATH"
    fi
    note "would keep node state in $VOWP_HOME"
    exit 0
fi

service_stopped=0
if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user disable --now vowp.service; then
        service_stopped=1
        note "disabled and stopped vowp.service"
    else
        warn "systemctl --user disable --now vowp.service failed; continuing"
    fi
else
    warn "systemctl not found; the user unit cannot be disabled"
fi

if [ "$service_stopped" -eq 0 ]; then
    down_bin=
    if [ -x "$VOWP_PATH" ]; then
        down_bin=$VOWP_PATH
    elif command -v vowp >/dev/null 2>&1; then
        down_bin=$(command -v vowp)
    fi
    if [ -n "$down_bin" ]; then
        if "$down_bin" --home "$VOWP_HOME" down; then
            note "asked the running supervisor to stop with '$down_bin --home $VOWP_HOME down'"
        else
            warn "'$down_bin down' failed; the supervisor may still be running"
        fi
    else
        warn "vowp not found; stop any running supervisor manually"
    fi
fi

rm -f "$UNIT_PATH"
note "removed $UNIT_PATH"

if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user daemon-reload; then
        :
    else
        warn "systemctl --user daemon-reload failed; run it after your user manager is available"
    fi
fi

remove_cron() {
    if ! command -v crontab >/dev/null 2>&1; then
        warn "crontab not found; remove the VOWP watchdog entry manually"
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
            rm -f "$tmp" "$tmp.filtered" "$tmp.err"
            return 0
        fi
    fi
    awk -v marker="$CRON_MARKER" '
        $0 == marker { next }
        $0 ~ /^\*\/5 \* \* \* \* ".*\/vowp-watchdog\.sh" >\/dev\/null 2>&1$/ { next }
        { print }
    ' "$tmp" > "$tmp.filtered"

    if crontab "$tmp.filtered"; then
        note "removed the vowp watchdog crontab entry"
    else
        warn "could not update the crontab; remove the VOWP watchdog entry manually"
    fi

    rm -f "$tmp" "$tmp.filtered" "$tmp.err"
}

remove_cron

rm -f "$WATCHDOG_PATH"
note "removed $WATCHDOG_PATH"

if [ "$KEEP_BINARY" -eq 1 ]; then
    note "kept $VOWP_PATH (--keep-binary)"
else
    rm -f "$VOWP_PATH"
    note "removed $VOWP_PATH"
fi

note "kept node state in $VOWP_HOME"
note "delete that directory by hand only if you want to discard the database and session logs"
