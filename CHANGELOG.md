# Changelog

All notable changes to VOWP are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/).

Release notes are in [`docs/release/0.4.0.md`](docs/release/0.4.0.md),
[`docs/release/0.3.0.md`](docs/release/0.3.0.md),
[`docs/release/0.2.0.md`](docs/release/0.2.0.md) and
[`docs/release/0.1.0.md`](docs/release/0.1.0.md). The product scope and the
features deliberately left out of v0.4 are in [`docs/roadmap.md`](docs/roadmap.md).

## [Unreleased]

## [0.4.0] - 2026-11-18

The v0.4 feature set: outbound Mac and Linux agent nodes, paired local `vowp chat` and opencode
orchestrator surfaces, opt-in run offload with push or verified git bundle return, and the phase-4
hardening work — optional WSS ingress, launchd/systemd-user agent service, surface token rotation,
separate-user checks, capacity history and live supervisor drain/upgrade. ASIF 0.4 session
export/import is a review draft. The upgrade path and the known limits are in
[`docs/release/0.4.0.md`](docs/release/0.4.0.md).

### Added

**ASIF 0.4 interchange (review draft)**

- `vowp session export <id> --format asif` writes one deterministic `.asif.zip` through the
  published `vowp-asif 0.1.1` adapter from crates.io. The default path is
  `./<resolved-id>.asif.zip`; `--out -` writes the exact package bytes and is refused on a
  terminal, and `--json` conflicts with it. Export is read-only: it writes no capsule, takes no
  session lock and moves no lease.
- Export redaction: repeatable `--redact` literals and `--redact-file` (read locally, one
  literal per line). Host paths are scrubbed by default; `--keep-host-paths` is opt-in.
- `vowp session import <file> --format asif` imports a `.asif.zip`, a standalone `session.json`,
  a directory holding it or stdin as one new idle session with a fresh ULID. It never
  overwrites, merges or keeps the source id; `--cwd` defaults to the project path and
  `--branch` selects one branch of a multi-branch document.
- Loss reporting and policy: human output prints the count and unique loss kinds to stderr,
  `--json` carries the summary, and `--report` writes the full `vowp-asif-report-v1`.
  `--strict` refuses `Blocking`/`Integrity` losses with exit 5 and writes nothing;
  `--allow-loss` accepts recoverable integrity/ambiguity problems as declared losses. The two
  flags conflict.
- Remote export and import over the existing SSH transport. Export verifies the hidden stream's
  byte count and SHA-256 before writing the local package; `--report` and directory input are
  refused with `--node`.
- `vowp_core::session::asif` exposes the adapter-free `AsifReport`/`AsifImportResponse` facade,
  and `vowp-protocol` exports both documents with their JSON Schemas. The adapter is a runtime
  dependency of `vowp-core` only; `vowp-cli` uses it as a dev-dependency for fixtures.
- The daemon serves token-free `GET /api/v1/sessions/{id}/export/asif` (with `?report=1` for
  the loss preview) and token-gated `POST /api/v1/sessions/import/asif?branch=<id>`. The session
  detail page gains the download card, `/import` is the upload page, and the PWA shell includes
  it.
- The cross-cutting contract is in
  [`docs/design/v0.4/asif-integration.md`](docs/design/v0.4/asif-integration.md); the session
  narrative is `sessions.md` §6.8, and `cli.md` and `http-api.md` carry the command and route
  detail.

ASIF 0.4 is a review draft. The slice reports declared losses and does not claim ASIF
conformance, an exact or lossless round trip, portable continuation or native-harness import.
The adapter is the published `vowp-asif = "=0.1.1"` crate from crates.io, recorded in
[`docs/design/v0.4/asif-integration.md`](docs/design/v0.4/asif-integration.md) §4.3; all nine
workspace crates pass `scripts/release/check-packages.sh`.

**v0.4 local orchestrator: agent nodes, local chat, offload and the local opencode surface**

