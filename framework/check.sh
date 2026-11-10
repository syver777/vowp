#!/usr/bin/env bash
# Validate the VOWP framework templates. Reports every problem it finds, then
# exits non-zero if any check failed. Never calls a model.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
failed=0

say_pass() { printf 'PASS  %s\n' "$*"; }
say_fail() { printf 'FAIL  %s\n' "$*" >&2; failed=1; }

if ! command -v python3 >/dev/null 2>&1; then
  printf 'FAIL  python3 is required\n' >&2
  exit 1
fi
if ! python3 -c 'import yaml' >/dev/null 2>&1; then
  printf 'FAIL  PyYAML is required (python3 -m pip install PyYAML)\n' >&2
  exit 1
fi

# Sections 1-3: YAML front-matter, CLAUDE.md, and the JSONC config.
if ! python3 - "$here" <<'PY'
import fnmatch
import json
import re
import sys
from pathlib import Path

import yaml

base = Path(sys.argv[1])
errors = []


def fail(message):
    errors.append(message)


def read_front_matter(path):
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        fail(f"{path}: missing opening YAML front-matter marker '---'")
        return None, text
    try:
        end = lines.index("---", 1)
    except ValueError:
        fail(f"{path}: missing closing YAML front-matter marker '---'")
        return None, text
    raw = "\n".join(lines[1:end])
    try:
        data = yaml.safe_load(raw)
    except yaml.YAMLError as exc:
        fail(f"{path}: front-matter is not valid YAML: {exc}")
        return None, text
    if not isinstance(data, dict):
        fail(f"{path}: front-matter must be a YAML mapping")
        return None, text
    body = "\n".join(lines[end + 1 :]).strip()
    return data, body


def require_keys(path, data, keys):
    for key in keys:
        if key not in data or data[key] is None:
            fail(f"{path}: front-matter is missing required key '{key}'")


def require_nonempty_string(path, data, key):
    value = data.get(key)
    if not isinstance(value, str) or not value.strip():
        fail(f"{path}: front-matter key '{key}' must be a non-empty string")


def require_list(path, data, key, nonempty=False):
    value = data.get(key)
    if not isinstance(value, list):
        fail(f"{path}: front-matter key '{key}' must be a YAML list")
        return
    if nonempty and not value:
        fail(f"{path}: front-matter key '{key}' must not be empty")


# --- Expected files --------------------------------------------------------

expected_files = [
    "README.md",
    "templates/AGENTS.md",
    "templates/CLAUDE.md",
    "templates/brief.md",
    "templates/report.md",
    "opencode/opencode.jsonc",
]
for relative in expected_files:
    if not (base / relative).is_file():
        fail(f"{base / relative}: missing")

agents_path = base / "templates" / "AGENTS.md"
if agents_path.is_file():
    agents_text = agents_path.read_text(encoding="utf-8")
    if not agents_text.strip():
        fail(f"{agents_path}: file is empty")
    lowered = agents_text.lower()
    for phrase in (
        "work autonomously until your report exists",
        "never push",
        "questions_for_owner",
        "status: blocked",
        "hermetic",
        "secret",
    ):
        if phrase not in lowered:
            fail(f"{agents_path}: missing required rule text: {phrase!r}")

readme_path = base / "README.md"
if readme_path.is_file():
    readme_text = readme_path.read_text(encoding="utf-8")
    for url in (
        "https://opencode.ai/docs/skills/",
        "https://code.claude.com/docs/en/skills",
    ):
        if url not in readme_text:
            fail(f"{readme_path}: missing source URL {url}")

# --- Templates -------------------------------------------------------------

brief_path = base / "templates" / "brief.md"
if not brief_path.is_file():
    brief = None
    brief_body = ""
else:
    brief, brief_body = read_front_matter(brief_path)
