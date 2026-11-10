# VOWP framework files

These are the harness-neutral files that `vowp init` installs into a project so that every agent — Claude
Code, opencode, or another harness — follows the same job contract. They are generic: replace the example
paths and commands with the project's own.

`vowp init` embeds these files in the `vowp` binary and writes them into the project. This directory is
the source of truth for the templates, skills, opencode agent presets and the validation script.

## What each file is

| Source | Installed as | Purpose |
|---|---|---|
| `templates/AGENTS.md` | `<repo>/AGENTS.md` | The harness-neutral rules: job contract, owned paths, autonomy, git, secrets, evidence, costs, risky actions, asking the owner. |
| `templates/CLAUDE.md` | `<repo>/CLAUDE.md` | Exactly one line, `@AGENTS.md`. Claude Code imports `AGENTS.md`, so both harnesses read one rule file. |
| `templates/brief.md` | `.vowp/templates/brief.md` | Brief template with the `job`, `owns`, `must_not_touch`, `needs_done`, `gates`, `done_when` front-matter and the Goal / Read first / Deliverables / Tests required / Out of scope / Done when sections. |
| `templates/report.md` | `.vowp/templates/report.md` | Report template with the `job`, `status`, `branch`, `commits`, `tests`, `files_changed`, `open_items`, `questions_for_owner` front-matter. |
| `skills/vowp-worker/SKILL.md` | `.claude/skills/vowp-worker/SKILL.md` | The job contract for workers. |
| `skills/vowp-planner/SKILL.md` | `.claude/skills/vowp-planner/SKILL.md` | Shaping the graph: small jobs, disjoint ownership, precise briefs, executable gates. |
| `skills/vowp-verifier/SKILL.md` | `.claude/skills/vowp-verifier/SKILL.md` | Judging evidence: real tests, owned paths, no fakes, concrete findings. |
| `skills/vowp-keeper/SKILL.md` | `.claude/skills/vowp-keeper/SKILL.md` | Diagnosing stalls and failures from logs, with bounded retry guidance and no code changes. |
| `opencode/opencode.jsonc` | `<repo>/opencode.jsonc` | opencode V2 agent definitions for `planner`, `keeper`, `worker` and `verifier`, with permission presets that never ask for a keystroke. |
| `check.sh` | not installed | Validates the templates, skills, JSONC and (when `opencode` is installed) agent loading. |

`vowp init` installs the brief/report templates under `.vowp/templates/`. When a job is added without a
custom brief, `vowp job add` writes the brief template body to `.vowp/jobs/<job-id>/brief.md`. A worker
writes its report to `.vowp/jobs/<job-id>/report.md`. The graph node stores those paths.

If the project already has an `AGENTS.md`, `CLAUDE.md` or `opencode.jsonc`, `vowp init` must merge or append,
never silently overwrite it. In particular, `CLAUDE.md` may contain an existing import; keep it and add
`@AGENTS.md` if it is missing.

## Where each harness loads skills

Checked against the current documentation and against `opencode v2.0.28`.

| Harness | Project locations it reads | Global locations | Source |
|---|---|---|---|
| Claude Code | `.claude/skills/<skill-name>/SKILL.md` (also nested under subdirectories, and `.claude/commands/*.md` for the older command format) | `~/.claude/skills/<skill-name>/SKILL.md`; plugin and enterprise locations | <https://code.claude.com/docs/en/skills> |
| opencode | `.opencode/skills/`, and compatibility `.claude/skills/`, `.agents/skills/` | `~/.config/opencode/skills/`, `~/.claude/skills/`, `~/.agents/skills/` | <https://opencode.ai/docs/skills/> |

**Install decision:** the canonical install is `.claude/skills/<name>/SKILL.md`.

- Claude Code reads project skills only from `.claude/skills/` (the docs list enterprise, personal,
  project and nested `.claude/skills/` locations; `.agents/` is not one of them).
- opencode explicitly lists `.claude/skills/` as a project compatibility source, so the same copy loads in
  opencode without duplication.