- Phase 0: outbound agent nodes with Ed25519 node identity and one-time enrollment, the
  authenticated SSH-stdio gateway and presence reporting. The optional WSS transport is covered
  in the Phase-4 entry below.
- Phase 1: a local `vowp chat` surface that pairs over the agent connection, acquires and
  heartbeats the same session lease as the home, ingests turns into the single session log and
  replays its spool after a disconnect, with lease takeover and the read-only follow mode.
- Phase 2: opt-in offload with a placement policy, a Mac path map, an isolated worktree per job,
  ordered run events and home verification of the pushed `job/<id>` branch. Offload remains off
  by default.
- Phase 3: the local opencode surface — attach/detach, native-event, turn and MCP frames; the
  mode-0600 agent socket; the opencode SSE bridge; the stdio MCP proxy with its bounded offline
  read cache; and home materialization and lifecycle of a local orchestrator.
- The acceptance repair: the home gateway handles all five local-surface frames and MCP tool
  calls, `vowp agent pair` uses the live agent socket instead of superseding it, a local opencode
  host auto-wires its bridge, and unsent bridge frames are included in the pending spool count.
  The real-gateway multi-project E2E proof is
  [`crates/vowp-cli/tests/agent.rs`](crates/vowp-cli/tests/agent.rs).

