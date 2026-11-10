#!/usr/bin/env bash
# Check that every `vowp` command shown in the user documentation exists in the
# real CLI, and that the documented status matches the implementation: an
# unimplemented command must be marked *(planned)*, and an implemented command
# must not be.
#
# The command surface is parsed from docs/design/cli.md (the written contract)
# and cross-checked against V0_1_COMMANDS in crates/vowp-cli/src/cli.rs. The
# implementation status comes from the CLI dispatch table: a command is current
# when its handler module is real code, not an explicit NotImplemented stub. A
# required-set assertion catches parser drift.
#
# Scope: README.md, docs/concepts/**, docs/guides/**, docs/security.md,
# docs/troubleshooting.md, docs/roadmap.md and docs/design/v0.2/**. Other
# docs/design/** pages are the reference, not a consumer.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd -- "$here/../.." && pwd)"

if ! command -v python3 >/dev/null 2>&1; then
  printf 'FAIL  python3 is required\n' >&2
  exit 1
fi

python3 - "$root" <<'PY'
import re
import shlex
import sys
from pathlib import Path

root = Path(sys.argv[1])
cli_path = root / "docs" / "design" / "cli.md"

if not cli_path.is_file():
    print(f"FAIL  missing {cli_path}", file=sys.stderr)
    sys.exit(1)

cli = cli_path.read_text(encoding="utf-8")
families = {"job", "session", "inbox", "node", "schedule"}

# --- Parse the command surface out of cli.md -------------------------------
#
# Commands appear in cli.md in three shapes:
#   * a backticked invocation, e.g. `vowp init ...` or `vowp status --check`
#   * a table row whose first cell is only the subcommand, e.g. `job add ...`
#   * a heading with shorthands: `vowp inbox ls ... · show ... · ask ...`
# We derive the set; the documentation side is checked against it below.

known = set()


def add_invocation(text):
    """Add the command path found at the start of a piece of text."""
    match = re.match(r"\s*vowp\s+(--version|--help|[a-z_][a-z0-9_-]*)", text)
    if not match:
        return None
    command = match.group(1)
    if command == "--help":
        # clap provides --help, but cli.md documents the command surface, not
        # that flag. Do not treat it as an invocable command.
        return None
    path = command
    rest = text[match.end():].lstrip()
    if command in families:
        sub = re.match(r"([a-z_][a-z0-9_-]*)", rest)
        if sub:
            path = f"{command} {sub.group(1)}"
    known.add(path)
    return path


# Backticked spans (headings, tables, prose).
for span in re.findall(r"`([^`\n]+)`", cli):
    add_invocation(span)
    if "·" in span and "vowp " in span:
        parts = span.split("·")
        family = None
        first = re.search(r"\bvowp\s+([a-z_][a-z0-9_-]*)", parts[0])
        if first:
            family = first.group(1)
        if family in families:
            for part in parts[1:]:
                sub = re.match(r"\s*([a-z_][a-z0-9_-]*)", part)
                if sub:
                    known.add(f"{family} {sub.group(1)}")

# Table rows that name only the subcommand.
for family, sub in re.findall(
    r"`(job|session|inbox|node|schedule)\s+([a-z_][a-z0-9_-]*)", cli
):
    known.add(f"{family} {sub}")

required = {
    "init", "doctor", "up", "down", "status", "board", "plan", "attach",
    "graph", "chat", "cost", "loops", "promote", "rollback", "releases",
    "reindex", "backup", "schema", "version", "--version", "__window",
    "job add", "job update", "job ls", "job show", "job retry", "job park",
    "job nudge", "job restart", "job replan", "job fail", "job cancel",
    "session ls", "session show", "session new", "session chat",
    "session switch", "session capsule", "session export", "session import",
    "session watch", "session tail",
    "inbox ls", "inbox show", "inbox ask", "inbox answer", "inbox dismiss",
    "node add", "node ls", "node rm",
    "schedule add", "schedule ls", "schedule pause", "schedule resume",
    "schedule rm",
}
missing_in_cli = sorted(required - known)
if missing_in_cli:
    print(
        "FAIL  cli.md parser did not find required command(s): "
        + ", ".join(missing_in_cli),
        file=sys.stderr,
    )
    sys.exit(1)

