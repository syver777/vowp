#!/bin/sh
# Fake harness used by the vowp-adapters tests. See docs/design/adapters.md §9.
# It never calls a real model; every input comes from the FAKE_* environment.

if [ "$FAKE_HANG" = "ignoreterm" ] || [ "$FAKE_HANG" = "ignoreboth" ]; then
    trap '' TERM
fi
if [ "$FAKE_HANG" = "ignoreint" ] || [ "$FAKE_HANG" = "ignoreboth" ]; then
    trap '' INT
fi

if [ -n "$FAKE_ARGV_OUT" ]; then
    printf '%s\n' "$@" > "$FAKE_ARGV_OUT"
fi

if [ -n "$FAKE_STDIN_OUT" ]; then
    cat > "$FAKE_STDIN_OUT"
fi

if [ -n "$FAKE_STDERR" ]; then
    printf '%s\n' "$FAKE_STDERR" >&2
fi

if [ -n "$FAKE_FIXTURE" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
        printf '%s\n' "$line"
        if [ -n "$FAKE_SLEEP" ]; then
            sleep "$FAKE_SLEEP"
        fi
    done < "$FAKE_FIXTURE"
fi

if [ "$FAKE_SPAWN_CHILD" = "1" ]; then
    sleep 300 &
    child=$!
    if [ -n "$FAKE_CHILD_PID_OUT" ]; then
        printf '%s\n' "$child" > "$FAKE_CHILD_PID_OUT"
    fi
fi

if [ -n "$FAKE_HANG" ]; then
    while :; do
        sleep 300
    done
fi

exit "${FAKE_EXIT:-0}"
