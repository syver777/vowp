# Roadmap

Status: current for the v0.2 feature set; a planning note for contributors.

This page records what the current tree ships, what is deliberately **not** in v0.2, and the design
order for v0.3 and later. It promises no dates. The authoritative proof for every claim is the
linked document or the code itself.

The v0.1 public release is **v0.1.0**. The v0.2 feature set is merged on `main`; this tree identifies
itself as `0.2.0`, and the release job owns the tag, published artifacts and release notes. This page
does not duplicate those instructions.

The product rationale is [`docs/design/plan.md`](design/plan.md); the v0.1 implementation spec is
[`docs/design/README.md`](design/README.md); the normative v0.2 specification is
[`docs/design/v0.2/home-and-surfaces.md`](design/v0.2/home-and-surfaces.md); the current behaviour is
the code under `crates/`.

## v0.2.0 — one home, many surfaces

| Capability | What ships in one line | Proof |
|---|---|---|
| One home | A global DB plus one DB per project, one node supervisor for every active project, project registry, migration from v0.1 with row-count checks and rollback. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md), [`docs/design/v0.2/home-and-surfaces.md`](design/v0.2/home-and-surfaces.md) |
| Fleet read model | `snapshot.json` every tick, `vowp fleet status\|queue\|inbox\|capacity`, `vowp schema snapshot`, a board that shows every project. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#the-read-model) |
| Measured capacity | Host sampler, per-run measurement, p50/p90 estimates, admission with waiting reasons, brakes that never kill, owner targets and override, AI advice through the inbox. | [`docs/concepts/capacity.md`](concepts/capacity.md) |
| Sessions on surfaces | One append-only log on the home; leases with heartbeat and takeover; read-only follow mode; `vowp chat`. | [`docs/concepts/sessions-and-switching.md`](concepts/sessions-and-switching.md), [`docs/design/sessions.md`](design/sessions.md) |
| Remote execution | The harness can run on the laptop while the session stays on the home; durable spool, idempotent ingest, replay after an SSH drop or a killed client. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#remote-execution) |
| App surfaces | opencode server on the home joined through an SSH tunnel and `opencode pair`; Claude Code Remote Control where a claude.ai login exists; transcript mirror at create/resume/startup. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#app-surfaces-and-orchestrators) |
| Orchestrators | The `orchestrator` session role, project-local app config, join instructions for desktop/phone/chat, automatic resume after a restart. | [`docs/guides/sessions.md`](guides/sessions.md) |
| MCP | `vowp mcp`: typed fleet, inbox, job, capacity, tunnel, secret and session tools; every write checks the lease and records the actor. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#mcp-tools) |
| Control machine | `vowp control init`, routing to the home, snapshot cache with an `as of` time, managed SSH tunnels, phone over SSH. | [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#the-control-machine) |
| Secrets | Basic and vault-user modes, hidden prompt and one-time paste page, injection by name, exact-value redaction, never in a backup. | [`docs/security.md`](security.md), [`docs/concepts/home-and-surfaces.md`](concepts/home-and-surfaces.md#secrets-briefly) |
| Lifecycle and backup | Classes, `vowp gc` dry run and `--apply`, closed-session compaction, run-output rotation, 7 daily + 4 weekly backups, offsite and `age`, restore drill. | [`docs/concepts/data-lifecycle.md`](concepts/data-lifecycle.md) |
| AI-first setup | `vowp setup plan\|home\|control\|verify`, one printed plan, no-sudo path, stops at every human hand-off. | [`docs/setup/agent-setup.md`](setup/agent-setup.md) |
| Reference | Regenerated CLI and JSON Schema pages for the v0.1 and v0.2 command surface. | [`docs/reference/cli.md`](reference/cli.md), [`docs/reference/schema/README.md`](reference/schema/README.md) |
| Verification | The Cargo test suite and the documentation link and command checks. | [`CHANGELOG.md`](../CHANGELOG.md), [`docs/tests/README.md`](tests/README.md) |

The home is the only writer. A control machine, app or MCP server sends commands or streams events;
it never opens a database over the network. The only listeners are loopback-only: the opencode app
server joined through an SSH tunnel and the one-time secret paste page.

## What is deliberately not in v0.2

- **One home.** There is no replication, leader election or multi-home scheduler. A second home is a
  separate installation.
- **No public HTTP or SSE API.** The planned daemon API, web window and phone browser board are
  design notes for v0.3; v0.2 has no listening public port.
- **No ACP adapter or cost dashboard.** Both remain deferred design notes under `design/v0.2/`.
- **No cross-node live sync.** The session stays on the home; only the executor moves. Export/import
  bundles remain the explicit way to copy a session.
- **No multi-user accounts, quotas, roles or audit.** One operator per home is the supported shape.
- **No VPS metrics history, parallel fan-out policies or hosted relay.** These need identity,
  policy or infrastructure that v0.2 does not provide.
- **No IANA time zones.** Schedules and limit resets use UTC or a fixed offset such as `+02:00`.
- **No phone push notifications, shell completions, native GUI or Windows support.** The supported
  platforms are Linux and macOS.
- **No continuous app-transcript watcher.** The mirror runs at orchestrator create/resume/startup;
  between those calls the app owns its transcript.
- **No promise that every app exposes MCP tools.** opencode 2.0.28 can load the server and report it
  `connected` without exposing its tools to the model; `vowp chat` is the tool-free fallback.
- **Secret injection only reaches harness `env_pass`.** Promotion/deploy commands run with an empty
  environment in this build; they cannot name secrets yet (good first issue 11).
- **`session watch` starts a new VOWP session on each pass with new lines.** It does not reuse the
  session already mapped to the Claude uuid (good first issue 4).

The detailed list of known v0.2 limitations lives in the concepts pages where the behaviour is
described. Follow-ups suitable for a first contribution are below.

## v0.3 — the API, the window and the reach

These items need transport, identity or infrastructure that v0.2 does not provide. They are **not**
started in v0.2.

| Item | What it adds | Design note |
|---|---|---|
| Daemon HTTP + SSE API | A loopback REST API and an event stream for jobs, sessions, events, board, inbox and cost. | [`design/v0.3/http-api.md`](design/v0.3/http-api.md) |
| Web / PWA window | Nodes, sessions, a session view with an executor switcher and capsule view, and the board. | [`design/v0.3/web-window.md`](design/v0.3/web-window.md) |
| Phone-friendly board | A mobile-first board page that installs as a PWA and shows the same snapshot as `vowp fleet status --json`. | [`design/v0.3/phone-board.md`](design/v0.3/phone-board.md) |
| ACP adapter | One adapter for Agent Client Protocol servers, with richer events and a permission flow. | [`design/v0.2/acp-adapter.md`](design/v0.2/acp-adapter.md) |
| Cost dashboard | Spend by day, job, role, provider and model; receipts, price-table estimates and unknown costs stay separate. | [`design/v0.2/cost-dashboard.md`](design/v0.2/cost-dashboard.md) |
| Cross-node session sync | Directory-level sync or same-id continuation beyond the v0.2 remote executor. | [`design/v0.2/cross-node-sync.md`](design/v0.2/cross-node-sync.md) |
| Multi-user and quotas | Accounts, per-user projects, role-based permissions, spend quotas and an audit trail. | — |
| VPS metrics | CPU, memory, disk, load and process health per node, with alerts and history. | — |
| Parallel fan-out | Split one job into several candidates under a bounded budget, compare evidence and pick a winner. | — |
| Hosted relay | Connect nodes behind NAT without SSH, with authentication, end-to-end encryption and abuse/cost controls. | — |

The order above is the recommended dependency order, not a schedule. The API and its wire contract
come first because the window, phone board and cost dashboard all consume it.

## Ideas / help wanted

These are useful but unscoped. They are not commitments.

- Native adapters for more harnesses (the generic CLI mapping adapter already covers many).
- Phone/push notifications through ntfy or a similar hook; v0.2 has no `[notify]` config key.
- IANA time zones for schedules and limit reset parsing.
- Shell completions for `vowp`.
- Full-text search over session events and a sidecar tail index for very large logs.
- A hash-chained or signed session log, and blob garbage collection beyond the lifecycle classes.
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

### 4. Make `session watch` incremental by byte offset and add `--follow`

**Files:** `crates/vowp-cli/src/cmd/session.rs`, `crates/vowp-cli/tests/session.rs`,
`docs/design/sessions.md`.
**Problem:** the importer stores a processed **line count**, re-reads the whole transcript every time,
and `--follow` is rejected. A transcript that grows while VOWP watches is re-read from the start, and
a line inserted anywhere throws the count off. Each pass with new lines also creates a new VOWP
session for the same Claude uuid instead of reusing the existing one.
**Do:** store the byte offset and the last complete line, read only the new suffix, poll every two
seconds for `--follow` until interrupted, and find or create one canonical session by native uuid.
Keep `--once` behaviour and never write to Claude's files.
**Accept:** two `--once` runs append to one session and never create a duplicate session for the same
uuid; `--follow` imports a growing fixture without duplicates and exits cleanly on Ctrl-C; a partial
final line is not imported until it ends with a newline.

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

### 9. Let `vowp schema` work without an initialised home

**Files:** `crates/vowp-cli/src/lib.rs`, `crates/vowp-cli/src/cmd/maint.rs`,
`crates/vowp-cli/tests/maint.rs`.
**Problem:** `vowp schema` reads only static protocol types, but `Ctx::build` rejects a fresh
`--home` before the command runs. A contributor cannot inspect the schemas until `vowp init`.
**Do:** run `schema` before the context guard (like `doctor`) and pass it only the global `--json`
flag; keep the existing output shape and exit codes.
**Accept:** `vowp --home <fresh-temp-dir> schema` exits 0 and lists the type names; `vowp schema Job`
prints the same document as before; commands that need a project still fail with the `vowp init` hint.

### 10. Redact secrets from gate evidence and ledger details

**Files:** `crates/vowp-core/src/gates/cmd.rs`, `crates/vowp-core/src/ledger/mod.rs`,
`crates/vowp-core/tests/gates_cmd.rs`.
**Problem:** v0.2 redacts exact injected values from events, capsules, report artifacts, raw run
mirrors and the snapshot, but command-gate evidence and ledger details can still persist a value that
a command printed.
**Do:** run the same `vault::redact` pass over gate evidence and ledger detail before writing; keep the
secret-scan gate as the separate backstop.
**Accept:** a fake command that prints an injected value leaves only `[REDACTED:<NAME>]` in
`gate_results` and `ledger`; an uninjected value is unchanged.

### 11. Inject secrets into promotion/deploy commands by name

**Files:** `crates/vowp-protocol/src/config.rs`, `crates/vowp-core/src/promote/promoter.rs`,
`crates/vowp-core/src/vault/mod.rs`, `crates/vowp-protocol/tests/config.rs`,
`crates/vowp-core/tests/promote_pipeline.rs`.
**Problem:** `EnvConfig` has no names-only environment list and the promoter runs deploy commands with
`env: Vec::new()`, so a deploy cannot receive a secret that `vowp secret set` stored. Only harness
`env_pass` is resolved by `vault::env_for` today.
**Do:** add an optional names-only list to `EnvConfig` (for example `env_pass = ["DEPLOY_TOKEN"]`),
resolve it through the vault helper when the promote/rollback command starts, and keep the value out
of argv, logs, gate evidence and the release record. A missing name is a clear error, never an empty
value.
**Accept:** a deploy command sees its configured secret in the child environment; an unconfigured
value is absent; the stored value never appears in the release, ledger or logs.

### 12. Setup verify fixture under load — closed

Closed because the merged fixture uses a 300 s timeout and one test target.

## How to help

Read [`CONTRIBUTING.md`](../CONTRIBUTING.md) first. Every command in the docs must run, tests must be
hermetic, and a change should be one small commit. The v0.3 notes are the best place to start a design
discussion before writing code.
