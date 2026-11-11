#!/bin/sh
# examples/hello-vowp/run.sh — run the three-job demo with fake adapters.
#
# The script builds a throwaway project and VOWP home under a temporary directory, starts
# a real supervisor, waits until hello-greeting, hello-farewell and hello-message are all
# `done`, and then checks that the merge queue put their files on `main`.
#
# Usage:
#   FAKE=1 ./run.sh                 # default FAKE=1
#   VOWP=/path/to/vowp FAKE=1 ./run.sh
#   KEEP=1 FAKE=1 ./run.sh          # keep the temporary home/repo for inspection
#
# Nothing here touches ~/.vowp, the real tmux server, or a real harness. A first `cargo run`
# build may fetch crates, just like any other cargo command in this repository.

set -eu

# The example may be launched from inside the user's tmux session. Drop the inherited
# session variables so every tmux client below (the supervisor creates a layout even with
# --no-tmux) talks to the private socket directory, never the caller's server.
unset TMUX TMUX_PANE

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
VOWP_REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)

FAKE=${FAKE:-1}
if [ "$FAKE" != "1" ]; then
    echo "hello-vowp only ships the fake-adapter mode; run it with FAKE=1" >&2
    exit 2
fi

# Pick the CLI: an explicit VOWP path, an installed binary, a built binary, or cargo run.
if [ -n "${VOWP:-}" ]; then
    VOWP_MODE=explicit
elif command -v vowp >/dev/null 2>&1; then
    VOWP_MODE=path
elif [ -x "$VOWP_REPO/target/debug/vowp" ]; then
    VOWP_MODE=local
elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/debug/vowp" ]; then
    VOWP_MODE=target
else
    VOWP_MODE=cargo
fi

vowp_cli() {
    case "$VOWP_MODE" in
        explicit) "$VOWP" "$@" ;;
        path) command vowp "$@" ;;
        local) "$VOWP_REPO/target/debug/vowp" "$@" ;;
        target) "$CARGO_TARGET_DIR/debug/vowp" "$@" ;;
        cargo) cargo run -q --manifest-path "$VOWP_REPO/Cargo.toml" -p vowp-cli --bin vowp -- "$@" ;;
    esac
}

WORK=${WORK:-}
CREATED_WORK=0
if [ -n "$WORK" ]; then
    if [ -e "$WORK" ]; then
        if [ ! -d "$WORK" ]; then
            echo "hello-vowp: WORK is not a directory: $WORK" >&2
            exit 2
        fi
        if [ -n "$(find "$WORK" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]; then
            echo "hello-vowp: WORK must be an empty directory: $WORK" >&2
            exit 2
        fi
    else
        mkdir -p "$WORK"
    fi
else
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/hello-vowp.XXXXXX")
    CREATED_WORK=1
fi

# Normalise WORK to an absolute physical path: git remotes, VOWP state and the fake worker's
# report path all need to keep working after run.sh changes directory, including when the
# caller passed WORK=relative-dir or a path containing spaces.
WORK=$(CDPATH= cd -- "$WORK" && pwd -P)
HOME_DIR="$WORK/home"
REPO="$WORK/hello-vowp"
ORIGIN="$WORK/origin.git"
TIMEOUT=${TIMEOUT:-180}
# The CLI keeps its tmux socket under the node home. Use that same private
# directory and an explicit private socket; never the caller's default server.
TMUX_TMPDIR="$HOME_DIR/tmux"
VOWP_TMUX_SOCKET="vowp-hello-$$"
case "$VOWP_TMUX_SOCKET" in
    ""|default) echo "hello-vowp: refusing to use the default tmux server" >&2; exit 2 ;;
esac
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR VOWP_TMUX_SOCKET

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ -f "$HOME_DIR/supervisor.lock" ]; then
        (cd "$REPO" && vowp_cli --home "$HOME_DIR" --project "$REPO" down --force >/dev/null 2>&1) || true
    fi
    if command -v tmux >/dev/null 2>&1; then
        tmux -L "$VOWP_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    fi
    if [ "$status" -eq 0 ] && [ "${KEEP:-0}" != "1" ] && [ "$CREATED_WORK" = "1" ]; then
        rm -rf "$WORK"
    else
        echo "hello-vowp: workdir kept at $WORK" >&2
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

mkdir -p "$HOME_DIR" "$REPO"

echo "hello-vowp: workdir $WORK"
echo "hello-vowp: VOWP home $HOME_DIR"
echo "hello-vowp: project $REPO"
echo "hello-vowp: creating the demo repository and VOWP home"

git init -q -b main "$REPO"
git init -q --bare -b main "$ORIGIN"
(
    cd "$REPO"
    git config user.name "Hello VOWP"
    git config user.email "hello@example.invalid"
)

