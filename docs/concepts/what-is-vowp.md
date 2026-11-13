# What VOWP is

> **Status: current — VOWP 0.3.0.** The concepts on this page describe the shipped framework. The
> 0.4 direction at the end is a design note, not shipped.

**VOWP is a VPS framework for AI agents to work on projects.** It turns one Linux VPS into the
always-on **home** for a fleet of agents: a global database, one database per project, append-only
session logs, a deterministic supervisor, job graphs, loops, cron schedules, measured capacity and a
single merge queue.

VOWP is not a harness, a model or a hosted service. You bring the VPS and the harness logins; VOWP
gives the agents a shared home, a history they can continue from, and a delivery process with
evidence.

## A VPS framework, not another agent

| VOWP is | VOWP is not |
|---|---|
| A home for many projects and agents | A model or a chat app |
| A deterministic supervisor and job graph | A prompt library |
| A database of sessions, jobs, gates and costs | A harness fork |
| A surface-agnostic session log | A hosted SaaS |
| A guarded Work → Production path | A VPS provider |

## The shape

- **Home (the VPS).** The only writer. It owns the global DB, the project DBs, the session logs, the
  supervisor, the snapshot, the capacity sampler and the schedules. It opens no public port.
- **Control machine.** A laptop, desktop or phone. It talks to the home over SSH, caches a snapshot
  and opens tunnels. It holds no truth.
- **Surface.** Where a person talks to a session now: the opencode desktop app, a Claude app,
  `vowp chat`, or an MCP server. One surface holds the lease; the rest follow read-only.
- **Executor node.** A registered machine that runs a turn. The home is the default; a laptop can be
  an executor when the work needs its local files, browser or tunnels.

## Five things that make it a framework

### 1. Interchangeable sessions

Every conversation is a universal, harness-neutral session with a stable id and an append-only
`log.jsonl` on the home. A session can move between models, harnesses, nodes and surfaces; a capsule
carries the context and the lease rules prevent two writers. The history does not belong to the
harness.

### 2. A built-in database for the work

SQLite holds the decisions: the project registry, jobs and edges, gates and evidence, runs, costs,
worker slots, leases, capacity, schedules and inbox. Content stays in plain files: briefs, reports,
session logs and blobs. Kill every process, restart, and the supervisor reconciles.

### 3. A fleet of workers on nodes

One supervisor ticks every active project in fairness order. Nodes are registered and tracked; the
snapshot shows projects, jobs, workers, capacity, provider limits, inbox and orchestrators. A broken
project parks alone. Capacity is measured, not guessed; brakes stop new runs and never kill running
work.

### 4. Systematic graphs, loops and schedules

A goal becomes a DAG of small jobs. L1 dispatches ready work, L2 continues a run, L3 verifies it
independently, and a single merge queue rebases, re-checks and fast-forwards. L1–L9 are deterministic
loops, not agent whims. Cron and interval schedules can create jobs, trigger a loop or run a command,
with missed-run handling and at-most-once keys.

### 5. Harness and provider freedom

Claude Code, opencode, Codex and other CLI agents are driven through their documented headless
interfaces. A model is a rung in a profile ladder, with fallbacks and shared provider limits. VOWP
does not fork a harness, scrape a TUI, or read a login token.

## What runs where

| Place | Runs | Holds |
|---|---|---|
| VPS home | supervisor, jobs, worktrees, harnesses, schedules | all truth |
| Control machine | app, `vowp chat`, `vowp mcp`, tunnels | snapshot cache, spool, surface state |
| Executor node | a turn's harness | its own checkout and login |
| Surface | the conversation | a lease, never the log |

## What it is not

- Not a VPS provider: you own the server, the domain and the bill.
- Not a harness: it does not embed a model or replace Claude Code, opencode or Codex.
- Not a public service: the home opens no public port; access is SSH with your key.
- Not multi-home in 0.3: one home per installation. Teams and replication are out of scope.
- Not a substitute for review: gates and evidence make the work checkable; a human still owns the
  promotion decision.

## Start here

- [Setup contract](../setup/agent-setup.md) — the copyable prompt and the exact setup sequence.
- [Concepts overview](overview.md) — the vocabulary and where state lives.
- [Home and surfaces](home-and-surfaces.md) — the VPS home, leases, takeover, apps and MCP.
- [Graph and loops](graph-and-loops.md) — jobs, gates, L1–L9 and the merge queue.
- [Sessions and switching](sessions-and-switching.md) — capsules, executors and handoff.
- [Schedules](../guides/schedules.md) — cron and interval work.
- [Running a home on a VPS](../guides/vps.md) — the server guide.

## 0.4 direction

0.3 keeps the VPS as the only home. The 0.4 design work explores a **local surface** and a **local
node**: the orchestrator app can run on your Mac with local context while the session log stays on
the VPS, and an outbound-only node agent can run offloaded jobs on the Mac. The VPS remains the only
writer; the local machine is a surface and an executor, not a second home.
