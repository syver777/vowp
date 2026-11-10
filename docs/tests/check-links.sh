#!/usr/bin/env bash
# Check that every relative Markdown link in the documentation resolves to a
# file in this repository, and that same-page/cross-page anchors in the
# user-facing pages point at a real heading.
#
# External URLs are not fetched: a link check must work offline and in CI.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd -- "$here/../.." && pwd)"

if ! command -v python3 >/dev/null 2>&1; then
  printf 'FAIL  python3 is required\n' >&2
  exit 1
fi

python3 - "$root" <<'PY'
import re
import sys
from pathlib import Path
from urllib.parse import unquote

root = Path(sys.argv[1])
files = sorted((root / "docs").rglob("*.md"))
readme = root / "README.md"
if readme.is_file():
    files.insert(0, readme)

# Anchors are enforced only in the user-facing documentation. The design spec
# is read as a reference and may use its own conventions.
owned_anchor_roots = [
    root / "README.md",
    root / "docs" / "concepts",
    root / "docs" / "guides",
    root / "docs" / "security.md",
    root / "docs" / "troubleshooting.md",
]


def is_owned(path):
    path = path.resolve()
    for owned in owned_anchor_roots:
        owned = owned.resolve()
        if path == owned:
            return True
        if owned.is_dir() and owned in path.parents:
            return True
    return False


def strip_code(text):
    """Remove fenced blocks and inline code so examples are not links."""
    lines = text.splitlines()
    out = []
    in_fence = False
    marker = ""
    for line in lines:
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            this = stripped[:3]
            if not in_fence:
                in_fence = True
                marker = this
            elif this == marker:
                in_fence = False
                marker = ""
            out.append("")
            continue
        if in_fence:
            out.append("")
        else:
            out.append(re.sub(r"`[^`\n]*`", "", line))
    return "\n".join(out)


def slugify(heading):
    text = heading.strip()
    text = re.sub(r"^#{1,6}\s*", "", text)
    text = re.sub(r"\s+#+\s*$", "", text)
    text = re.sub(r"!\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = text.replace("`", "").replace("*", "").replace("_", "")
    text = text.lower()
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    text = re.sub(r"\s+", "-", text.strip())
    return text


def anchors(path):
    found = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if re.match(r"^#{1,6}\s+", line):
            base = slugify(line)
            count = found.get(base, 0)
            found[base] = count + 1
            if count:
                found[f"{base}-{count}"] = 1
    return found


link_re = re.compile(
    r"!?\[([^\]]*)\]\(\s*(<[^>]*>|[^)\s]+)(?:\s+[\"'][^\"']*[\"'])?\s*\)"
)

failures = []
checked = 0
anchor_checked = 0
anchor_cache = {}

for path in files:
    text = strip_code(path.read_text(encoding="utf-8"))
    for match in link_re.finditer(text):
        target = match.group(2).strip()
        if target.startswith("<") and target.endswith(">"):
            target = target[1:-1].strip()
        if not target:
            continue
        if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", target):
            continue  # http, https, mailto, ssh, ...
        if target.startswith("//"):
            continue
        path_part, _, fragment = target.partition("#")
        path_part = unquote(path_part)
        fragment = unquote(fragment)
        if not path_part:
            resolved = path
        else:
            if path_part.startswith("/"):
                failures.append(
                    (path, target, "absolute path; use a repository-relative link")
                )
                continue
            resolved = (path.parent / path_part).resolve()
        checked += 1
        if not resolved.exists():
            failures.append((path, target, "target does not exist"))
            continue
        if fragment and resolved.suffix == ".md" and is_owned(resolved):
            if resolved not in anchor_cache:
                anchor_cache[resolved] = anchors(resolved)
            anchor_checked += 1
            if fragment not in anchor_cache[resolved]:
                failures.append((path, target, "heading anchor does not exist"))

if failures:
    for path, target, reason in failures:
        rel = path.relative_to(root)
        print(f"FAIL  {rel}: {target} — {reason}", file=sys.stderr)
    print(f"FAIL  check-links: {len(failures)} broken link(s)", file=sys.stderr)
    sys.exit(1)

print(
    f"PASS  check-links: {checked} relative links in {len(files)} files, "
    f"{anchor_checked} headings checked"
)
PY