if brief is not None:
    require_keys(
        brief_path,
        brief,
        ["job", "owns", "must_not_touch", "needs_done", "gates", "done_when"],
    )
    require_nonempty_string(brief_path, brief, "job")
    require_list(brief_path, brief, "owns", nonempty=True)
    require_list(brief_path, brief, "must_not_touch", nonempty=True)
    require_list(brief_path, brief, "needs_done")
    require_list(brief_path, brief, "gates", nonempty=True)
    require_nonempty_string(brief_path, brief, "done_when")
    for heading in (
        "# Goal",
        "# Read first",
        "# Deliverables",
        "# Tests required",
        "# Out of scope",
        "# Done when",
    ):
        if heading not in brief_body:
            fail(f"{brief_path}: missing section {heading!r}")

report_path = base / "templates" / "report.md"
if not report_path.is_file():
    report = None
else:
    report, _ = read_front_matter(report_path)
if report is not None:
    require_keys(
        report_path,
        report,
        [
            "job",
            "status",
            "branch",
            "commits",
            "tests",
            "files_changed",
            "open_items",
            "questions_for_owner",
        ],
    )
    require_nonempty_string(report_path, report, "job")
    require_nonempty_string(report_path, report, "branch")
    status = report.get("status")
    if status not in {"done", "partial", "blocked"}:
        fail(f"{report_path}: front-matter 'status' must be done, partial or blocked")
    require_list(report_path, report, "commits")
    require_list(report_path, report, "files_changed")
    require_list(report_path, report, "open_items")
    require_list(report_path, report, "questions_for_owner")
    tests = report.get("tests")
    if not isinstance(tests, dict):
        fail(f"{report_path}: front-matter 'tests' must be a mapping")
    else:
        for key in ("command", "passed", "failed"):
            if key not in tests:
                fail(f"{report_path}: front-matter 'tests' is missing '{key}'")

# --- Skills ----------------------------------------------------------------

expected_skills = {"vowp-worker", "vowp-planner", "vowp-verifier", "vowp-keeper"}
skills_dir = base / "skills"
found_skills = set()
skill_files = sorted(skills_dir.glob("*/SKILL.md"))
for path in skill_files:
    found_skills.add(path.parent.name)
    data, body = read_front_matter(path)
    if data is None:
        continue
    require_keys(path, data, ["name", "description"])
    name = data.get("name")
    description = data.get("description")
    if not isinstance(name, str) or not name:
        fail(f"{path}: front-matter 'name' must be a non-empty string")
    else:
        if name != path.parent.name:
            fail(
                f"{path}: skill name '{name}' must equal its directory "
                f"'{path.parent.name}'"
            )
        if not re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)*", name):
            fail(f"{path}: skill name '{name}' must be lowercase kebab-case")
    if not isinstance(description, str) or not description.strip():
        fail(f"{path}: front-matter 'description' must be a non-empty string")
    if not body:
        fail(f"{path}: skill has no instructions after the front-matter")

missing = expected_skills - found_skills
if missing:
    fail(f"missing expected skill directories: {', '.join(sorted(missing))}")
if not skill_files:
    fail(f"{skills_dir}: no skills/*/SKILL.md files found")

# --- CLAUDE.md -------------------------------------------------------------

claude_path = base / "templates" / "CLAUDE.md"
if not claude_path.is_file():
    fail(f"{claude_path}: missing")
else:
    claude = claude_path.read_text(encoding="utf-8")
    if claude not in {"@AGENTS.md", "@AGENTS.md\n"}:
        fail(f"{claude_path}: must contain exactly the line '@AGENTS.md'")

# --- opencode.jsonc --------------------------------------------------------

