#!/usr/bin/env bash
# Check that every VOWP crate packages cleanly and that every publish dry run works.
#
# This script never publishes anything and never needs a crates.io token. It is the
# repeatable form of the release package checks.
#
# Usage:
#   scripts/release/check-packages.sh [--help]
#
# Exit status: 0 when every crate packages and every dry run passes, 1 otherwise.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO=${CARGO:-cargo}

# Publish order: dependencies first.
CRATES=(
  vowp-protocol
  vowp-store
  vowp-graph
  vowp-tmux
  vowp-adapters
  vowp-core
  vowp-cli
)

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  "") ;;
  *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
esac

for tool in "$CARGO" grep awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 1; }
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/vowp-check-packages.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
declare -a RESULTS=()

say() { printf '%s\n' "$*"; }

# Sibling crates are not on crates.io during a local dry run. Patch the registry names to
# the local paths so cargo can resolve and verify the packaged manifest without publishing.
# The generated package manifest still records the registry dependency; nothing is uploaded.
patch_args_for() {
  local crate=$1
  PATCH_ARGS=()
  local deps=()
  case "$crate" in
    vowp-protocol|vowp-tmux) ;;
    vowp-store|vowp-graph|vowp-adapters) deps=(vowp-protocol) ;;
    vowp-core) deps=(vowp-protocol vowp-store vowp-graph vowp-tmux vowp-adapters) ;;
    vowp-cli) deps=(vowp-protocol vowp-store vowp-graph vowp-tmux vowp-adapters vowp-core) ;;
    *) echo "internal error: no dependency map for $crate" >&2; exit 1 ;;
  esac
  local dep
  for dep in "${deps[@]}"; do
    PATCH_ARGS+=(--config "patch.crates-io.$dep.path=\"crates/$dep\"")
  done
}

# Reject files that must never be shipped: build output, VCS data, secrets, local
# databases, test fixtures or anything outside the package root.
check_file_list() {
  local crate=$1 list=$2
  local bad=0

  if grep -Eq '(^|/)target/' "$list"; then
    say "  junk: target/ entry"; bad=1
  fi
  if grep -Eq '(^|/)\.git/' "$list"; then
    say "  junk: .git entry"; bad=1
  fi
  if grep -Eq '(^|/)\.env($|\.)' "$list"; then
    say "  junk: .env entry"; bad=1
  fi
  if grep -Eq '(^|/)tests/' "$list"; then
    say "  junk: tests/ entry (tests are excluded from published packages)"; bad=1
  fi
  if grep -Eq '\.(sqlite|sqlite3|db)$' "$list"; then
    say "  junk: local database entry"; bad=1
  fi
  if grep -Eq '^/|(^|/)\.\.(/|$)' "$list"; then
    say "  junk: path outside the package root"; bad=1
  fi

  grep -qx 'Cargo.toml' "$list" || { say "  missing Cargo.toml"; bad=1; }
  if [ "$crate" = "vowp-cli" ]; then
    grep -qx 'crates/vowp-cli/README.md' "$list" || { say "  missing crates/vowp-cli/README.md"; bad=1; }
    grep -q '^crates/vowp-cli/src/' "$list" || { say "  missing crates/vowp-cli/src/"; bad=1; }
    grep -qx 'crates/vowp-cli/src/main.rs' "$list" || { say "  missing crates/vowp-cli/src/main.rs"; bad=1; }
    grep -qx 'framework/templates/AGENTS.md' "$list" || { say "  missing embedded framework files"; bad=1; }
  else
    grep -qx 'README.md' "$list" || { say "  missing README.md"; bad=1; }
    grep -q '^src/' "$list" || { say "  missing src/"; bad=1; }
  fi
  return "$bad"
}

say "VOWP package checks (dry run only; nothing is published)"
say "workspace: $ROOT"
say ""

for crate in "${CRATES[@]}"; do
  list="$WORK/$crate.list"
  log="$WORK/$crate.log"
  status="ok"
  files="?"

  say "== $crate"
  if ! "$CARGO" package --list -p "$crate" --allow-dirty >"$list" 2>"$log"; then
    status="FAIL (package --list)"
    say "  cargo package --list failed; see output below"
    tail -20 "$log" >&2 || true
    RESULTS+=("$crate|?|FAIL|$status")
    fail=$((fail + 1))
    continue
  fi
  files=$(wc -l <"$list" | tr -d ' ')

  if ! check_file_list "$crate" "$list"; then
    status="FAIL (file list)"
    RESULTS+=("$crate|$files|FAIL|$status")
    fail=$((fail + 1))
    continue
  fi

  patch_args_for "$crate"
  if "$CARGO" publish --dry-run -p "$crate" --allow-dirty "${PATCH_ARGS[@]}" >"$log" 2>&1; then
    say "  package list: $files files, clean"
    say "  publish dry run: ok"
    RESULTS+=("$crate|$files|ok|$status")
    pass=$((pass + 1))
  else
    status="FAIL (publish --dry-run)"
    say "  publish dry run failed; last output:"
    tail -20 "$log" >&2 || true
    RESULTS+=("$crate|$files|ok|$status")
    fail=$((fail + 1))
  fi
done

say ""
printf '%-16s %8s %10s  %s\n' "CRATE" "FILES" "LIST" "PUBLISH DRY RUN"
printf '%-16s %8s %10s  %s\n' "----------------" "--------" "----------" "----------------"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r crate files list_status dry_status <<<"$row"
  printf '%-16s %8s %10s  %s\n' "$crate" "$files" "$list_status" "$dry_status"
done

say ""
say "Note: sibling crates are patched to local paths for the dry run; the packaged"
say "manifests still contain registry dependencies. Nothing was uploaded."
say ""
if [ "$fail" -ne 0 ]; then
  say "RESULT: FAIL ($pass passed, $fail failed)"
  exit 1
fi
say "RESULT: PASS ($pass crates packaged and verified)"
