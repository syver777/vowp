# Changelog

All notable changes to VOWP are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/).

Release notes for the first public release are in
[`docs/release/0.1.0.md`](docs/release/0.1.0.md). The product scope and the
features deliberately left out of v0.1 are in [`docs/roadmap.md`](docs/roadmap.md).

## [Unreleased]

Nothing yet.

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

[Unreleased]: https://github.com/syver777/vowp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/syver777/vowp/releases/tag/v0.1.0
