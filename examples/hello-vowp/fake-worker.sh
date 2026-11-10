#!/bin/sh
# Fake harness for examples/hello-vowp. It never calls a model or the network.
#
# VOWP starts it in the job worktree with the L2 prompt on stdin. The script reads the
# report path from that prompt, writes the tiny file the job owns, commits it, and writes
# a report with the front-matter the `report` gate requires. This is a fake adapter, not
# a test double that pretends work happened: the gate and merge queue still run for real.

set -eu

if [ "${1:-}" = "--version" ]; then
    printf '%s\n' "fake-worker 1.0"
    exit 0
fi

prompt=$(cat)

# The L2 prompts name the report path (start: "write your report to …"; continue:
# "(…/report.md)"; fix: "update …/report.md. Do not widen scope."). The path is absolute
# and may contain spaces, so match each prompt shape up to its end marker instead of
# extracting a whitespace-delimited token. Planner/keeper turns have no job report, so
# they are a no-op.
report=$(
    printf '%s\n' "$prompt" |
        sed -n \
            -e 's|^.*write your report to \(.*/\.vowp/jobs/[A-Za-z0-9_-]*/report\.md\)$|\1|p' \
            -e 's|^.*writing your report (\(.*/\.vowp/jobs/[A-Za-z0-9_-]*/report\.md\)).*$|\1|p' \
            -e 's|^.*update \(.*/\.vowp/jobs/[A-Za-z0-9_-]*/report\.md\)\. Do not widen scope\.$|\1|p' |
        head -n 1
)
if [ -z "$report" ]; then
    printf '%s\n' "fake-worker: no job report in this prompt; nothing to do"
    exit 0
fi

job=$(basename "$(dirname "$report")")
branch=$(git rev-parse --abbrev-ref HEAD)

case "$job" in
    hello-greeting)
        file=src/greeting.txt
        mkdir -p src
        printf '%s\n' "Hello from VOWP" > "$file"
        ;;
    hello-farewell)
        file=src/farewell.txt
        mkdir -p src
        printf '%s\n' "Goodbye from VOWP" > "$file"
        ;;
    hello-message)
        file=src/message.txt
        mkdir -p src
        {
            cat src/greeting.txt
            cat src/farewell.txt
        } > "$file"
        ;;
    *)
        printf '%s\n' "fake-worker: unknown job $job" >&2
        exit 1
        ;;
esac

git add -- "$file"
if ! git diff --cached --quiet; then
    git \
        -c user.name="Fake Worker" \
        -c user.email="fake@example.invalid" \
        commit -q -m "$job: fake worker output"
fi
sha=$(git rev-parse --short HEAD)

mkdir -p "$(dirname "$report")"
cat > "$report" <<EOF
---
job: "$job"
status: done
branch: "$branch"
commits: ["$sha"]
tests: "the job's cmd gate re-checks $file"
files_changed: ["$file"]
open_items: []
questions_for_owner: []
---

# $job

The fake worker wrote \`$file\` on branch \`$branch\` and committed it.
EOF

printf '%s\n' "fake-worker: finished $job on $branch ($sha)"
