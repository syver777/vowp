#!/bin/sh
# examples/hello-vowp/tests/test_run.sh — exercise run.sh with the work directories that
# new users actually pass.
#
# Three independent scenarios, each in its own temporary test root with a private HOME and
# tmux socket directory:
#   1. default WORK (a temporary directory created by run.sh);
#   2. a relative WORK (WORK=relwork, launched from the scenario directory);
#   3. a WORK path containing a space.
#
# Every scenario runs with FAKE=1 and must finish with hello-greeting, hello-farewell and
# hello-message in state `done` (run.sh checks that, and this script also checks the `job ls`
# table and the merged files). Nothing here touches ~/.vowp, the caller's tmux server, or a
# real harness. The demo makes no network calls; the one-time `cargo build` fallback may
# fetch crates, just like any other cargo command in this repository.
#
# Usage:
#   bash examples/hello-vowp/tests/test_run.sh
#   TIMEOUT=300 bash examples/hello-vowp/tests/test_run.sh
#   KEEP=1 bash examples/hello-vowp/tests/test_run.sh   # keep the test root

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VOWP_REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
RUN="$ROOT/run.sh"
KEEP=${KEEP:-0}
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/hello-vowp-test.XXXXXX")
failures=0

# Use a real built binary for every scenario when possible. This keeps the test independent
# of run.sh's `cargo run` fallback, which is slower and (under the repository's build
# container) cannot see a temporary directory outside the mounted worktree.
if [ -z "${VOWP:-}" ]; then
    if command -v vowp >/dev/null 2>&1; then
        VOWP=$(command -v vowp)
    elif [ -x "$VOWP_REPO/target/debug/vowp" ]; then
        VOWP="$VOWP_REPO/target/debug/vowp"
    elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/debug/vowp" ]; then
        VOWP="$CARGO_TARGET_DIR/debug/vowp"
    else
        echo "hello-vowp tests: building vowp once"
        cargo build -q --manifest-path "$VOWP_REPO/Cargo.toml" -p vowp-cli --bin vowp
        if [ -n "${CARGO_TARGET_DIR:-}" ]; then
            VOWP="$CARGO_TARGET_DIR/debug/vowp"
        else
            VOWP="$VOWP_REPO/target/debug/vowp"
        fi
    fi
fi
export VOWP

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ "$KEEP" = "1" ] || [ "$status" -ne 0 ]; then
        echo "hello-vowp tests: test root kept at $TEST_ROOT" >&2
    else
        rm -rf "$TEST_ROOT"
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

run_case() {
    case_name=$1
    case_kind=$2
    case_desc=$3
    scenario="$TEST_ROOT/$case_name"
    log="$scenario/run.log"
    mkdir -p "$scenario/home" "$scenario/tmp" "$scenario/tmux"

    echo "== $case_name: $case_desc =="
    case_status=0
    if (
        cd "$scenario"
        unset WORK TMUX TMUX_PANE
        HOME="$scenario/home"
        TMPDIR="$scenario/tmp"
        TMUX_TMPDIR="$scenario/tmux"
        FAKE=1
        KEEP=1
        export HOME TMPDIR TMUX_TMPDIR FAKE KEEP
        case "$case_kind" in
            relative)
                WORK=relwork
                export WORK
                ;;
            spaced)
                WORK="$scenario/work dir"
                export WORK
                ;;
        esac
        "$RUN"
    ) >"$log" 2>&1; then
        case_status=0
    else
        case_status=$?
    fi

    if [ "$case_status" -ne 0 ]; then
        fail "$case_name: run.sh exited $case_status"
        echo "--- $case_name log tail ---" >&2
        tail -n 40 "$log" >&2 || true
        return 0
    fi

    workdir=$(sed -n 's/^hello-vowp: workdir //p' "$log" | head -n 1)
    if [ -z "$workdir" ]; then
        fail "$case_name: run.sh did not print a work directory"
        return 0
    fi
    case "$workdir" in
        /*) ;;
        *)
            fail "$case_name: WORK was not normalised to an absolute path: $workdir"
            return 0
            ;;
    esac
    if [ ! -d "$workdir" ]; then
        fail "$case_name: reported work directory does not exist: $workdir"
        return 0
    fi

    case "$case_kind" in
        default)
            tmp_physical=$(CDPATH= cd -- "$scenario/tmp" && pwd -P)
            case "$workdir" in
                "$tmp_physical"/hello-vowp.*) ;;
                *) fail "$case_name: default WORK is not under the private TMPDIR: $workdir" ;;
            esac
            ;;
        relative)
            expected=$(CDPATH= cd -- "$scenario/relwork" && pwd -P)
            if [ "$workdir" != "$expected" ]; then
                fail "$case_name: workdir $workdir is not $expected"
            fi
            ;;
        spaced)
            expected=$(CDPATH= cd -- "$scenario/work dir" && pwd -P)
            if [ "$workdir" != "$expected" ]; then
                fail "$case_name: workdir $workdir is not $expected"
            fi
            ;;
    esac

    for id in hello-greeting hello-farewell hello-message; do
        if ! grep -Eq "^$id[[:space:]]+done([[:space:]]|$)" "$log"; then
            fail "$case_name: job ls output does not show $id done"
        fi
    done
    if ! grep -qx 'jobs: done 3' "$log"; then
        fail "$case_name: status output does not say jobs: done 3"
    fi

    project="$workdir/hello-vowp"
    for file in src/greeting.txt src/farewell.txt src/message.txt; do
        if [ ! -f "$project/$file" ]; then
            fail "$case_name: merged file is missing: $project/$file"
        fi
    done
    if ! grep -qx 'Hello from VOWP' "$project/src/greeting.txt" 2>/dev/null; then
        fail "$case_name: merged src/greeting.txt has the wrong content"
    fi
    if ! grep -qx 'Goodbye from VOWP' "$project/src/farewell.txt" 2>/dev/null; then
        fail "$case_name: merged src/farewell.txt has the wrong content"
    fi
    if ! grep -qx 'Hello from VOWP' "$project/src/message.txt" 2>/dev/null ||
        ! grep -qx 'Goodbye from VOWP' "$project/src/message.txt" 2>/dev/null; then
        fail "$case_name: merged src/message.txt has the wrong content"
    fi

    echo "ok $case_name (workdir $workdir)"
}

run_case default default "default WORK (temporary directory)"
run_case relative relative "relative WORK=relwork"
run_case spaced spaced "WORK containing a space"

if [ "$failures" -ne 0 ]; then
    echo "hello-vowp tests: $failures failure(s)" >&2
    exit 1
fi

echo "hello-vowp tests: all 3 scenarios passed"