# A family name alone ("vowp job") is a section title, not an invocable
# command. Keep only concrete two-word paths.
for family in families:
    known.discard(family)

# --- Parse the real CLI's top-level command list and dispatch --------------
#
# V0_1_COMMANDS is the clap surface: every command the binary parses. The
# dispatch table maps each Command variant to its handler module. A module is
# a stub when it only returns CliError::NotImplemented. This check therefore
# distinguishes a command that merely appears in --help from one that actually
# runs.

cli_rs_path = root / "crates" / "vowp-cli" / "src" / "cli.rs"
dispatch_path = root / "crates" / "vowp-cli" / "src" / "cmd" / "mod.rs"
if not cli_rs_path.is_file() or not dispatch_path.is_file():
    print("FAIL  cannot find the vowp-cli source to check implementation status", file=sys.stderr)
    sys.exit(1)

cli_rs = cli_rs_path.read_text(encoding="utf-8")
const_match = re.search(
    r"pub const V0_1_COMMANDS:.*?=.*?\[(.*?)\];", cli_rs, re.DOTALL
)
if not const_match:
    print("FAIL  cannot parse V0_1_COMMANDS from cli.rs", file=sys.stderr)
    sys.exit(1)
real_top = set(re.findall(r'"(__window|[a-z_][a-z0-9_-]*)"', const_match.group(1)))
real_top.add("--version")

dispatch = dispatch_path.read_text(encoding="utf-8")
module_for_variant = {}
for variant, module in re.findall(
    r"Command::(\w+)(?:\s*\{[^}]*\}|\s*\([^)]*\))?\s*=>\s*([a-z_]+)::run", dispatch
):
    module_for_variant[variant] = module
if not module_for_variant:
    print("FAIL  cannot parse the CLI dispatch table", file=sys.stderr)
    sys.exit(1)


def variant_for(command):
    if command == "__window":
        return "Window"
    return "".join(part.capitalize() for part in command.split("_"))


def module_is_stub(module):
    path = root / "crates" / "vowp-cli" / "src" / "cmd" / f"{module}.rs"
    if not path.is_file():
        return True
    return "CliError::NotImplemented" in path.read_text(encoding="utf-8")


implemented = {"--version"}
for command in real_top:
    if command == "--version":
        continue
    module = module_for_variant.get(variant_for(command))
    if not module:
        # init and doctor run before the project context exists, so they are
        # handled directly in lib.rs rather than through the dispatch table.
        module = command.strip("_").replace("-", "_")
    if module and not module_is_stub(module):
        implemented.add(command)

unknown_to_binary = sorted(
    command for command in known if command.split()[0] not in real_top
)
if unknown_to_binary:
    print(
        "FAIL  docs/design/cli.md names command(s) the binary does not parse: "
        + ", ".join(unknown_to_binary),
        file=sys.stderr,
    )
    sys.exit(1)

# --- Scan the documentation ------------------------------------------------

doc_files = [root / "README.md"]
for directory in ("concepts", "guides"):
    doc_files.extend(sorted((root / "docs" / directory).glob("*.md")))
doc_files.extend(
    [
        root / "docs" / "security.md",
        root / "docs" / "troubleshooting.md",
        root / "docs" / "roadmap.md",
    ]
)
doc_files.extend(sorted((root / "docs" / "design" / "v0.2").glob("*.md")))
doc_files = [p for p in doc_files if p.is_file()]