The design is in [`docs/design/v0.4/local-orchestrator.md`](docs/design/v0.4/local-orchestrator.md);
the Phase 3 proof table is in [`docs/roadmap.md`](docs/roadmap.md#v04-local-orchestrator). The owner
live Mac smoke test passed on 2026-11-18; the record is in
[`docs/releases/v0.4-hardening.md`](docs/releases/v0.4-hardening.md).

**Phase-4 hardening: agent nodes, WSS, rotation, bundles and capacity history**

- `vowp agent enroll --wss <url>` (with optional `--tls-fingerprint`) stores the optional WSS
  transport. The home's `[ingress.wss]` table starts a loopback WebSocket listener behind the
  owner's TLS reverse proxy; `vowp up` starts and stops the hidden `agent gateway --listen`
  child. TLS, ACME and the public port stay outside VOWP.
- `vowp agent trust --fingerprint <sha256:…>` confirms a changed system-mode WSS leaf
  fingerprint after the owner verifies it through a trusted channel; it rewrites only the private
  `agent.json` and never connects. A `tls = "pin"` transport stays a hard pin, so a changed pinned
  certificate still requires re-enrollment with `--tls-fingerprint`.
- `vowp agent service install|uninstall|status` writes or removes exactly one 0600 launchd
  `LaunchAgent` or systemd user unit for the current user. `agent run --service` writes the
  launchd restart marker and drains the spool and leases on SIGTERM. `agent doctor` reports
  state-directory, service, surface and copied-login warnings by name only.
- Surface rotation gains `--wait` and `--grace` (default ten minutes, capped at 24 hours), a
  durable `queued_at`/`overlap_until` envelope, `PREFIX`/`OVERLAP` columns, a `surfaces` status
  array and the `needs_pair` state. `surface pair --replace` revokes the old surface and creates
  its replacement in one transaction. Rotation and revocation write one token-free ledger note.
- `[offload] result_transport = "bundle"` is legal and returns a verified `base..head` git bundle
  over the authenticated agent channel. The five bundle frames and
  `AssignmentPayload.result_transport` are exported; the home verifies size/sha256,
  `git bundle verify`, one job ref and base ancestry before the gates or any project ref. Push
  stays the default and neither transport falls back.
- Global schema v7 adds `capacity_rollups` with a v6-to-v7 migration and backup.
  `vowp capacity history [--since] [--bucket 5m|1h|1d]` reads the bounded history read-only; raw
  samples are kept 48 hours and rollups 30 days. Cost remains `vowp cost --by day`.
- The separate-user shape, service limits, WSS trust boundary, rotation storage and bundle
  verification are documented in [`docs/security.md`](docs/security.md), with the operator path
  in [`docs/guides/agent-node.md`](docs/guides/agent-node.md) and the design/status pages updated.
  The owner live Mac smoke test passed on 2026-11-18; the smoke-test record, the rollback drill and
  the security-review sign-off are recorded phase-exit evidence in
  [`docs/releases/v0.4-hardening.md`](docs/releases/v0.4-hardening.md).

### Changed

- The default fix loop is now two rounds (`max_attempts = 2`). The verifier's findings are
  copied into each fix brief. A recovery retry that finds a terminal fix child allocates a fresh
  child (`<id>-fix-<attempt>-rN`) instead of sending `NeedsSatisfied` to the finished job, which
  closes the `NeedsSatisfied in state Done` wedge.

## [0.3.0] - 2026-11-12

The v0.3 feature set: a loopback HTTP/SSE API and `vowp-daemon`, a web/PWA
window and phone board served from the home, the ACP adapter, and session pull
with same-id, full-directory and divergence reporting. This is an early
release; the interfaces may still change. The upgrade path and the known limits
are in [`docs/release/0.3.0.md`](docs/release/0.3.0.md).

### Added

**Loopback HTTP and SSE API**

- `vowp-daemon`, a library and binary that opens an existing home read-only
  for reads (writable but non-creating for the two writes), serves one project
  and binds `127.0.0.1` only. It resolves the home from `--home`, `VOWP_HOME`
  or `$HOME/.vowp` and selects the checkout project or the only registered one.
- Read endpoints: `GET /api/v1/health`, `/board`, `/board/stream`, `/jobs`,
  `/sessions`, `/sessions/{id}`, `/sessions/{id}/capsules/{capsuleId}`,
  `/sessions/{id}/events`, `/sessions/{id}/events/stream`, `/nodes`, `/inbox`
  and `/cost`. The JSON documents are the same objects the CLI prints.
- Per-session SSE with one `session` frame per stored event, a periodic
  `heartbeat`, a single `resync` frame after a compacted or truncated log, and
  bounded batches with HTTP backpressure. The board stream sends the exact
  board document when it changes plus heartbeats; it has no cursor.
- Two token-gated writes: `POST /api/v1/inbox/{id}/answer` queues the same
  `answer_inbox` request as the CLI, and `POST /api/v1/sessions/{id}/switch`
  runs the same `SessionService` switch as `vowp session switch` with the CLI
  guards and a five-second drain. Both require `Authorization: Bearer
  <VOWP_API_TOKEN>`; missing or empty disables writes with `503`, a missing or
  wrong token is `401`, and reads stay token-free. The token is compared in
  constant time and never logged, echoed or put in a URL.
- API wire documents and their JSON Schemas: `Health`, `Board`, `BoardRun`,
  `BoardLimit`, `Page`, `JobPage`, `SessionPage`, `NodePage`, `SessionDetail`,
  `EventPage`, `InboxPage`, `InboxAnswerRequest`, `SessionSwitchRequest`,
  `CostReport`, `CostRow`, `CostTotal`, `ApiError` and `ApiErrorBody`.

**Web/PWA window**

- `vowp up` starts `vowp-daemon` as a separate child process, prints
  `web window: http://127.0.0.1:<port>/` and the exact
  `vowp tunnel open window --remote-port <port>` control-machine hint, writes
  the address to `<home>/web-window` for a detached run, and stops the child on
  `vowp down`, SIGINT/SIGTERM or shutdown. A missing daemon warns and the
  supervisor keeps running.
- Embedded static pages for the board, session list, session detail with an
  event timeline, capsule view, cost, inbox and nodes, with no Node or
  TypeScript build step. The board and session timelines use SSE with
  `after_seq` resume and a five-second polling fallback; the cost, inbox and
  nodes pages poll.
- The session detail page includes the executor switcher (target, reason and
  force) and the inbox page includes the answer form. Both share a token
  unlock panel that keeps `VOWP_API_TOKEN` in `sessionStorage`, never in the
  URL, clears it on `401` and shows the disabled line on `503`.
- The cost page renders grouped ledger rows, totals and an accessible inline
  bar chart of known USD rows; unknown costs stay `?` and are never drawn as
  zero.
- PWA shell: `/manifest.webmanifest`, `/icon.svg` and `/sw.js` with the service
  worker scoped to `/`. The worker caches only the static shell; `/api/` and
  stream requests always go to the network and are never served from the cache
  as authoritative data.

**Phone board**

- `GET /board` serves the same PWA shell at a 360 px card layout: job counts
  with state labels, running-job cards with age and last-event age, slots,
  planner and keeper, active limits with resets, today's cost, the open-inbox
  badge, the last checkpoint and a visible last-updated time. A phone reaches
  it through the same loopback SSH tunnel as the window; no public port is
  opened.

**ACP adapter**

- The `acp` harness kind and `acp:<model>` executor string drive an Agent
  Client Protocol (revision 1) JSON-RPC 2.0 server over stdin/stdout. One
  prompt turn maps streamed updates to `tool.call`, `tool.result`,
  `file.change`, `plan`, one accumulated `assistant.message`, token-only
  `cost` with an unknown source, and `turn.ended`; VOWP keeps writing the
  session framing.
- A permission request is never auto-approved: it becomes a `note` plus a
  non-retryable `error`, the adapter answers `cancelled`, and the turn ends.
  There is no keystroke path and no inbox-approval round trip in this slice.
- `vowp doctor` probes `acp` only when `[harnesses.acp]` or an ACP profile
  rung is configured.

**Session pull and divergence**

- `vowp session pull <node> <session>` exports a bundle from another node over
  the existing SSH transport, verifies its format, protocol version and
  checksum in a private staging directory, and imports a child session with
  the source as its parent.
- `--same-id` keeps the origin session id when it is absent locally, with no
  parent and the same tail and capsule rules as `session import --same-id`. A
  present id is exit 4, a duplicate event id is a conflict, and neither form
  overwrites or merges.
- `--full` copies the whole `sessions/<id>/` directory (full log, blobs,
  capsules and `adapters.json`) as a `vowp-session-dir-v1` manifest plus raw
  bytes. Every size and sha256 is checked in private staging before the
  directory is renamed into place and reindexed; the cap is 1 GiB. A present
  id, corrupt snapshot, over-cap source or compacted source is refused and
  leaves no local session.
- Both id-preserving pulls report divergence instead of a bare already-exists
  error. They print the local and remote heads and exit 4, and `--json`
  returns a stable `session_divergence` conflict with `local`, `remote`,
  `node` and `source_session` fields. The pull writes nothing; the operator
  deletes or exports one side and pulls again. There is no force flag and no
  merge.
- `vowp session watch` keeps a byte cursor, imports only newly completed lines
  and maps one Claude native uuid to one canonical VOWP session.
  `session watch --follow` polls every two seconds until Ctrl-C; `--once`
  makes one incremental pass and a non-TTY run without `--follow` does not
  hang.

**Other features**

- `vowp graph --json` prints `{"jobs":[...],"edges":[...]}` for the normal read
  path. `--mermaid` and `--export`/`--import` keep their precedence and
  behaviour.
- `vowp job show --gates` lists every gate configured on the job in order and
  marks one with no recorded result as `not run`; recorded results replace only
  their own entry.
- `[env.<name>].build` names the commands the build guard runs in that
  environment's checkout; when empty, the merge and project check commands
  remain the fallback.
- Promotion and rollback commands receive the secret values named by
  `[env.<name>].env_pass` in their child environment only. A missing or
  invalid name fails the operation by name before anything is banked or
  deployed.

### Changed

- `vowp up` starts the web-window daemon and prints its address and tunnel
  hint; `up --once` starts no daemon, and a missing or ambiguous project
  leaves the window off with a warning.
- Interval schedules keep the phase of their creation time instead of
  aligning to the Unix epoch, including after a restart and in catch-up.
- `vowp session watch` reuses the canonical session for a Claude native uuid
  and imports incrementally instead of creating a session on each pass.
- The graph Mermaid renderer emits state `classDef`s for node classes.

## [0.2.0] - 2026-11-11

The v0.2 feature set: one VPS **home** that owns the truth, and many **surfaces** that talk to it.
This is an early release; the interfaces may still change. The upgrade path, acceptance evidence
and known limits are in [`docs/release/0.2.0.md`](docs/release/0.2.0.md).

### Added

**One home, one supervisor**

- A global database (`~/.vowp/vowp.db`) plus one database per project
  (`~/.vowp/projects/<name>/project.db`), with a `Store` handle bound to the global DB and exactly
  one project DB. Ordered cross-file writes, startup reconcile and a crash-injection test.
- `vowp migrate` splits a v0.1 home into the new layout with per-table row-count checks and a
  pre-migration backup; `vowp up` migrates automatically with `--no-migrate` as the escape hatch.
- One node supervisor for every active project, with fairness weights, round-robin L1–L9 steps,
  per-project isolation (a broken project parks alone with a node-inbox notice) and the
  `vowp-home`/`vowp-<project>` tmux layout.
- Project registry: `vowp project add|ls|pause|resume|archive|rm|weight`.

**Fleet read model**

- `snapshot.json` (`vowp-snapshot/1`) written every tick with the supervisor, project, queue,
  capacity, limit, inbox, session and orchestrator cards; `vowp schema snapshot`.
- `vowp fleet status|queue|inbox|capacity`, `vowp status` with project rows, and a board that shows
  every project.

**Measured capacity**

- A Linux sampler for memory, swap, load, CPU quota, disk and pressure, with a portable fallback;
  per-run cgroups or a `/proc` tree walk; `run_usage` and pooled p50/p90 estimates.
- Deterministic admission before every worker/verifier/planner/keeper run, with a waiting reason on
  the board; brakes that stop admissions and never kill running work; `auto` and `fixed` targets,
  owner override above the advice, and `vowp capacity show|report|set`.
- A keeper-profile advisor every 30 minutes and on events, delivered as a `capacity` inbox item;
  the AI proposes, the owner decides.

**Sessions on every surface**

- Surfaces and leases in the global database: one writing surface per session, 5-second
  heartbeats, a 60-second stale sweep, takeover with a clear `LeaseRevoked` notice, bounded drain,
  capsule and switch, and read-only follow mode.
- Remote execution: `vowp session turn-begin`, `vowp __exec --grant`, `vowp session ingest`, a
  durable per-run spool, idempotent ingest by event id and replay after an SSH drop or a killed
  client. The home stays the only writer; the executor owns its harness and injected values.
- `vowp chat` as the guaranteed terminal surface (`/switch`, `/status`, `/takeover`, `/follow`,
  `/capsule`) with the lease rules as its reference implementation.
- App hosts: an opencode server on the home joined through an SSH tunnel and `opencode pair`, and
  Claude Code Remote Control where a claude.ai login exists; the app transcript is mirrored into the
  canonical log at create/resume/startup.
- `vowp mcp`: a stdio MCP server with fleet, queue, inbox, capacity and session read tools and
  inbox, job, capacity, tunnel, secret and session write tools; every write checks the lease and
  records the actor.
- Orchestrator sessions: `vowp orchestrator new|ls|stop|resume`, project-local app config under
  `~/.vowp/orchestrators/<name>/`, join instructions for desktop, phone and chat, and automatic
  resume after a home restart.

**Control machine**

- `vowp control init|status` creates `~/.vowp-control/` (no supervisor, loops, graph or worktrees),
  routes project/fleet commands to the home and caches the snapshot with an `as of` time.
- `vowp tunnel open|ls|close` manages `ssh -L` forwards from the machine that uses them.
- A phone needs only an SSH app and the key: it runs the same `vowp chat` on the home.

**Secrets**

- `vowp secret set|ls|rm|upgrade|request`, basic mode and vault-user mode through the hidden
  `vowp __vault-exec`/`__vault-admin` helpers; values are never accepted as arguments.
- Injection by name through the harness `env_pass` (promotion/deploy commands do not receive
  secrets in this build), exact-value redaction from events, capsules, report artifacts, raw run
  mirrors and the snapshot, and no secrets in backups.
- A one-time `127.0.0.1` paste page with a 10-minute expiry and single use, reachable through a
  tunnel, so a value never enters a chat transcript.

**Lifecycle and backup**

- Lifecycle classes derived from database state, `vowp gc` dry run and `--apply` that never touches
  production, truth, open work or secrets, closed-session compaction and run-output
  compression/rotation.
- `vowp backup` of the global DB plus every project DB, capsules and the catalog, with 7 daily and
  4 weekly copies, offsite rsync/git targets, optional `age` encryption, restore and a restore
  drill.

**AI-first setup**

- `vowp setup plan|home|control|verify`: an idempotent, fully printed plan; a no-sudo path; a stop
  at every human hand-off; and a fixture verification that merges a job, reads the snapshot from the
  control machine and answers through `vowp chat`.
- `scripts/install.sh` and the systemd unit work with the new home and control roles.

**Docs and reference**

- The v0.2 specification (`docs/design/v0.2/home-and-surfaces.md`), v0.2 concepts for home,
  capacity and data lifecycle, and the setup contract.
- A regenerated CLI and JSON Schema reference covering the v0.1 and v0.2 command surface and every
  exported type.

### Changed

- `vowp up` runs one node supervisor for all projects instead of one per home; `vowp down`,
  `status`, `board`, `doctor`, `loops`, `schedule`, `backup`, `schema` and `node` reflect the home
  and project split. `vowp session switch` probes a remote target with `vowp doctor` before moving.
- `vowp init` creates the project's database and registers it through the project registry.
- Schedules and loops are per project; `--all` lists every project's rows. Provider limits stay
  node-wide.

### Removed

- The v0.1 single-database layout (migrated, not read in place) and the claim that several projects
  can share one `VOWP_HOME` with one supervisor each.

## [0.1.0] - 2026-11-09

The first public release of VOWP: a local-first supervisor for AI coding agents.
It runs stock Claude Code, opencode and generic CLI harnesses as job workers,
plans work as a job graph, keeps sessions in an append-only event log, verifies
work with recorded gate evidence, and ships changes through guarded, reversible
promotions.

0.1.0 is an **early release**. The v0.1 command surface is implemented and
tested, but the interfaces may change before v0.2. Known limitations, with
workarounds, are in
[`docs/release/0.1.0.md`](docs/release/0.1.0.md#known-limitations); the features
deliberately left out are in [`docs/roadmap.md`](docs/roadmap.md).

### Added

**Crates** (Rust edition 2024, MIT, workspace version `0.1.0`)

- `vowp-protocol`: shared types, events, capsules, config, gate validation
  (including the gate anti-pattern rules), adapter/service traits, `Clock` and
  JSON Schema export.
- `vowp-store`: SQLite schema and migrations, JSONL session logs, blobs,
  reindex and backup.
- `vowp-graph`: job DAG, state machine, ownership-overlap checks, dispatch
  planning, ASCII and Mermaid rendering, YAML import/export.
- `vowp-tmux`: safe tmux window manager (no `respawn`, no `send-keys`) and an
  in-memory fake.
- `vowp-adapters`: Claude Code, opencode and generic CLI adapters over the
  harnesses' headless/JSON interfaces; shared process runner, limit/error
  classification and secret redaction.
- `vowp-core`: session service, capsule writer and seeding, session switch with
  bounded drain and recovery, session bundle export/import, gates (command,
  report, review, deploy, secret-scan), cost ledger, schedules (cron, interval),
  the L1 supervisor tick (including the single-writer merge-queue step), L2
  worker prompts, L3 verify, L4 planner, L5 keeper, L6 automatic limit
  failover/probe-back, L7 inbox answers, L8 watchdog, guarded promotion and
  rollback.
- `vowp-cli`: the `vowp` binary.

**CLI commands** (full `--help` tree: [`docs/reference/cli.md`](docs/reference/cli.md))

- Project and node: `init`, `doctor`, `up` (foreground or `--detach`, stop
  file, SIGTERM/SIGINT cancellation), `down`, `status`, `board`, `attach`,
  `plan`, `node add|ls|rm`, global `--node` over SSH.
- Jobs and graph: `job add|update|ls|show|retry|park|nudge|restart|replan|fail|cancel`,
  `graph` (ASCII, Mermaid, export, import in dependency order).
- Sessions: `session ls|show|new|chat|switch|capsule|export|import|watch|tail`
  and the `chat` alias; cross-node sessions and reindex recovery.
- Operations: `inbox ls|show|ask|answer|dismiss`, `cost`, `loops` (list),
  `loops pause|resume`, `schedule add loop|command|job-template`,
  `schedule ls|pause|resume|rm`, `reindex`, `backup`, `schema`, `version`.
- Work → Production: `promote` (guards, serving snapshot, candidate/serving
  diff, approval, deploy, probe, recorded rollback command, `--dry-run`),
  `rollback`, `releases`.

**Adapters**: Claude Code (`claude -p`, stream-json, `rate_limit_event` limit
parsing), opencode (`opencode run --format json`) and a generic CLI adapter
driven by a TOML mapping file.

**Loops** (see [`docs/concepts/graph-and-loops.md`](docs/concepts/graph-and-loops.md)):
L1 tick every 60 s (reconcile, dispatch, gates, and step 7 of the tick — the
single-writer merge queue: rebase, repo-wide checks, secret scan,
fast-forward), L2 worker start/continue/fix, L3 independent verify with a
bounded fix loop, L4 planner, L5 keeper (covers planning when the planner has
no available rung), L6 automatic limit failover and probe-back, L7 inbox
answer files, L8 watchdog and L9 schedules. The merge queue is part of the L1
tick, not an L3 step.

**Sessions**: append-only event log per session, handoff capsules,
`session switch` between harness, model or node with a bounded drain, seeded
first turn and crash recovery; export/import bundles.

**Install and operations**: `scripts/install.sh` and `scripts/uninstall.sh`
(both support `--dry-run`), a systemd user unit, a cron-driven heartbeat
watchdog, and a hermetic test suite for them. The 0.1.0 release includes
prebuilt Linux and macOS archives, the seven `vowp-*` crates on crates.io and
the `vowp` npm wrapper.

**Framework**: templates, four role skills (`vowp-worker`, `vowp-planner`,
`vowp-verifier`, `vowp-keeper`) and agent presets installed by `vowp init`
under `.claude/skills/`.

**Docs and example**: concepts, guides, security model, troubleshooting, a
generated CLI and JSON Schema reference (`docs/reference/`), the design spec
(`docs/design/`), a docs command/link checker (`docs/tests/`) and the runnable
`examples/hello-vowp` fake-adapter demo.

[Unreleased]: https://github.com/syver777/vowp/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/syver777/vowp/releases/tag/v0.4.0
[0.3.0]: https://github.com/syver777/vowp/releases/tag/v0.3.0
[0.2.0]: https://github.com/syver777/vowp/releases/tag/v0.2.0
[0.1.0]: https://github.com/syver777/vowp/releases/tag/v0.1.0
