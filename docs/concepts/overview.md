# Concepts overview

VOWP is a VPS framework for AI agents to work on projects. It turns one Linux VPS into the home that
owns the work, then turns a goal into a graph of small jobs, gives each job one git worktree and one
agent session, checks the result with instrumented gates and an independent verifier, merges through a
single queue, and can promote the merged result through guarded environments.

It is **not** a harness: it does not embed a model, replace Claude Code or opencode, or hold anyone's
login. It is closer to a build system and foreman for agents. The harnesses stay stock; VOWP supplies the
home, the graph, the state machine, the evidence and the handoff format. For the one-page view, see
[What is VOWP](what-is-vowp.md).

> **Status: current.** VOWP v0.3.0 is implemented on `main`. The pages in this directory describe
> that code. The binding home and surface specification is
> [`docs/design/v0.2/home-and-surfaces.md`](../design/v0.2/home-and-surfaces.md); v0.3 adds the
> loopback API, the window and session pull.

## The shape of v0.3

v0.3 has three kinds of place:

- **The home** is the authority. VOWP is VPS-first: the home is normally a Linux VPS that stays on.
  One `vowp` supervisor runs there for every active project; it owns the global database, one database
  per project, the session logs, the snapshot, the capacity sampler and the tmux windows.
- **Control machines** are where people talk to the home: a laptop, a desktop app, or a phone with an
  SSH client. They hold no truth. They cache a snapshot, open SSH tunnels and run a surface.
- **Executor nodes** run a turn's harness. The home is the default executor node; a laptop can be
  registered as an executor so its own files, browser and tunnels are available to the session.

A **surface** is where a person is talking to a session right now: the opencode desktop app, a Claude
app, `vowp chat`, or the MCP server those apps call. A session has one append-only log on the home and
at most one surface with the **lease** to write to it. Every other surface can follow read-only.

```
 laptop / phone (control)                         home (the authority)
 ┌─────────────────────────────┐                  ┌──────────────────────────────────────────────┐
 │ opencode desktop / Claude   │                  │ one vowp supervisor                          │
 │ vowp chat / vowp mcp        │   ssh (key)      │  global DB  ~/.vowp/vowp.db                  │
 │ snapshot cache, tunnels     │ ───────────────► │  project DBs ~/.vowp/projects/<p>/project.db │
 └─────────────────────────────┘                  │  sessions/<id>/log.jsonl                    │
                                                  │  snapshot.json every tick                    │
                                                  │  tmux: vowp-home, vowp-<project>             │
                                                  │  workers / verifiers → stock harnesses       │
                                                  └──────────────────────────────────────────────┘
```

The home opens no public port. The opencode app server binds loopback and is reached through an SSH
tunnel; the one-time secret page is also loopback-only. A phone needs an SSH app and the owner's key,
nothing else.

## Principles

1. **Stock harnesses, no forks.** VOWP drives the documented headless/JSON interfaces. It never scrapes
   a TUI, never sends keystrokes to answer a dialog, and never reads, stores or forwards login tokens.
2. **The home is the only writer.** A control machine, app or MCP server sends `vowp` commands to the
   home; it never opens a database file or appends a session log. The snapshot cache and a remote
   executor's spool are the only local state they keep.
3. **The database is the truth for decisions; files are the truth for content.** SQLite holds the
   project registry, jobs, edges, gates, runs, costs, limits, surfaces, leases, capacity and inbox rows.
   Briefs, reports, session logs and blobs stay in plain files. Killing every process and restarting
   `vowp up` reconciles from those two sources.
4. **Deterministic loops, LLM judgment.** A plain program ticks every 60 seconds and never "decides to
   stop". Models are called only for judgment: planning, diagnosing a stall, reviewing a report, writing
   a handoff, or proposing a capacity target.
5. **Any model, any machine, any surface.** A model is a setting in a profile ladder, not an assumption.
   The same session can run on Claude Code on a laptop or opencode on the home, and can be continued
   from another app.
6. **Evidence over claims.** A job is done when its gates pass with recorded evidence — commands, exit
   codes, diffs, verifier findings — not when an agent says so.
7. **Work → Production is guarded and reversible.** Promotions snapshot what is live, ship only the
   intended change, record the rollback and require a human yes by default.
8. **Cost is reported, capacity is measured.** Real cost comes from the harness's receipt, a labelled
   price-table estimate, or `unknown` — never a silent zero. Capacity admission uses measured RAM, CPU,
   disk and provider limits; it is not a spending budget.
9. **Humans are never a bottleneck for unrelated work.** A question parks one job and lands in an
   inbox; everything else keeps running.
