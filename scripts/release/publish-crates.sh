#!/usr/bin/env bash
# Publish the VOWP crates in dependency order.
#
# DRY RUN BY DEFAULT: without --execute this script runs the package/publish-dry-run checks
# from scripts/release/check-packages.sh and prints the commands it would run. It never
# uploads and never needs a token.
#
# With --execute it publishes one crate at a time with `cargo publish -p <crate>`, waits for
# the new version to appear in the crates.io sparse index, and only then publishes the next
# crate. It stops at the first failure and prints a summary table.
#
# The crates.io token is read ONLY from the CARGO_REGISTRY_TOKEN environment variable. The
# token is never written to a file and never printed. Cargo reads the same variable itself,
# so the script does not pass --token.
#
# Usage:
#   scripts/release/publish-crates.sh                 # dry run (default)
#   CARGO_REGISTRY_TOKEN=... scripts/release/publish-crates.sh --execute
#
# Optional environment:
#   VOWP_INDEX_WAIT_SECS   seconds to wait for a crate to become visible (default 600)
#   VOWP_INDEX_POLL_SECS   seconds between index polls (default 10)

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO=${CARGO:-cargo}
EXECUTE=0

usage() {
  sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --execute) EXECUTE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

for tool in "$CARGO" git python3 curl grep sort sleep; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 1; }
done

# Publish order: dependencies first.
CRATES=(
  vowp-protocol
  vowp-store
  vowp-graph
  vowp-tmux
  vowp-adapters
  vowp-core
  vowp-daemon
  vowp-cli
)

VERSION=$(cd "$ROOT" && "$CARGO" metadata --no-deps --format-version 1 2>/dev/null |
  python3 -c 'import json, sys
d = json.load(sys.stdin)
print(next(p["version"] for p in d["packages"] if p["name"] == "vowp-protocol"))')

if [ -z "$VERSION" ]; then
  echo "could not determine the workspace version" >&2
  exit 1
fi

WAIT_SECS=${VOWP_INDEX_WAIT_SECS:-600}
POLL_SECS=${VOWP_INDEX_POLL_SECS:-10}

say() { printf '%s\n' "$*"; }

# crates.io sparse index path for a crate name.
index_path() {
  local name=$1 len=${#1}
  if [ "$len" -eq 1 ]; then
    echo "1/$name"
  elif [ "$len" -eq 2 ]; then
    echo "2/$name"
  elif [ "$len" -eq 3 ]; then
    echo "3/${name:0:1}/$name"
  else
    echo "${name:0:2}/${name:2:2}/$name"
  fi
}

crate_is_visible() {
  local crate=$1 path
  path=$(index_path "$crate")
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 15 "https://index.crates.io/$path" 2>/dev/null |
      grep -q "\"vers\":\"$VERSION\""
  else
    "$CARGO" search "$crate" --limit 1 2>/dev/null |
      grep -q "^$crate = \"$VERSION\""
  fi
}

wait_for_crate() {
  local crate=$1 waited=0
  while ! crate_is_visible "$crate"; do
    if [ "$waited" -ge "$WAIT_SECS" ]; then
      return 1
    fi
    sleep "$POLL_SECS"
    waited=$((waited + POLL_SECS))
  done
  return 0
}

print_summary() {
  local row crate status
  say ""
  printf '%-16s %-10s %s\n' "CRATE" "VERSION" "STATUS"
  printf '%-16s %-10s %s\n' "----------------" "----------" "------------------------------"
  for row in "${SUMMARY[@]}"; do
    IFS='|' read -r crate status <<<"$row"
    printf '%-16s %-10s %s\n' "$crate" "$VERSION" "$status"
  done
}

if [ "$EXECUTE" -eq 0 ]; then
  say "VOWP crates.io publish — DRY RUN (nothing will be uploaded)"
  say "workspace: $ROOT"
  say "version:   $VERSION"
  say ""
  if "$ROOT/scripts/release/check-packages.sh"; then
    say ""
    say "Planned publish order (real run with --execute):"
    for crate in "${CRATES[@]}"; do
      say "  cargo publish -p $crate"
      say "    then wait until $crate $VERSION is visible in the crates.io index"
    done
    say ""
    say "The real run needs CARGO_REGISTRY_TOKEN in the environment; a dry run does not."
    say "DRY RUN COMPLETE: no crate was published."
    exit 0
  else
    rc=$?
    say ""
    say "DRY RUN FAILED: fix the package checks above before publishing."
    exit "$rc"
  fi
fi

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  echo "CARGO_REGISTRY_TOKEN is not set; refusing to publish." >&2
  echo "Set it in the environment of this command (never in a file committed to the repo)." >&2
  exit 1
fi

if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "the working tree is not clean; refusing to publish an uncommitted tree." >&2
  echo "Commit the release first, then re-run with --execute." >&2
  exit 1
fi

say "VOWP crates.io publish — EXECUTE"
say "workspace: $ROOT"
say "version:   $VERSION"
say "token:     CARGO_REGISTRY_TOKEN (value not shown)"
say ""

SUMMARY=()
fail=0
for crate in "${CRATES[@]}"; do
  if [ "$fail" -ne 0 ]; then
    SUMMARY+=("$crate|skipped after earlier failure")
    continue
  fi
  say "== publishing $crate $VERSION"
  if ! "$CARGO" publish -p "$crate"; then
    SUMMARY+=("$crate|FAILED (cargo publish)")
    fail=1
    continue
  fi
  say "   waiting for $crate $VERSION in the crates.io index (up to ${WAIT_SECS}s)"
  if wait_for_crate "$crate"; then
    SUMMARY+=("$crate|published and visible")
    say "   visible"
  else
    SUMMARY+=("$crate|published, NOT visible after ${WAIT_SECS}s")
    fail=1
  fi
done

print_summary
say ""
if [ "$fail" -ne 0 ]; then
  say "RESULT: FAIL — stopped at the first failure; later crates were not published."
  say "Check crates.io before retrying. Published versions cannot be replaced; only yanked."
  exit 1
fi
say "RESULT: PASS — all ${#CRATES[@]} crates are published and visible."