def strip_jsonc(text):
    """Remove // and /* */ comments and trailing commas, respecting strings."""
    out = []
    i = 0
    in_string = False
    escaped = False
    while i < len(text):
        char = text[i]
        if in_string:
            out.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            i += 1
            continue
        if char == '"':
            in_string = True
            out.append(char)
            i += 1
            continue
        if char == "/" and i + 1 < len(text) and text[i + 1] == "/":
            i += 2
            while i < len(text) and text[i] not in "\r\n":
                i += 1
            continue
        if char == "/" and i + 1 < len(text) and text[i + 1] == "*":
            end = text.find("*/", i + 2)
            if end == -1:
                raise ValueError("unterminated block comment")
            i = end + 2
            continue
        out.append(char)
        i += 1
    stripped = "".join(out)
    # Remove commas before a closing brace or bracket, repeatedly for nested
    # structures such as [1, 2, ].
    while True:
        newer = re.sub(r",(\s*[}\]])", r"\1", stripped)
        if newer == stripped:
            break
        stripped = newer
    return stripped


config_path = base / "opencode" / "opencode.jsonc"
required_agents = {"planner", "keeper", "worker", "verifier"}
agent_markers = {
    "planner": "Owns the job graph",
    "keeper": "Diagnoses stalled",
    "worker": "Executes exactly one",
    "verifier": "Independently verifies",
}
config = None
if not config_path.is_file():
    fail(f"{config_path}: missing")
else:
    try:
        config = json.loads(strip_jsonc(config_path.read_text(encoding="utf-8")))
    except (ValueError, json.JSONDecodeError) as exc:
        fail(f"{config_path}: not valid JSONC: {exc}")
    if config is not None:
        if not isinstance(config, dict):
            fail(f"{config_path}: top level must be a JSON object")
        else:
            if config.get("$schema") != "https://opencode.ai/config.json":
                fail(
                    f"{config_path}: $schema must be "
                    "'https://opencode.ai/config.json'"
                )
            agents = config.get("agents")
            if not isinstance(agents, dict):
                fail(f"{config_path}: 'agents' must be an object")
            else:
                for name in sorted(required_agents):
                    agent = agents.get(name)
                    if not isinstance(agent, dict):
                        fail(f"{config_path}: missing agent '{name}'")
                        continue
                    description = agent.get("description")
                    if not isinstance(description, str) or not description.strip():
                        fail(f"{config_path}: agent '{name}' needs a description")
                    elif agent_markers[name] not in description:
                        fail(
                            f"{config_path}: agent '{name}' description does not "
                            "look like the VOWP preset"
                        )
                    system = agent.get("system")
                    if not isinstance(system, str) or f"vowp-{name}" not in system:
                        fail(
                            f"{config_path}: agent '{name}' prompt must refer to "
                            f"the 'vowp-{name}' skill"
                        )
                    permissions = agent.get("permissions")
                    if not isinstance(permissions, list):
                        fail(f"{config_path}: agent '{name}' needs permissions")
                        continue
                    for rule in permissions:
                        if not isinstance(rule, dict):
                            fail(
                                f"{config_path}: agent '{name}' has a non-object "
                                "permission rule"
                            )
                            continue
                        if rule.get("effect") == "ask":
                            fail(
                                f"{config_path}: agent '{name}' has an 'ask' rule; "
                                "headless runs must only allow or deny"
                            )
                    if not any(
                        isinstance(rule, dict)
                        and rule.get("effect") == "deny"
                        and rule.get("action") == "shell"
                        and fnmatch.fnmatchcase(
                            str(rule.get("resource", "")), "git push*"
                        )
                        for rule in permissions
                    ):
                        fail(
                            f"{config_path}: agent '{name}' must deny 'git push*'"
                        )
                    if not any(
                        isinstance(rule, dict)
                        and rule.get("effect") == "deny"
                        and rule.get("action") == "shell"
                        and fnmatch.fnmatchcase(str(rule.get("resource", "")), "sudo*")
                        for rule in permissions
                    ):
                        fail(f"{config_path}: agent '{name}' must deny 'sudo*'")

                verifier = agents.get("verifier")
                if isinstance(verifier, dict):
                    vperms = verifier.get("permissions")
                    if isinstance(vperms, list) and not any(
                        isinstance(rule, dict)
                        and rule.get("effect") == "deny"
                        and rule.get("action") == "edit"
                        and rule.get("resource") == "*"
                        for rule in vperms
                    ):
                        fail(
                            f"{config_path}: verifier must deny editing all files"
                        )
                    if isinstance(vperms, list) and not any(
                        isinstance(rule, dict)
                        and rule.get("effect") == "deny"
                        and rule.get("action") == "shell"
                        and fnmatch.fnmatchcase(
                            str(rule.get("resource", "")), "git commit*"
                        )
                        for rule in vperms
                    ):
                        fail(
                            f"{config_path}: verifier must deny 'git commit*'"
                        )

            # No configured rule anywhere may resolve to 'ask' in these presets.
            def walk(node):
                if isinstance(node, dict):
                    if node.get("effect") == "ask":
                        fail(f"{config_path}: an 'ask' permission rule remains")
                    for value in node.values():
                        walk(value)
                elif isinstance(node, list):
                    for value in node:
                        walk(value)

            walk(config)