# Initialise the node home and the project registration, then install the fake-adapter
# config. `vowp init` writes its own default vowp.toml first; the example replaces it.
(
    cd "$REPO"
    vowp_cli --home "$HOME_DIR" init --name hello-vowp --no-framework >/dev/null
)
# `vowp up` schedules the node supervisor from the home-level config, not the project
# config, so give the demo a one-second tick there too. The project config carries the
# same value for project-scoped commands.
cat > "$HOME_DIR/config.toml" <<'EOF'
[node]
tick_secs = 1
EOF
# The temporary checkout path is only known at run time. Materialise the absolute worktree
# root inside the checkout; the absolute path also keeps the report path in the harness
# prompt absolute, which the fake worker relies on. `.vowp/` is already ignored.
worktree_root="$REPO/.vowp/worktrees"
escaped_worktree_root=$(printf '%s' "$worktree_root" | sed -e 's/[\\&|]/\\&/g' -e 's/"/\\"/g')
sed "s|@WORKTREE_ROOT@|$escaped_worktree_root|" "$ROOT/vowp.toml" > "$REPO/vowp.toml"
mkdir -p "$REPO/.vowp/adapters"
cp "$ROOT/mapping.toml" "$REPO/.vowp/adapters/fake-worker.toml"
cp "$ROOT/fake-worker.sh" "$REPO/.vowp/adapters/fake-worker.sh"
chmod +x "$REPO/.vowp/adapters/fake-worker.sh"
cp -R "$ROOT/project/." "$REPO/"

# The initial commit is what the jobs branch from, and the local bare origin is what the
# merge queue fetches, rebases onto, and pushes to.
(
    cd "$REPO"
    git add -A
    git commit -q -m "hello-vowp: initial demo project"
    git remote add origin "$ORIGIN"
    git push -q -u origin main
)

# The generic CLI adapter resolves its mapping file relative to the process working
# directory when [project].path is unset, so every later command runs from the checkout.
cd "$REPO"

echo "hello-vowp: adding the three-job graph"

vowp_cli --home "$HOME_DIR" --project "$REPO" job add \
    --id hello-greeting \
    --title "Write the greeting" \
    --kind build \
    --owns "src/greeting.txt" \
    --gates "report,owns_respected,grep -qx 'Hello from VOWP' src/greeting.txt" \
    --brief-file "$ROOT/briefs/hello-greeting.md"

vowp_cli --home "$HOME_DIR" --project "$REPO" job add \
    --id hello-farewell \
    --title "Write the farewell" \
    --kind build \
    --owns "src/farewell.txt" \
    --gates "report,owns_respected,grep -qx 'Goodbye from VOWP' src/farewell.txt" \
    --brief-file "$ROOT/briefs/hello-farewell.md"

vowp_cli --home "$HOME_DIR" --project "$REPO" job add \
    --id hello-message \
    --title "Assemble the message" \
    --kind build \
    --needs "hello-greeting,hello-farewell" \
    --owns "src/message.txt" \
    --gates "report,owns_respected,grep -qx 'Hello from VOWP' src/message.txt && grep -qx 'Goodbye from VOWP' src/message.txt" \
    --brief-file "$ROOT/briefs/hello-message.md"

vowp_cli --home "$HOME_DIR" --project "$REPO" graph

# The supervisor's child processes (the fake harness) resolve fake-worker.sh through PATH.
PATH="$REPO/.vowp/adapters:$PATH"
export PATH

# The supervisor ensures a window layout even when `vowp up` is called with --no-tmux,
# so give it a private tmux socket directory. The real tmux server is never contacted.
echo "hello-vowp: starting the supervisor"
vowp_cli --home "$HOME_DIR" --project "$REPO" up --detach --no-tmux

job_state() {
    vowp_cli --home "$HOME_DIR" --project "$REPO" --json job show "$1" 2>/dev/null \
        | tr -d ' \n' \
        | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p'
}

show_failure_diagnostics() {
    vowp_cli --home "$HOME_DIR" --project "$REPO" job ls --all >&2 || true
    echo "--- supervisor.log tail ---" >&2
    tail -n 80 "$HOME_DIR/logs/supervisor.log" >&2 || true
}

deadline=$(( $(date +%s) + TIMEOUT ))
while :; do
    pending=""
    for id in hello-greeting hello-farewell hello-message; do
        state=$(job_state "$id")
        case "$state" in
            done) ;;
            stalled|failed|parked|cancelled)
                echo "hello-vowp: job $id is $state; stopping" >&2
                show_failure_diagnostics
                exit 1
                ;;
            *) pending="$pending $id" ;;
        esac
    done
    if [ -z "$pending" ]; then
        break
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "hello-vowp: timed out after ${TIMEOUT}s; still waiting for:$pending" >&2
        show_failure_diagnostics
        exit 1
    fi
    sleep 1
done

echo "hello-vowp: all three jobs are done; stopping the supervisor"
vowp_cli --home "$HOME_DIR" --project "$REPO" down

# The acceptance check: merge-queue results are in the main checkout and on origin/main.
grep -qx 'Hello from VOWP' "$REPO/src/greeting.txt"
grep -qx 'Goodbye from VOWP' "$REPO/src/farewell.txt"
{
    printf '%s\n' 'Hello from VOWP'
    printf '%s\n' 'Goodbye from VOWP'
} > "$WORK/expected-message.txt"
cmp -s "$WORK/expected-message.txt" "$REPO/src/message.txt"
for file in src/greeting.txt src/farewell.txt src/message.txt; do
    git --git-dir="$ORIGIN" show "main:$file" > "$WORK/origin-$(basename "$file")"
done
cmp -s "$WORK/expected-message.txt" "$WORK/origin-message.txt"
grep -qx 'Hello from VOWP' "$WORK/origin-greeting.txt"
grep -qx 'Goodbye from VOWP' "$WORK/origin-farewell.txt"

vowp_cli --home "$HOME_DIR" --project "$REPO" status
vowp_cli --home "$HOME_DIR" --project "$REPO" job ls --all

echo
echo "hello-vowp: all three jobs reached done and origin/main has the result"
echo "hello-vowp: rerun with KEEP=1 to inspect the temporary home and repository"
