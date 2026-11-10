# Roadmap

Status: current for the command references below; this page is a planning note for contributors.
It records what the current tree ships, what is deliberately **not** in v0.1, and the design order
for v0.2 and v0.3. It promises no dates. The authoritative proof for every v0.1 claim is the linked
document or the code itself.

The first public release is **v0.1.0**. The tree in this repository identifies itself as `0.1.0`; the
release process owns the tag and the published artifacts. This page does not duplicate those instructions.

The product rationale is [`docs/design/plan.md`](design/plan.md); the v0.1 implementation spec is
[`docs/design/README.md`](design/README.md); the current behaviour is the code under `crates/`.

## v0.1.0 — what ships

| Capability | What ships in one line | Proof |
|---|---|---|
| One binary, one CLI | `vowp` exposes 24 visible commands plus the hidden `__window` used by tmux. | [`docs/reference/cli.md`](reference/cli.md), [`docs/design/cli.md`](design/cli.md) |
| Job graph | Jobs, `needs`/`verifies`/`fixes`/`escalates`/`blocks` edges, the job state machine, ownership globs and dispatch planning. | [`docs/concepts/graph-and-loops.md`](concepts/graph-and-loops.md), [`docs/design/loops.md`](design/loops.md) |
| Supervisor loops | L1 tick, L2 worker continues, L3 independent verify, L4 planner, L5 keeper, L6 failover, L7 inbox, L8 watchdog and L9 schedules. | [`docs/concepts/graph-and-loops.md`](concepts/graph-and-loops.md#the-loops), [`docs/design/loops.md`](design/loops.md) |
| Gates and evidence | Report, command, ownership, pushed, review, script, deploy and secret-scan gates with recorded evidence. | [`docs/guides/jobs.md`](guides/jobs.md), [`docs/design/loops.md`](design/loops.md) |
| Universal sessions | Append-only JSONL event log, capsules, executor switches, `session tail`, and `vowp-bundle-v1` export/import. | [`docs/concepts/sessions-and-switching.md`](concepts/sessions-and-switching.md), [`docs/design/sessions.md`](design/sessions.md) |
| Harness adapters | Claude Code (`claude -p` stream-json), opencode (`opencode run --format json`) and a generic CLI mapping adapter. | [`docs/design/adapters.md`](design/adapters.md), [`docs/concepts/sessions-and-switching.md`](concepts/sessions-and-switching.md) |
| Providers and failover | Profile ladders, quota/credit/auth/rate/context classification, probe-back, and a cost ledger that never treats unknown as free. | [`docs/concepts/providers-and-failover.md`](concepts/providers-and-failover.md) |
| Work → Production | Guards, serving snapshot, candidate/serving diff, human approval by default, deploy, probe and recorded rollback. | [`docs/concepts/work-to-production.md`](concepts/work-to-production.md) |
| Multiple nodes | Node registration, `--node` over SSH, and cross-node session export/import/switch; no shared database. | [`docs/guides/multi-node.md`](guides/multi-node.md) |
| Schedules | Cron and interval schedules, missed-run policy, pause/resume and a command/job/loop action. | [`docs/guides/schedules.md`](guides/schedules.md) |
| Install and operations | Build from source, per-user `install.sh`, systemd user unit, cron watchdog, backup and reindex. | [`docs/guides/install.md`](guides/install.md), [`docs/guides/operating.md`](guides/operating.md), [`scripts/README.md`](../scripts/README.md) |
| Security model | No stored credentials, stock binaries only, worktree and cwd confinement, and a secret scan before every push. | [`docs/security.md`](security.md) |
| Runnable example | `examples/hello-vowp` runs a three-job graph against fake adapters without a model account. | [`examples/hello-vowp/README.md`](../examples/hello-vowp/README.md) |
| Verification | The Cargo test suite and the documentation link and command checks. | [`CHANGELOG.md`](../CHANGELOG.md), [`docs/tests/README.md`](tests/README.md) |

The current tree builds and installs from source; the [install guide](guides/install.md) has the exact
steps. Prebuilt binaries, crates.io packages and the npm wrapper are release artifacts prepared by the
release process, not features implemented by this roadmap.

## What is deliberately not in v0.1

- **No HTTP or SSE API.** The supervisor opens no listening socket. Commands use the same local
  SQLite database or run over SSH; there is no network service.
- **No web or PWA window.** The board, session view, capsule view and executor switcher are terminal
  or CLI features in v0.1.
- **No ACP adapter.** Claude Code and opencode are driven through their documented headless modes.
- **No cross-node live sync.** Sessions move as exported bundles or through a cross-node switch; the
  database and session logs are not replicated.
- **No teams, quotas, VPS metrics, parallel fan-out policies or hosted relay.** One operator and one
  home node per project is the supported v0.1 shape.
- **No IANA time zones.** Schedules and limit resets use UTC or a fixed offset such as `+02:00`.
- **No phone notifications, shell completions, native GUI or Windows support.** The supported
  platforms are Linux and macOS.
- **No website.** GitHub, the README and `docs/` are the home.

The detailed list of known limitations and open defects lives in
[`CHANGELOG.md`](../CHANGELOG.md#known-limitations). Known limitations include: `session watch --follow`
is unavailable, a cross-node switch skips the remote `doctor` probe, interval schedules align to the Unix
epoch instead of their creation time, `[env.*]` has no `build` key, and `vowp doctor` warns about provider
API keys even when a harness uses its own login. Those are tracked as small follow-up issues below where
they are suitable for a first contribution.

## v0.2 — the API, the window and the handoff

v0.2 adds transport and UI around the existing core. It does **not** rewrite the event log or the job
schema: `vowp schema` already exports the wire types, and the v0.1 compatibility rules are in
[`docs/design/architecture.md`](design/architecture.md#8-compatibility-and-versioning).

| Item | What it adds | Design note | First slice |
|---|---|---|---|
| Daemon HTTP + SSE API | A loopback REST API and an event stream for jobs, sessions, events, board, inbox and cost. | [`design/v0.2/http-api.md`](design/v0.2/http-api.md) | Read-only health/jobs/sessions plus a per-session event tail. |
| Web / PWA window | Nodes, sessions, a session view with an executor switcher and capsule view, and the board. | [`design/v0.2/web-window.md`](design/v0.2/web-window.md) | Static shell and read-only board page; no sessions page, SSE or writes. |
| Phone-friendly board | A mobile-first board page that installs as a PWA and shows the same snapshot as `vowp board --json`. | [`design/v0.2/phone-board.md`](design/v0.2/phone-board.md) | Responsive board cards with polling before SSE. |
| ACP adapter | One adapter for Agent Client Protocol servers, with richer events and a permission flow. | [`design/v0.2/acp-adapter.md`](design/v0.2/acp-adapter.md) | Map ACP updates to canonical events against a fake ACP server. |
| Cross-node session sync | Move a session between nodes without hand-copying bundle files. | [`design/v0.2/cross-node-sync.md`](design/v0.2/cross-node-sync.md) | `session pull` built on the existing bundle and `--node` transport. |
| Cost dashboard | Spend by day, job, role, provider and model; receipts, price-table estimates and unknown costs stay separate. | [`design/v0.2/cost-dashboard.md`](design/v0.2/cost-dashboard.md) | `/api/v1/cost` plus a table view over `vowp cost --json`. |

The order above is the recommended dependency order, not a schedule. The API and its wire contract
come first because the window, phone board and cost dashboard all consume it. ACP and cross-node sync
can proceed in parallel once their small protocol questions are settled.

## v0.3 — teams, scale and reach

These items need identity, policy or infrastructure that v0.2 does not provide. They are **not**
started in v0.1 or v0.2.

- **Multi-user and quotas.** Accounts, per-user projects, role-based permissions, spend quotas and an
  audit trail. Requires an identity model and API authentication beyond one loopback token.
- **VPS metrics.** CPU, memory, disk, load and process health per node, with alerts and history.
  Requires a sampler, retention policy and API endpoints.
- **Parallel fan-out policies.** Split one job into several candidates, run them under a bounded
  budget, compare evidence and pick a winner. Requires graph-level fan-out/fan-in and explicit cost
  bounds.
- **Hosted relay.** Connect nodes behind NAT without SSH. Requires a relay protocol, authentication,
  end-to-end encryption and abuse/cost controls. The default deployment remains self-hosted on the
  user's own VPS.

## Ideas / help wanted

These are useful but unscoped. They are not commitments.

- Native adapters for more harnesses (the generic CLI mapping adapter already covers many).
- Phone/push notifications through ntfy or a similar hook; v0.1 has no `[notify]` config key.
- IANA time zones for schedules and limit reset parsing.
- Shell completions for `vowp`.
- Full-text search over session events and a sidecar tail index for very large logs.
- A hash-chained or signed session log, and blob garbage collection.
- Replication or a shared database (libSQL/Postgres) for teams.
- Windows support, or a documented WSL path.
- More gates: lint, coverage, licence and dependency checks.
- Better first-run docs for each supported harness.

## Good first issues

These are small, real tasks found by reading the current code. Each entry names the files to touch and
the behaviour a reviewer can check.

### 1. Emit JSON from `vowp graph --json`

**Files:** `crates/vowp-cli/src/cmd/graph.rs`, `crates/vowp-cli/tests/graph.rs`, `docs/design/cli.md`.
**Problem:** `vowp graph` ignores the global `--json` flag and always prints ASCII or Mermaid, although
the CLI contract says every command has machine-readable output.
**Do:** print `{"jobs": [...], "edges": [...]}` for the normal read path, keeping `--export` and
`--import` as they are. Add an integration test for the JSON document and update the design line.
**Accept:** `vowp graph --json` is valid JSON, contains the same jobs as `vowp job ls --json`, and
`vowp graph --mermaid` still prints only Mermaid.

### 2. Show configured gates before the first run

**Files:** `crates/vowp-cli/src/cmd/job.rs`, `crates/vowp-cli/tests/job.rs`.
**Problem:** `vowp job show --gates` prints only recorded results, so a job that has never run shows an
empty section.
**Do:** print every configured gate from `job.gates` with a `not run` marker, then replace each entry
with its recorded result when one exists.
**Accept:** a freshly added job lists `report`, `secret_scan` and any command gate as `not run`; after a
run the same command shows the recorded pass/fail and evidence.

### 3. Do not require API keys for harness-owned logins

**Files:** `crates/vowp-cli/src/cmd/doctor.rs`, `crates/vowp-cli/tests/doctor.rs`.
**Problem:** `check_providers` collects every provider in every profile and warns when
`<PROVIDER>_API_KEY` is absent. A Claude Code login does not need `CLAUDE_API_KEY`, so a healthy setup
gets a misleading warning.
**Do:** classify a provider as harness-owned when every rung that uses it is `claude-code` (and, if
appropriate, opencode's own login); report that as `ok` with a detail such as `claude-code uses its own
login`. Still warn for a provider that a key-driven harness needs.
**Accept:** a project whose only ladder is `claude-code:...` and whose environment has no
`CLAUDE_API_KEY` gets an `ok` providers check; a project with an opencode rung still warns about the
missing key.

### 4. Make `session watch` incremental and add `--follow`

**Files:** `crates/vowp-cli/src/cmd/session.rs`, `crates/vowp-cli/tests/session.rs`,
`docs/design/sessions.md`.
**Problem:** the importer stores a processed **line count**, re-reads the whole transcript every time,
and `--follow` is rejected. A transcript that grows while VOWP watches is re-read from the start, and
a line inserted anywhere throws the count off.
**Do:** store the byte offset and the last complete line, read only the new suffix, and poll every two
seconds for `--follow` until interrupted. Keep `--once` behaviour and never write to Claude's files.
**Accept:** append lines between two `--once` runs and only the new events are appended; `--follow`
imports a growing fixture without duplicates and exits cleanly on Ctrl-C; a partial final line is not
imported until it ends with a newline.

### 5. Anchor interval schedules to their creation time

**Files:** `crates/vowp-core/src/schedules/spec.rs`, `crates/vowp-core/src/schedules/engine.rs`,
`crates/vowp-core/tests/schedules_spec.rs`, `crates/vowp-core/tests/schedules_engine.rs`.
**Problem:** `ScheduleSpec::parse` has no anchor, so `every 30m` is aligned to the Unix epoch. The
design says the anchor is the schedule's `created_at`.
**Do:** carry an anchor into `next_after` and pass `schedule.created_at` from `sync_from_config`,
`catch_up` and `tick`; keep cron behaviour unchanged.
**Accept:** a schedule created at 10:07 with `every 30m` next runs at 10:37, and a restart before that
time does not move it.

### 6. Add state colours to `vowp graph --mermaid`

**Files:** `crates/vowp-graph/src/render.rs`, `crates/vowp-graph/tests/render.rs`,
`docs/design/crates/vowp-graph.md`.
**Problem:** the Mermaid renderer emits plain nodes. State colours are marked v0.2 in the design and
are a self-contained first step.
**Do:** emit one `classDef` per state group (running, waiting, failed/parked, done) and assign each
node a class. Keep the output deterministic and escape labels as today.
**Accept:** the golden Mermaid output includes the class definitions and assignments in a stable order;
no colours appear in the ASCII renderer.

### 7. Add an `[env.<name>].build` command

**Files:** `crates/vowp-protocol/src/config.rs`, `crates/vowp-core/src/promote/guards.rs`,
`crates/vowp-protocol/tests/config.rs`, `crates/vowp-core/tests/promote_guards.rs`,
`docs/design/architecture.md`.
**Problem:** the promotion `build` guard falls back to `[merge].check_cmds` or
`[project].check_cmds`; an environment cannot name its own build command, and `build = [...]` is
rejected as an unknown key.
**Do:** add an optional `build: Vec<String>` to `EnvConfig`; run it in the checkout when set, otherwise
keep the current fallback. Document the precedence.
**Accept:** a config with `[env.prod].build = ["make build"]` parses and the guard runs that command;
without it the existing fallback and its recorded note are unchanged.

### 8. Copy a job's brief into its worktree

**Files:** `crates/vowp-core/src/loops/dispatch.rs`, `crates/vowp-core/tests/loops_supervisor.rs`.
**Problem:** `write_brief` writes the generic fallback whenever the worktree copy is missing. A brief
created by `vowp job add --brief-file` in the project checkout is not copied into the job worktree, so
the worker reads the fallback.
**Do:** when the worktree brief is absent, read the project-checkout copy at `<repo>/<job.brief>` and
write that; use the generic fallback only when neither exists. Never overwrite an existing brief.
**Accept:** a job added with a custom brief and then dispatched has that exact brief in its worktree;
a job with no source brief still gets the generated fallback.

### 9. Probe a remote node before a cross-node switch

**Files:** `crates/vowp-core/src/session/switch.rs`, `crates/vowp-core/src/deps.rs`,
`crates/vowp-core/tests/switch.rs`.
**Problem:** the switch code has a documented `TODO`: a target on another node is attached after only
the local adapter registry is probed. The design calls for `vowp --node <n> doctor --json` first.
**Do:** when `to_node` differs from this node, run the remote doctor check through the injected command
runner using the node's SSH target; abort the switch with `AdapterUnavailable` when it is not ok. Keep
local switches unchanged.
**Accept:** a fake SSH that reports a failed doctor prevents the attach and leaves the session on the
source; a healthy fake SSH allows the switch; no test touches the network.

### 10. Let `vowp schema` work without an initialised home

**Files:** `crates/vowp-cli/src/lib.rs`, `crates/vowp-cli/src/cmd/maint.rs`,
`crates/vowp-cli/tests/maint.rs`.
**Problem:** `vowp schema` reads only static protocol types, but `Ctx::build` rejects a fresh
`--home` before the command runs. A contributor cannot inspect the schemas until `vowp init`.
**Do:** run `schema` before the context guard (like `doctor`) and pass it only the global `--json`
flag; keep the existing output shape and exit codes.
**Accept:** `vowp --home <fresh-temp-dir> schema` exits 0 and lists the type names; `vowp schema Job`
prints the same document as before; commands that need a project still fail with the `vowp init` hint.

## How to help

Read [`CONTRIBUTING.md`](../CONTRIBUTING.md) first. Every command in
the docs must run, tests must be hermetic, and a change should be one small commit. The v0.2 notes are
the best place to start a design discussion before writing code.