10. **Never kill running work for capacity.** Admission and brakes stop new runs. The only interrupts are
    the explicit switch, takeover and `vowp down` paths.

## Vocabulary

| Term | Meaning |
|---|---|
| **Home** | The machine that owns the global DB, the project DBs, the session logs, the snapshot and the one node supervisor. One per installation in v0.3. |
| **Control machine** | A computer or phone that talks to the home. It holds no truth; it may cache a snapshot. |
| **Node** | A machine name in the `nodes` registry. The home is the only node that writes; a laptop can be an executor node. |
| **Project** | One git repository registered on the home. v0.3 runs every active project under one supervisor. |
| **Harness** | An agent program: Claude Code, opencode, Codex, Gemini CLI, … |
| **Adapter** | VOWP code that drives one harness headlessly and turns its output into canonical events. |
| **Session** | A universal, harness-neutral conversation with a stable id and an append-only event log on the home. |
| **Surface** | Where a person talks to a session now: an app, `vowp chat` or an MCP server. |
| **Lease** | The right of one surface to send input to one session. A lease without a heartbeat for 60 s is free. |
| **Executor** | One concrete way to run a model: `harness:model[@effort][#variant]`, plus the node that runs it. |
| **Profile** | A role's ordered ladder of executors (rung 0 is preferred). |
| **Capsule** | A structured handoff written when a session switches harness, model, node or surface. |
| **Switch** | Moving a session to another executor, recorded as lineage. |
| **Window** | Only a tmux view onto live work. A window is never a surface. |
| **Orchestrator** | The long-lived session that reads the fleet snapshot, proposes changes and asks the owner before applying them. |
| **Job** | A graph node: one unit of work with a brief, owned paths, gates and a report. |
| **Edge** | A relation between jobs: `needs`, `verifies`, `fixes`, `escalates`, `blocks`. |
| **Graph** | Jobs plus edges. A DAG except for bounded `fixes` loop edges. |
| **Gate** | A machine-checkable condition with recorded evidence. |
| **Loop** | A repeating process with a tick, an input, actions and bounds (L1–L9). |
| **Supervisor** | The deterministic program that runs the loops (`vowp up`). |
| **Role** | `planner`, `keeper`, `worker`, `verifier` or `orchestrator`. |
| **Slot** | A concurrent worker place, also a tmux window. |
| **Inbox** | Questions, notices and approvals waiting for a human. Project items live in the project DB; node items in the global DB. |
| **Ledger** | Append-only record of dispatches, gate results, merges, costs and switches. |
| **Snapshot** | `~/.vowp/snapshot.json`, the derived read model written every tick. |
| **Capacity target** | The number of concurrent runs the node admits, set by measurement (`auto`) or the owner (`fixed`). |
| **Lifecycle class** | What a file is and whether it may ever be deleted: production, truth, open work, secrets, or a disposable class. |
| **Environment** | Where code runs: `work` (staging) and `prod`, each with its own checkout and guards. |
| **Promotion** | Moving a verified change to an environment through guards, with a snapshot and rollback. |

## The life of one job

1. **Added.** `vowp job add` validates the id and dependencies, parses the gate list, inserts the job and
   its `needs` edges, and writes the brief. The planner can do the same through the CLI.
2. **Ready.** The project's L1 loop sees that every `needs` job is `done` and moves the job from
   `pending` to `ready`.
3. **Admitted.** Before a worker slot is used, the node capacity manager checks the brake, the provider,
   the worker target, RAM, CPU and disk. A job that cannot start stays `ready` and shows the reason
   (`waiting: RAM 1.2 GB short`) in the queue and the snapshot.
4. **Dispatched.** When capacity is available, the job's owned paths do not overlap another running job,
   and its provider is not limited, L1 creates the worktree `<worktree_root>/<id>` and branch
   `job/<id>`, opens a tmux window and starts the worker session.
5. **Running.** The worker reads `AGENTS.md` and its brief, edits only `owns`, runs its gates, commits
   and writes the report. Large tool output is spilled to content-addressed blobs in the session log.
6. **Gating.** Command gates run in the job worktree and record evidence: command, exit code, duration
   and the last 200 lines, with long output stored as a blob.
7. **Reviewing.** A separate verifier job reads the brief, the diff and the gate evidence, runs what it
   needs to, and writes `verdict: pass` or concrete findings. Failure creates a bounded fix job in the
   same worktree.
8. **Merging.** The single merge queue rebases onto the default branch, re-runs the repository-wide
   checks, scans the diff for secrets and fast-forwards `main`. Conflicts become a `merge` job.