def parse_command(line):
    """Return (path, original line) for a line that invokes vowp, else None."""
    line = line.strip()
    if line.startswith("$ "):
        line = line[2:].strip()
    if not line:
        return None
    # Accept the documented way to run the workspace binary.
    prefix = "cargo run -p vowp-cli --bin vowp -- "
    if line.startswith(prefix):
        line = "vowp " + line[len(prefix):]
    if not re.match(r"^vowp(\s|$)", line):
        return None
    # Drop a trailing shell comment. Do not split on '#' without whitespace:
    # executor variants such as 'opencode:provider/model#max' use it.
    line = re.sub(r"\s+#\s.*$", "", line).strip()
    try:
        tokens = shlex.split(line, comments=False, posix=True)
    except ValueError:
        tokens = line.split()
    if not tokens or tokens[0] != "vowp":
        return None
    i = 1
    if i < len(tokens) and tokens[i] in ("--version", "-V"):
        return "--version", line
    global_with_value = {"--node", "--project", "--home"}
    while i < len(tokens) and tokens[i].startswith("-"):
        if tokens[i] in global_with_value:
            i += 2
        else:
            i += 1
    if i >= len(tokens):
        return None
    command = tokens[i]
    if not re.fullmatch(r"--version|[a-z_][a-z0-9_-]*", command):
        # A placeholder or output line, not a command invocation.
        return None
    path = command
    if command in families:
        # A bare family name ("vowp job") is a section label, not an
        # invocation; the CLI requires a subcommand.
        if i + 1 >= len(tokens):
            return None
        sub = tokens[i + 1]
        if not re.fullmatch(r"[a-z_][a-z0-9_-]*", sub) or sub.startswith("-"):
            return None
        path = f"{command} {sub}"
    return path, line


references = []
page_lines = {}
for path in doc_files:
    lines = path.read_text(encoding="utf-8").splitlines()
    page_lines[path] = lines
    in_fence = False
    fence_marker = ""
    for number, line in enumerate(lines, 1):
        stripped = line.strip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            marker = stripped[:3]
            if not in_fence:
                in_fence = True
                fence_marker = marker
            elif marker == fence_marker:
                in_fence = False
                fence_marker = ""
            continue
        if in_fence:
            parsed = parse_command(line)
            if parsed:
                references.append((path, number, parsed[0], parsed[1]))
        else:
            # Inline code spans on prose lines and tables.
            for span in re.findall(r"`([^`\n]+)`", line):
                parsed = parse_command(span)
                if parsed:
                    references.append((path, number, parsed[0], parsed[1]))

# A page that shows commands must declare its status: current, planned or
# mixed. The declaration is the leading `Status:` line (optionally blockquoted
# and bold), not an incidental `status:` in a fenced example. A page declared
# planned covers all its commands; a mixed page uses *(planned)* locally for
# the not-yet-implemented ones.
page_status = {}
status_re = re.compile(
    r"^(?:>\s*)?(?:\*\*)?status:\s*(current|planned|mixed)\b", re.IGNORECASE
)
for path in sorted({path for path, _, _, _ in references}):
    in_fence = False
    fence_marker = ""
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            marker = stripped[:3]
            if not in_fence:
                in_fence = True
                fence_marker = marker
            elif marker == fence_marker:
                in_fence = False
                fence_marker = ""
            continue
        if in_fence:
            continue
        match = status_re.match(line)
        if match:
            page_status[path] = match.group(1).lower()
            break


SECTION_MARKER = re.compile(r"^\s*(?:>\s*)?\*\*planned section[:.]?\*\*", re.IGNORECASE)
HTML_SECTION_MARKER = "<!-- planned-section -->"
# A shell-comment marker, whether it is the whole line or trails a command on
# the same line. The whitespace before '#' keeps executor variants such as
# 'opencode:provider/model#max' out of the match.
PLANNED_COMMENT = re.compile(r"(?:^|\s)#\s*\(planned\)(?!\w)", re.IGNORECASE)