- `.agents/skills/` is read by opencode but **not** by Claude Code. Installing only there would fail the
  "both harnesses load them" requirement. If another Agent Skills client needs `.agents/skills/`, add a
  symlink to `.claude/skills` rather than committing a second copy; opencode's precedence gives the later
  source the same skill ID, and two copies can drift.

The harness documentation above is the authority: use `.claude/skills/` as the single installed
location, and keep the source under `framework/skills/` for the binary to embed.

opencode also accepts extra local directories or HTTP catalogs via the top-level `skills` array
(<https://opencode.ai/docs/skills/#sources>). That is optional and not used by `vowp init`.

## opencode agents

`opencode/opencode.jsonc` uses opencode **V2** configuration keys: `agents` (not V1's `agent`) and
`permissions` (not V1's `permission`), with `action` / `resource` / `effect` rules. The format is documented
at <https://opencode.ai/docs/agents/> and <https://opencode.ai/docs/permissions/>. Install it as
`<repo>/opencode.jsonc`, or as `<repo>/.opencode/opencode.jsonc`; opencode reads both, and a `.opencode`
config overrides a direct one in the same directory (<https://opencode.ai/docs/config/#locations>).

The four agents are `planner`, `keeper`, `worker` and `verifier`:

- **planner** may read and run commands, edit `.vowp/**` and `docs/**`, and never push.
- **keeper** may read logs and run diagnostic commands, edit only `.vowp/**`, and never commit or push.
- **worker** may edit and run any command except `git push*` and `sudo*`.
- **verifier** may run commands but cannot edit files; `git commit*` and `git push*` are denied.

All four deny the interactive `question` tool and the `subagent` tool, and secret-file reads are denied, so a
headless run has no dialog to answer. The file intentionally pins no model: the VOWP profile selects it and
passes `--model provider/model#variant` (or sets it on the session). If you want a local default, add a
`model` to one agent and document it as an example, not as a requirement.

The prompts point to the matching skill (`vowp-planner`, `vowp-keeper`, `vowp-worker`, `vowp-verifier`).

## Customising

1. **Project rules:** edit `AGENTS.md`. Keep it short and imperative. Project-specific build, test, lint and
   formatting commands belong there or in the brief, not in the skills.
2. **Do not edit `CLAUDE.md`:** it stays exactly `@AGENTS.md`. Put Claude-specific notes in `AGENTS.md` if
   they apply to everyone, or in a separate imported file if they truly are Claude-only.
3. **Brief and report:** copy `brief.md` and `report.md` into `.vowp/templates/` and adjust the example
   front-matter only if the project's gate vocabulary differs. Every brief gets real `owns` globs and real
   commands, not placeholders.
4. **Skills:** add project skills beside the `vowp-*` ones in `.claude/skills/`. Keep each `name` equal to its
   directory and give it a one-sentence `description` saying when to use it. Keep `vowp-*` names stable: the
   opencode prompts and the supervisor refer to them.
5. **Agents:** edit the permissions in `opencode.jsonc` to match the project. Keep the deny rules for
   `git push*`, `sudo*` and secret reads. Re-run `framework/check.sh` after any edit.

## Validation

```bash
bash framework/check.sh
```

The script checks:

1. YAML front-matter in every template that has it and in every `SKILL.md`; required keys; skill `name`
   equal to the directory and in lowercase kebab-case.
2. `templates/CLAUDE.md` is exactly `@AGENTS.md`.
3. `opencode/opencode.jsonc` is valid JSONC after comments and trailing commas are stripped; the four agents
   exist and the required deny rules are present.
4. If `opencode` is on `PATH`, a fresh temp project with a copy of `opencode.jsonc` is loaded with
   `opencode debug agents`; all four agents must be listed. If the CLI is absent, this step prints `SKIP`
   and the rest still runs.

`check.sh` never calls a model. It is self-contained apart from `python3` with PyYAML, `jq` is not required,
and the opencode step is read-only and uses a temporary directory.

If an installed skill does not appear in a harness, check that `vowp init` wrote
`.claude/skills/vowp-*/SKILL.md` and that the harness's own skill listing sees that directory. Claude Code
and opencode both read project skills from `.claude/skills/`.
