#!/usr/bin/env bash
# Build one release archive and its per-archive checksum.
#
# Usage:
#   scripts/release/package-archive.sh \
#     --version 0.3.0 \
#     --target x86_64-unknown-linux-gnu \
#     --binary target/x86_64-unknown-linux-gnu/release/vowp \
#     --dist dist
#
# The archive is dist/vowp-<version>-<target>.tar.gz and contains a top-level
# directory of the same name with vowp, LICENSE, README.md and
# THIRD-PARTY-LICENSES.md. The script refuses a missing or empty input and
# prints the sorted tar listing.
#
# Exit status: 0 on success, 2 on a usage error, 1 on a packaging failure.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

usage() {
  sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

fail() {
  printf 'package-archive: %s\n' "$*" >&2
  exit 1
}

version=
target=
binary=
dist=dist

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      version=$2
      shift 2
      ;;
    --target)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      target=$2
      shift 2
      ;;
    --binary)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      binary=$2
      shift 2
      ;;
    --dist)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      dist=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'package-archive: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

[ -n "$version" ] || { usage >&2; exit 2; }
[ -n "$target" ] || { usage >&2; exit 2; }
[ -n "$binary" ] || { usage >&2; exit 2; }
case "$version" in
  *[!0-9A-Za-z.+-]*) fail "invalid version: $version" ;;
esac
case "$target" in
  *[!0-9A-Za-z_.-]*) fail "invalid target: $target" ;;
esac

[ -f "$binary" ] || fail "binary is missing: $binary"
[ -s "$binary" ] || fail "binary is empty: $binary"
for file in LICENSE README.md THIRD-PARTY-LICENSES.md; do
  [ -f "$ROOT/$file" ] || fail "$file is missing from $ROOT"
  [ -s "$ROOT/$file" ] || fail "$file is empty in $ROOT"
done

name="vowp-${version}-${target}"
staging="$dist/$name"
archive="$dist/$name.tar.gz"

mkdir -p "$dist"
rm -rf "$staging"
mkdir -p "$staging"

cleanup() {
  rm -rf "$staging"
}
trap cleanup EXIT HUP INT TERM

cp "$binary" "$staging/vowp"
chmod 0755 "$staging/vowp"
cp "$ROOT/LICENSE" "$ROOT/README.md" "$ROOT/THIRD-PARTY-LICENSES.md" "$staging/"

for file in vowp LICENSE README.md THIRD-PARTY-LICENSES.md; do
  [ -s "$staging/$file" ] || fail "$file is missing or empty in $staging"
done

COPYFILE_DISABLE=1 tar -C "$dist" -czf "$archive" "$name"
rm -rf "$staging"
trap - EXIT HUP INT TERM

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist" && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256")
else
  (cd "$dist" && shasum -a 256 "${name}.tar.gz" > "${name}.tar.gz.sha256")
fi

(cd "$dist" && tar -tzf "${name}.tar.gz" | sort)