def planned_near(path, number):
    """True when the reference itself or its explicit section/block marker says planned.

    A stray "(planned)" sentence in the same prose section is not enough. The
    accepted declarations are the literal *(planned)* on the reference line, a
    leading or trailing `# (planned)` comment inside the same fenced block, or an
    explicit `**Planned section:**` / `<!-- planned-section -->` line in the
    section. The comment may be the whole line or trail a command on the same
    line, e.g. `vowp up # (planned)`.
    """
    lines = page_lines[path]
    index = number - 1
    current = lines[index].strip()
    if (
        "*(planned)*" in lines[index]
        or PLANNED_COMMENT.search(lines[index])
        or SECTION_MARKER.match(current)
        or HTML_SECTION_MARKER in current
    ):
        return True
    in_fence = False
    fence_marker = ""
    block_start = 0
    section_start = 0
    for i in range(0, index):
        stripped = lines[i].strip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            marker = stripped[:3]
            if not in_fence:
                in_fence = True
                fence_marker = marker
                block_start = i
            elif marker == fence_marker:
                in_fence = False
                fence_marker = ""
            continue
        if not in_fence and re.match(r"#{1,6}\s", stripped):
            section_start = i
    if in_fence:
        for line in lines[block_start:index]:
            if PLANNED_COMMENT.search(line.strip()):
                return True
        for line in lines[index + 1:]:
            stripped = line.strip()
            if stripped.startswith("```") or stripped.startswith("~~~"):
                if stripped[:3] == fence_marker:
                    break
                continue
            if PLANNED_COMMENT.search(stripped):
                return True
    for line in lines[section_start:index]:
        stripped = line.strip()
        if SECTION_MARKER.match(stripped) or HTML_SECTION_MARKER in stripped:
            return True
    return False


failures = []
for path, number, command, line in references:
    top = command.split()[0]
    if command not in known:
        rel = path.relative_to(root)
        failures.append(f"{rel}:{number}: `{command}` is not in docs/design/cli.md\n    {line}")
        continue
    if top not in real_top:
        rel = path.relative_to(root)
        failures.append(f"{rel}:{number}: `{command}` is not in the real CLI\n    {line}")
        continue
    marked = page_status.get(path) == "planned" or planned_near(path, number)
    if top in implemented:
        if marked:
            rel = path.relative_to(root)
            failures.append(
                f"{rel}:{number}: `{command}` is implemented but marked "
                f"*(planned)*"
                f"\n    {line}"
            )
        continue
    if marked:
        continue
    rel = path.relative_to(root)
    failures.append(
        f"{rel}:{number}: `{command}` is not implemented and is not marked "
        f"*(planned)*"
        f"\n    {line}"
    )

# A page-level `mixed` declaration is stale when every command it shows is
# implemented and no reference carries a planned marker. (A `planned` page is
# already caught above, once per implemented reference.)
for path in sorted({path for path, _, _, _ in references}):
    if page_status.get(path) != "mixed":
        continue
    page_refs = [entry for entry in references if entry[0] == path]
    if all(entry[2].split()[0] in implemented for entry in page_refs) and not any(
        planned_near(path, entry[1]) for entry in page_refs
    ):
        rel = path.relative_to(root)
        failures.append(
            f"{rel}: declares Status: mixed but every command shown is implemented"
        )

for path in sorted({path for path, _, _, _ in references}):
    if path not in page_status:
        rel = path.relative_to(root)
        failures.append(
            f"{rel}: shows vowp commands but has no 'Status: current|planned|mixed'"
        )

if failures:
    for failure in failures:
        print(f"FAIL  {failure}", file=sys.stderr)
    print(
        f"FAIL  check-commands: {len(failures)} command reference problem(s)",
        file=sys.stderr,
    )
    sys.exit(1)

distinct = sorted({command for _, _, command, _ in references})
unimplemented = [
    command for _, _, command, _ in references if command.split()[0] not in implemented
]
print(
    f"PASS  check-commands: {len(references)} references, "
    f"{len(distinct)} distinct commands; {len(implemented)} implemented, "
    f"{len(unimplemented)} reference(s) still marked *(planned)*"
)
print("      " + ", ".join(distinct))
PY
