# Roadmap

Status: current for the 0.4.0 release; a planning note for contributors.

This page records what the current tree ships, the v0.4 local-orchestrator work and the open
candidates. It promises no dates. The authoritative proof for every claim is the linked document,
file or test, or the code itself.

The v0.1 public release is **v0.1.0**, v0.2 is **v0.2.0**, v0.3 is **v0.3.0** and v0.4 is
**v0.4.0**, shipped on 2026-11-18. The v0.4 local-orchestrator design is accepted and Phases 0–4
shipped in 0.4.0. The security review checklist, the owner live Mac smoke test and the rollback
drill are recorded in [`docs/releases/v0.4-hardening.md`](releases/v0.4-hardening.md); the owner
smoke test passed on 2026-11-18. The shipped inventory is in [`CHANGELOG.md`](../CHANGELOG.md) and
the [`0.4.0` release notes](release/0.4.0.md). The slices that merged after the 0.3.0 tag are
included in 0.4.0 and recorded separately below.

The product rationale is [`docs/design/plan.md`](design/plan.md); the v0.1 implementation spec is
[`docs/design/README.md`](design/README.md); the normative v0.2 specification is
[`docs/design/v0.2/home-and-surfaces.md`](design/v0.2/home-and-surfaces.md); the current behaviour is
the code under `crates/`.

## v0.2.0 — one home, many surfaces (shipped)

The v0.2 feature set shipped in **v0.2.0**. This section is the historical record; later releases add
to it.

| Capability | What shipped in one line | Proof |
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

The v0.2 home is the only writer. A control machine, app or MCP server sends commands or streams
events; it never opens a database over the network. The only listeners are loopback-only: the
opencode app server joined through an SSH tunnel and the one-time secret paste page.

## What was deliberately not in v0.2

These were the v0.2 limits at that release. Later releases address some of them; each item says where.

- **One home.** There is no replication, leader election or multi-home scheduler. A second home is a
  separate installation.
- **No public HTTP or SSE API in v0.2.** The daemon API, web window and phone browser board shipped in
  0.3.0; see below.
- **No ACP adapter or cost dashboard in v0.2.** The first ACP adapter slice and the `/cost` page
  shipped in 0.3.0; see below.
- **No cross-node live sync.** v0.3.0 adds session-pull snapshots, and a bundle push merged after the
  0.3.0 tag; neither is live sync. Export/import bundles remain the explicit way to copy a session.
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
- **`session watch` started a new VOWP session on each pass with new lines in v0.2.** 0.3.0 keeps a
  byte cursor and reuses the canonical session for a Claude native uuid.

The detailed list of known v0.2 limitations lives in the concepts pages where the behaviour is
described. Open follow-ups are below.

## v0.3.0 — the API, the window and the reach (shipped)

The v0.3 feature set shipped in **v0.3.0** on 2026-11-12. It adds the loopback daemon API, the
web/PWA window and phone board, the first ACP adapter slice, the cost page and the session-pull path.
The shipped inventory is in [`CHANGELOG.md`](../CHANGELOG.md) and the
[`0.3.0` release notes](release/0.3.0.md); the design notes below remain the reference for each area.

| Item | What shipped in 0.3.0 | Design note |
|---|---|---|
| Daemon HTTP + SSE API | A loopback read API for health, board, jobs, sessions, events, capsules, nodes, inbox and cost, with per-session and board SSE streams and the token-gated inbox-answer and local-session-switch writes. | [`design/v0.3/http-api.md`](design/v0.3/http-api.md) |
| Web / PWA window | Embedded board, session list, session detail with a live timeline and capsule view, cost, inbox and nodes pages, plus the executor switcher and inbox answer form behind a token unlock panel. | [`design/v0.3/web-window.md`](design/v0.3/web-window.md) |
| Phone-friendly board | The same PWA shell at `/board` with a 360 px card layout, reached through the same loopback SSH tunnel. | [`design/v0.3/phone-board.md`](design/v0.3/phone-board.md) |
| ACP adapter | The `acp` harness kind and `acp:<model>` executor, streamed tool/file/plan updates, one accumulated assistant message, token-only cost and a permission request that ends the turn. Streaming chunk events and a permission UI remain design. | [`design/v0.2/acp-adapter.md`](design/v0.2/acp-adapter.md) |
| Cost dashboard | `GET /api/v1/cost` and the `/cost` page: grouping by job, day, role, provider and model, a known-USD chart and unknown rows kept separate. Budgets are not implemented. | [`design/v0.2/cost-dashboard.md`](design/v0.2/cost-dashboard.md) |
| Cross-node session pull | Child and `--same-id` bundle pulls, `--full` directory copies with manifest and checksum checks, and a divergence report instead of a merge. Push and sync are not part of 0.3.0. | [`design/v0.2/cross-node-sync.md`](design/v0.2/cross-node-sync.md) |