if errors:
    for error in errors:
        print(f"FAIL  {error}", file=sys.stderr)
    sys.exit(1)

print("PASS  template front-matter: brief.md, report.md")
print(f"PASS  skills: {len(skill_files)} SKILL.md files, names match directories")
print("PASS  templates/CLAUDE.md is exactly '@AGENTS.md'")
print("PASS  opencode/opencode.jsonc is valid JSONC with all four agents")
PY
then
  failed=1
fi

# Section 4: load the config in a fresh temp project with the real opencode CLI.
if ! command -v opencode >/dev/null 2>&1; then
  printf 'SKIP  opencode is not on PATH; agent-loading check skipped\n'
else
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/vowp-framework-check.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  cp "$here/opencode/opencode.jsonc" "$tmp/opencode.jsonc"

  # `timeout` is not present on stock macOS; use it only when available.
  run_debug_agents() { opencode debug agents; }
  if command -v timeout >/dev/null 2>&1; then
    run_debug_agents() { timeout 120 opencode debug agents; }
  fi

  loaded=0
  last_err=""
  for attempt in 1 2 3 4 5; do
    out="$tmp/agents-$attempt.json"
    err="$tmp/agents-$attempt.err"
    parse_err="$tmp/agents-$attempt.parse.err"
    if (cd "$tmp" && run_debug_agents >"$out" 2>"$err"); then
      if python3 - "$out" 2>"$parse_err" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        agents = json.load(handle)
except (OSError, ValueError) as exc:
    print(f"cannot parse opencode debug agents output: {exc}", file=sys.stderr)
    sys.exit(1)

if not isinstance(agents, list):
    print("opencode debug agents did not return a JSON list", file=sys.stderr)
    sys.exit(1)

by_id = {agent.get("id"): agent for agent in agents if isinstance(agent, dict)}
required = ["planner", "keeper", "worker", "verifier"]
missing = [name for name in required if name not in by_id]
if missing:
    print(f"missing agents: {', '.join(missing)}", file=sys.stderr)
    sys.exit(1)

markers = {
    "planner": "Owns the job graph",
    "keeper": "Diagnoses stalled",
    "worker": "Executes exactly one",
    "verifier": "Independently verifies",
}
for name, marker in markers.items():
    description = by_id[name].get("description", "")
    if marker not in description:
        print(
            f"agent '{name}' was loaded from another config (description mismatch)",
            file=sys.stderr,
        )
        sys.exit(1)
PY
      then
        loaded=1
        break
      fi
      last_err="$parse_err"
    else
      last_err="$err"
    fi
    sleep 1
  done

  if [ "$loaded" -eq 1 ]; then
    say_pass "opencode debug agents lists planner, keeper, worker, verifier"
  else
    say_fail "opencode did not load all four agents from opencode.jsonc"
    if [ -n "$last_err" ] && [ -s "$last_err" ]; then
      sed 's/^/      /' "$last_err" >&2
    fi
  fi
fi

if [ "$failed" -ne 0 ]; then
  printf 'RESULT  FAIL\n' >&2
  exit 1
fi
printf 'RESULT  PASS\n'
