# VOWP design specification — index

Status: current for v0.3 (the v0.2 home and surface specification remains the binding base).

This directory describes how VOWP works: architecture, types, database schema, behavioural contracts
and the command line. The **v0.1 implementation spec** is still the base; the **v0.2 spec** in
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) is normative for the home, fleet, capacity,
surfaces, secrets, lifecycle and setup changes. `plan.md` is the original design summary;
`decisions.md` records every place where the v0.1 detailed spec tightens or clarifies it.

## How to use it

**If you are a contributor**: start with `architecture.md`, then the crate file for the area you want
to change, and update the spec in the same change as the code. For anything v0.2 touches, read the
v0.2 spec's section for that area before the v0.1 file; the v0.2 spec wins where they differ.

1. `conventions.md` (Rust rules, dependency list, test rules).
2. `types.md` for every type you touch — types are defined **once**, in `vowp-protocol`; never
   redefine them. v0.2 additions are in §16 of that file.
3. Your crate's file in `crates/` — module layout, the complete public API with exact signatures,
   behaviour rules, and the **named tests** that prove it.
4. The behavioural document your change is part of (`adapters.md`, `loops.md`, `sessions.md`,
   `cli.md`) and the schema under `schema/` if you touch the database.
5. The v0.2 spec (`v0.2/home-and-surfaces.md`) for the global/project split, supervisor, snapshot,
   capacity, surfaces/leases, remote execution, apps, MCP, control machine, secrets, lifecycle,
   setup, the new CLI commands, interface hooks and named tests.

Names in the spec are binding: same type, table, column, command and test names. If the spec is
ambiguous or wrong, open an issue rather than inventing a different design.

## Files

| File | What it defines |
|---|---|
| `plan.md` | The original design summary: principles, concepts, architecture and behaviour. |
| `decisions.md` | Deviations/clarifications relative to the design summary, each with its reason; open owner questions. |
| `architecture.md` | Components and data flow, threading model, on-disk layout of a home and a control machine, the full `vowp.toml` schema, error/logging conventions, security model, compatibility. |
| `schema/global.sql` | The v0.2 global SQLite schema (schema version 2): node registry, projects, sessions, surfaces/leases, capacity, catalog, node inbox/ledger, limits, kv. |
| `schema/project.sql` | The v0.2 per-project SQLite schema (schema version 1): jobs, edges, runs, gate results, workers, loops, schedules, project inbox/ledger, releases. |
| `schema.sql` | The v0.1 single-DB schema, kept as the migration source description. |
| `types.md` | Every v0.1 protocol type as Rust with serde attributes and JSON examples, plus the v0.2 capacity, surface/lease, snapshot, catalog and vault types in §16. |
| `conventions.md` | Rust conventions, per-crate dependency list, test and fixture rules, naming. |
| `crates/vowp-protocol.md` | Crate API beyond `types.md`: modules, helper traits (`Log`, `FakeClock`), tests. |
| `crates/vowp-store.md` | SQLite access, the global/project split, migration, session logs, blobs, reindex, backup. |
| `crates/vowp-graph.md` | DAG, state machine, ownership overlap, dispatch planning, rendering. |
| `crates/vowp-tmux.md` | Safe tmux window manager and its in-memory fake. |
| `crates/vowp-adapters.md` | Claude Code, opencode and generic cli adapters; classification; process helpers. |
| `crates/vowp-core.md` | `session`, `loops`, `gates`, `ledger`, `schedules`, `promote`, `capacity`, `surface`, `fleet`, `lifecycle`, `vault` modules. |
| `crates/vowp-cli.md` | The `vowp` binary: module layout, context and routing, output, exit codes, remote execution, MCP, setup. |
| `adapters.md` | Adapter behaviour: claude-code and opencode mappings, cli mapping-file format, limit/credit/auth/context classification, cost receipts, fixtures. |
| `loops.md` | L1–L9, the job state-machine transition table, gates, merge queue, failover, inbox, watchdog, schedules, drills. |
| `sessions.md` | Canonical event log, reindex, capsule writer, switch protocol, v0.2 surfaces/leases/takeover, remote execution, app hosts, export/import bundle. |
| `cli.md` | Every command: arguments, flags, human and `--json` output, exit codes, `--node` over ssh, home/control routing. |
| `job-map.md` | The job kinds, ownership and dispatch map used by the supervisor. |

## v0.2 design notes

| File | Status | What it covers |
|---|---|---|
| [`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) | **normative for v0.2** | Data model + DDL, migration, node supervisor, snapshot/fleet, capacity, remote execution, leases/takeover, app hosts/MCP/chat, control machine, secrets, lifecycle, setup, CLI, interface hooks, named tests, open assumptions. |
| [`v0.2/README.md`](v0.2/README.md) | current | The v0.2 note index, binding assumptions, reading order and out-of-scope list. |
| [`v0.2/cross-node-sync.md`](v0.2/cross-node-sync.md) | updated for v0.2 | The v0.1 bundle move; v0.2 remote execution supersedes "move the session", leaving directory sync to v0.3. |
| [`v0.2/acp-adapter.md`](v0.2/acp-adapter.md) | deferred | Agent Client Protocol adapter design; not v0.2 delivery. |
| [`v0.2/cost-dashboard.md`](v0.2/cost-dashboard.md) | deferred | Cost dashboard on the v0.3 daemon API; not v0.2 delivery. |
| [`v0.3/README.md`](v0.3/README.md) | planned | Daemon HTTP+SSE API, web window and phone browser board. |

## Vocabulary (one meaning everywhere)

*Home* (the machine that owns the truth) · *control machine* (a laptop, desktop or phone that talks
to the home) · *node* (a machine in the registry; a laptop can be an executor node) · *project* (a
repo registered on the home) · *job* (graph node) · *edge* (`needs|verifies|fixes|escalates|blocks`) ·
*gate* (checkable condition + evidence) · *run* (one adapter process) · *session* (universal
conversation, event log on the home) · *event* (log line) · *capsule* (handoff) · *surface* (where a
person talks to a session: chat, app or MCP server) · *lease* (one surface's right to write) ·
*switch* (executor change) · *executor* (`harness:model@effort#variant`) · *profile* (role's ladder of
executors) · *limit* (`quota|credits|rate|context|auth`) · *loop* (L1–L9) · *schedule* (L9 row) ·
*ledger* (append-only record) · *inbox* (human questions) · *release* (promotion).

A **window** is only a tmux window, never a surface.

## Status of the spec

The v0.2 **feature set** is implemented: one home, one node supervisor, the global + per-project
database split, the snapshot/fleet read model, measured capacity, sessions on surfaces with leases
and remote execution, app hosts, MCP, the chat fallback, control machines, secrets, lifecycle/backup
and AI-first setup. That is not a claim that every item in the v0.2 specification has shipped: the
architecture and roadmap record the remaining follow-ups (for example gate/ledger redaction,
deploy-command secret injection and the duplicate `session watch` session). The public HTTP+SSE
daemon, web window, phone browser board, ACP adapter, cost dashboard, multi-user accounts and
multi-home sync remain planned (v0.3 or later) and are marked as such in
[`../roadmap.md`](../roadmap.md).