### Merged after the 0.3.0 tag and shipped in 0.4.0

These five slices are on `main` and in the 0.4.0 archives and release notes. They are not in the
0.3.0 archives.

- **Session push.** `vowp session push <node> <session>` exports one `vowp-bundle-v1` in memory and
  streams it to the remote importer. The default creates a child; `--same-id` keeps the origin id only
  when it is absent remotely and reports the same `session_divergence` conflict as pull, writing
  nothing on either side, when it is present. The remaining gap is a full-directory push (the `--full`
  form of `session push`) and a bidirectional `session sync`; neither exists. See
  [`design/v0.2/cross-node-sync.md`](design/v0.2/cross-node-sync.md#fifth-v03-slice-implemented).
- **Daemon job actions.** Token-gated `POST /api/v1/jobs/{id}/retry`, `/park` and `/cancel` validate
  the current state with the graph transition rules and queue one existing request; the handler never
  changes a job row. See [`design/v0.3/http-api.md`](design/v0.3/http-api.md).
- **`/jobs` page and job detail.** The daemon serves `/jobs` with a job table and token-gated retry,
  park and cancel controls, and a read-only `/jobs/{id}` detail page with the job's runs and gates.
  SSE for the list and the keeper job actions remain later work. See
  [`design/v0.3/web-window.md`](design/v0.3/web-window.md) and
  [`design/v0.3/http-api.md`](design/v0.3/http-api.md).
- **Supervisor-log redaction.** Every supervisor log message is redacted before the line is appended.
  The redactor is loaded once at supervisor start; an unavailable redactor writes a fixed marker
  instead of the message. See [`security.md`](security.md#redaction).
- **Switch and watch redaction.** A switch turn and the `session watch` importer now redact every
  appended payload and `raw` value. The remaining limits are in
  [`security.md`](security.md#redaction): exact values only, the report and run mirror stay raw during
  a turn, and a secret added after the supervisor started is picked up only at its next restart.

## v0.4 — local orchestrator

Shipped in **0.4.0** on 2026-11-18. The v0.4 design is accepted:
[`design/v0.4/local-orchestrator.md`](design/v0.4/local-orchestrator.md)
records the decision. In one sentence: the Mac runs a local surface and an outbound-only local node,
the home stays the only writer and the only home, the MVP transport is SSH stdio, results return by
the node pushing `job/<id>` with its own git credentials, the local `vowp chat` path is proved first,
and offload is opt-in with home-only secret values staying home.

The accepted phases and their state in this tree:

| Phase | Scope | Status |
|---|---|---|
| 0 | Protocol frames, node identity, enrollment, SSH gateway and presence | shipped |
| 1 | Local `vowp chat`: surface pairing, leases, turn ingest, spool replay and takeover | shipped |
| 2 | Offload executor: placement policy, path map, isolated worktree, events, result push and home verification | shipped; offload is opt-in and off by default |
| 3 | Local opencode surface: app frames, agent socket, opencode bridge, MCP proxy, orchestrator materialization and local cache | shipped |
| 4 | Hardening: WSS ingress, launchd, token rotation UX, bundle result, separate user, metrics and docs | shipped in 0.4.0; security checklist, owner live Mac smoke test and rollback drill [recorded](releases/v0.4-hardening.md) |

Phase 0 is proved by the wire types in
[`crates/vowp-protocol/src/agent.rs`](../crates/vowp-protocol/src/agent.rs), the enrollment, key
authentication and gateway in
[`crates/vowp-core/src/agent/gateway.rs`](../crates/vowp-core/src/agent/gateway.rs), the node and
enrollment records in [`crates/vowp-store/src/agent.rs`](../crates/vowp-store/src/agent.rs) and the
gateway tests in [`crates/vowp-core/tests/agent.rs`](../crates/vowp-core/tests/agent.rs).

Phase 1 is proved by the surface lease implementation in
[`crates/vowp-core/src/surface/lease.rs`](../crates/vowp-core/src/surface/lease.rs), the local chat
path in [`crates/vowp-cli/src/chat/mod.rs`](../crates/vowp-cli/src/chat/mod.rs), the pairing test
[`crates/vowp-agent/tests/surface_pair.rs`](../crates/vowp-agent/tests/surface_pair.rs) and the lease
tests in [`crates/vowp-core/tests/surface_lease.rs`](../crates/vowp-core/tests/surface_lease.rs).

Phase 2 is proved by [`crates/vowp-core/src/offload/mod.rs`](../crates/vowp-core/src/offload/mod.rs)
and [`crates/vowp-core/src/offload/policy.rs`](../crates/vowp-core/src/offload/policy.rs), the
executor and worktree in [`crates/vowp-agent/src/executor.rs`](../crates/vowp-agent/src/executor.rs)
and [`crates/vowp-agent/src/worktree.rs`](../crates/vowp-agent/src/worktree.rs), and the Phase 2 exit
test [`crates/vowp-core/tests/offload_e2e.rs`](../crates/vowp-core/tests/offload_e2e.rs). Offload is
off by default: the `[offload]` table in
[`crates/vowp-protocol/src/config.rs`](../crates/vowp-protocol/src/config.rs) defaults `enabled` to
false, every job stays on the home until the owner enables offload and a project opts in, and
[`crates/vowp-protocol/tests/config.rs`](../crates/vowp-protocol/tests/config.rs) pins that default.

Phase 3 is shipped. These slices are merged in this tree:

| Slice | What is in the tree | Proof |
|---|---|---|
| App frames | Attach/detach, native event, turn and MCP tool frames; the home assigns the sequence and deduplicates native ids. | [`crates/vowp-protocol/src/agent.rs`](../crates/vowp-protocol/src/agent.rs), [`crates/vowp-protocol/tests/surface.rs`](../crates/vowp-protocol/tests/surface.rs) |
| Agent socket | A mode-0600 `agent.sock` forwards attach/detach and tool calls over the one outbound connection and correlates results by request id. | [`crates/vowp-agent/src/socket.rs`](../crates/vowp-agent/src/socket.rs), [`crates/vowp-agent/tests/socket.rs`](../crates/vowp-agent/tests/socket.rs) |
| opencode bridge | Follows the local opencode event stream, maps completed events, backfills from a cursor, normalizes paths and redacts injected values before spooling. | [`crates/vowp-agent/src/bridge.rs`](../crates/vowp-agent/src/bridge.rs), [`crates/vowp-agent/tests/bridge.rs`](../crates/vowp-agent/tests/bridge.rs) |
| MCP proxy | A local stdio MCP server answers initialize, list and ping locally and forwards tool calls through the agent socket; home-only tools are refused. | [`crates/vowp-cli/src/mcp/proxy.rs`](../crates/vowp-cli/src/mcp/proxy.rs), [`crates/vowp-cli/tests/mcp.rs`](../crates/vowp-cli/tests/mcp.rs) |
| Home orchestrator record and command drain | The home records a node-bound orchestrator, queues typed start, resume and stop commands durably, and the live gateway drains them and applies correlated results; disconnect and reconnect update the record. | [`crates/vowp-core/src/surface/orchestrator.rs`](../crates/vowp-core/src/surface/orchestrator.rs), [`crates/vowp-core/src/agent/gateway.rs`](../crates/vowp-core/src/agent/gateway.rs), [`crates/vowp-cli/tests/orchestrator.rs`](../crates/vowp-cli/tests/orchestrator.rs) |
| Agent-side orchestrator host | The agent materializes a private per-orchestrator config, starts the app in its own process group, attaches the paired surface, and keeps the host across a home disconnect; stop signals only that host group. | [`crates/vowp-agent/src/orchestrator.rs`](../crates/vowp-agent/src/orchestrator.rs), [`crates/vowp-agent/tests/orchestrator.rs`](../crates/vowp-agent/tests/orchestrator.rs) |
| Offline read cache | A bounded private cache under `<state_dir>/cache` answers the six read-only MCP tools with an `as of` line when the home is offline or the reply times out; writes never use it and a revoked surface clears its entries. | [`crates/vowp-agent/src/cache.rs`](../crates/vowp-agent/src/cache.rs), [`crates/vowp-agent/tests/cache.rs`](../crates/vowp-agent/tests/cache.rs) |
| Job detail page | The daemon serves a read-only job detail API with runs and gates, and the `/jobs/{id}` page renders them. | [`crates/vowp-daemon/src/app.rs`](../crates/vowp-daemon/src/app.rs), [`crates/vowp-daemon/tests/job_detail.rs`](../crates/vowp-daemon/tests/job_detail.rs) |

Phase 3 is shipped in 0.4.0.

Phase 4 shipped in 0.4.0. [`design/v0.4/hardening.md`](design/v0.4/hardening.md) turns the phase
scope — optional WSS ingress behind a loopback listener, launchd/systemd-user service install,
rotation UX, bundle result, separate user, bounded metrics history and the security-review, owner
live Mac smoke test and rollback gates — into one contract per item, with the named hermetic tests
and the ordered slice list. The WSS transport and listener, the agent service, the rotation
`--wait`/`--grace` path, the bundle result, the separate-user checks and the capacity history are in
the release, and [the agent-node guide](guides/agent-node.md) is the operator path. The security
review checklist, the owner live Mac smoke test and the rollback drill on a restored copy are
recorded in [`docs/releases/v0.4-hardening.md`](releases/v0.4-hardening.md); the smoke test passed on
2026-11-18. The accepted [`design/v0.4/local-orchestrator.md`](design/v0.4/local-orchestrator.md)
remains the binding design for the phases that shipped.

## Open follow-ups

These follow-ups are drawn from the current code and the 0.3.0 known limitations. One has shipped;
the rest are candidates or design work. They are not commitments, no release is promised, and a
design note is linked only where one exists.

| Follow-up | Status | Design note or current limit |
|---|---|---|
| ACP streaming chunk events and a permission flow | design | [`design/v0.2/acp-adapter.md`](design/v0.2/acp-adapter.md), [`0.3.0` known limitations](release/0.3.0.md#acp-adapter) |
| Live updates for the cost, inbox and nodes pages (they poll every five seconds) | candidate | [`design/v0.3/web-window.md`](design/v0.3/web-window.md) |
| Node add, remove and probe writes through the API (the nodes route is read-only) | candidate | [`design/v0.3/http-api.md`](design/v0.3/http-api.md) |
| Job detail page (`/jobs/{id}`) and its runs and gates | shipped | [`design/v0.3/web-window.md`](design/v0.3/web-window.md), [`design/v0.3/http-api.md`](design/v0.3/http-api.md) |
| SSE for the jobs list and the keeper job actions (nudge, restart, replan and fail) | design | [`design/v0.3/web-window.md`](design/v0.3/web-window.md), [`design/v0.3/http-api.md`](design/v0.3/http-api.md) |
| A browser chat turn (needs a run queue and cancellation) | design | [`design/v0.3/web-window.md`](design/v0.3/web-window.md) |
| A full-directory push (the `--full` form of `session push`) and a bidirectional `session sync` | design | [`design/v0.2/cross-node-sync.md`](design/v0.2/cross-node-sync.md) |
| Multi-user accounts, quotas, roles and audit | open | — |
| VPS metrics history and alerts | history shipped; alerts open | [`design/v0.4/hardening.md`](design/v0.4/hardening.md) §7 |
| Parallel fan-out with a bounded budget | open | — |
| A hosted relay without SSH | open | — |
| Shell completions for `vowp` | candidate | — |
| IANA time zones for schedules and limit resets | candidate | — |

## How to help

Read [`CONTRIBUTING.md`](../CONTRIBUTING.md) first. Every command in the docs must run, tests must be
hermetic, and a change should be one small commit. Design discussion belongs in the design note for
the area or in an issue that links this page; the v0.4 note and the open follow-ups above are the
current starting points.