9. **Done.** The worktree and branch are removed, dependents unblock, and the ledger has a merge record.

A failure never silently disappears: each outcome moves through the state machine, and exhausted
retries create a decision job or an inbox item. The full table is in
[graph and loops](graph-and-loops.md#the-job-state-machine).

## Where state lives

The CLI resolves a home from `--home <dir>`, else `$VOWP_HOME`, else `~/.vowp`; on a control machine it
finds `~/.vowp-control/` and routes commands to the home. Tests always pass an explicit temporary
directory.

```
~/.vowp/
├─ vowp.db                     global DB: registry, sessions index, surfaces/leases,
│                              limits, capacity, catalog, node inbox and ledger
├─ projects/<name>/project.db  one DB per project: jobs, edges, runs, gates, loops,
│                              schedules, project inbox and ledger, releases
├─ sessions/<sessionId>/
│    ├─ log.jsonl              authoritative append-only event log
│    ├─ blobs/<sha256>         spilled tool output and attachments
│    ├─ capsules/…json         handoff documents
│    ├─ summary.md             written when a closed session is compacted
│    └─ adapters.json          harness-side session ids
├─ orchestrators/<name>/       project-local app config, lease token and host state
├─ secrets/                    basic-mode secret files (0700/0600); never backed up
├─ snapshot.json               node read model, rewritten atomically every tick
├─ board.json                  v0.1 copy for one active project (or the only paused one)
├─ migration.json              present only while a v0.1 → v0.2 migration is in flight
├─ archive/projects/<name>-<ts>/  vowp `project rm` archive
├─ runs/<runId>.jsonl          raw harness stdout (debug only)
├─ logs/supervisor.log         structured loop log
├─ releases.log                append-only promotion and rollback history
├─ backups/                    `vowp backup` output
└─ inbox/<inboxId>.md          human-readable copy of inbox items
```

A control machine keeps only derived state:

```
~/.vowp-control/
├─ control.json                role, home name, SSH target, remote home (0600)
├─ nodes.json                  node registry mirror
├─ snapshot.json               last snapshot from the home
├─ snapshot.meta.json          when it was fetched and from where
├─ spool/<session>/<run>/       unacked remote events and their ack file
├─ surfaces.json               local surface state
└─ tunnels.json                managed forwards and pids (0600)
```

Inside a project, `vowp init` writes `vowp.toml`, the embedded framework files sourced from
[`framework/`](../../framework/README.md), and a `.vowp/` directory for job briefs, reports, optional
graph snapshots and planner checkpoints. That directory holds project runtime content, not a database;
the databases live in the home. Job worktrees live outside the repository, by default in
`<repo>/../<repo-name>-wt/<job-id>`.

The session log is the authority for conversation content. The corresponding rows in the global DB are
an index that `vowp reindex` can rebuild. Deleting the database never loses session content; deleting a
session directory does. A full backup now includes the session content as well as both database types;
see [data lifecycle](data-lifecycle.md).

## What v0.3 includes

- One node supervisor for every active project, with project pause/resume/archive and a broken project
  parked alone.
- The global + per-project database split, the v0.1 → v0.2 migration, and a per-project archive
  directory.
- `snapshot.json`, vowp `schema snapshot`, and vowp `fleet status`, `fleet queue`, `fleet inbox` and
  `fleet capacity`.
- Measured capacity: host sampling, per-run usage, estimates, admission, brakes, owner targets and an
  advisor that only proposes.
- Sessions on every surface: leases, takeover, follow mode, remote execution with a durable spool,
  opencode and Claude app hosts, the MCP server, `vowp chat` and orchestrator sessions.
- Control machines: vowp `control init`, snapshot cache, SSH tunnels, and phone access with an SSH app.
- The v0.3 loopback HTTP/SSE daemon, web/PWA window and phone-friendly board, plus the ACP adapter
  and explicit session pull between nodes.
- Secrets in basic or vault-user mode, injected by name, with exact-value redaction in session logs, capsules, report artifacts and the snapshot.
- Lifecycle classes, vowp `gc`, closed-session compaction, backup/restore and the nightly GC schedule.
- AI-first setup: vowp `setup plan`, vowp `setup home` and vowp `setup control`.

## What is deliberately deferred

- The browser cost dashboard.
- More than one home, replication, leader election and multi-VPS scheduling.
- Multi-user accounts, quotas, roles or audit.
- Windows support and a native VOWP desktop app.

Next: [home and surfaces](home-and-surfaces.md), or the [quickstart](../guides/quickstart.md) if you
would rather run it.
