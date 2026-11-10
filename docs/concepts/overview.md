# Concepts overview

VOWP is a supervisor for AI coding agents. It turns a goal into a graph of small jobs, gives each job
one git worktree and one agent session, checks the result with instrumented gates and an independent
verifier, merges through a single queue, and can promote the merged result through guarded environments.

It is **not** a harness: it does not embed a model, replace Claude Code or opencode, or hold anyone's
login. It is closer to a build system and foreman for agents. The harnesses stay stock; VOWP supplies the
graph, the state machine, the evidence and the handoff format.

> **Status: current.** VOWP v0.1 is implemented. The design is complete in
> [`docs/design/`](../design/README.md); the commands described on this page run against the code on
> `main`.

## Principles

1. **Stock harnesses, no forks.** VOWP drives the documented headless/JSON interfaces. It never scrapes a
   TUI, never sends keystrokes to answer a dialog, and never reads, stores or forwards login tokens.
2. **The database is the truth for decisions; files are the truth for content.** SQLite holds jobs, edges,
   gates, runs, costs, limits, loops and inbox rows. Briefs, reports, session logs and blobs stay in plain
   files. Killing every process and restarting `vowp up` reconciles from those two sources.
3. **Deterministic loops, LLM judgment.** A plain program ticks every 60 seconds and never "decides to
   stop". Models are called only for judgment: planning, diagnosing a stall, reviewing a report, writing a
   handoff.
4. **Any model, any machine.** A model is a setting in a profile ladder, not an assumption. The same job
   can run on Claude Code on a laptop or another harness on a VPS.
5. **Evidence over claims.** A job is done when its gates pass with recorded evidence — commands, exit
   codes, diffs, verifier findings — not when an agent says so.
6. **Work → Production is guarded and reversible.** Promotions snapshot what is live, ship only the
   intended change, record the rollback and require a human yes by default.
7. **Cost is reported, not capped.** Real cost comes from the harness's receipt, a labelled price-table
   estimate, or `unknown` — never a silent zero. Retries and no-progress loops are bounded instead.
8. **Humans are never a bottleneck for unrelated work.** A question parks one job and lands in an inbox;
   everything else keeps running.

## The system at a glance

```
                    ┌──────────────────────────────────────────────┐
  you ──► vowp CLI ─┤ vowp-core                                    │
                    │  session · loops · gates · ledger · schedules│
                    │  promote                                     │
                    └───────┬───────────────┬──────────────────────┘
                            │               │
                    ┌───────▼──────┐  ┌─────▼─────────┐
                    │ vowp-store   │  │ vowp-graph    │
                    │ SQLite, logs │  │ DAG, states,  │
                    │ blobs        │  │ dispatch      │
                    └──────────────┘  └───────────────┘
                            │
        ┌───────────────────┼────────────────────┬──────────────────┐
        ▼                   ▼                    ▼                  ▼
  vowp-tmux           vowp-adapters        vowp-protocol      git worktrees
  windows             claude-code          types, traits,     one per job
                      opencode             config, schema
                      generic cli
                            │
                            ▼
                 harness child processes
```

One `vowp` binary contains everything. `vowp up` is the long-running supervisor; every other
invocation is a short-lived client of the same SQLite database (WAL mode). A node runs at most one supervisor per
project home, enforced by a lock file. There is no daemon, no listening socket and no network listener in
v0.1.

## Vocabulary

| Term | Meaning |
|---|---|
| **Node** | A machine running VOWP: your laptop (`local`), a VPS (`vps`), any server. |
| **Project** | A repository homed on one node, registered by `vowp init`. |
| **Harness** | An agent program: Claude Code, opencode, Codex, Gemini CLI, … |
| **Adapter** | VOWP code that drives one harness headlessly and turns its output into canonical events. |
| **Session** | A universal, harness-neutral conversation with a stable id and an append-only event log. |
| **Executor** | One concrete way to run a model: `harness:model[@effort][#variant]`. |
| **Profile** | A role's ordered ladder of executors (rung 0 is preferred). |
| **Capsule** | A structured handoff written when a session switches harness, model or node. |
| **Switch** | Moving a session to another executor, recorded as lineage. |
| **Window** | A view onto live work: a tmux window, the board, or a chat. |
| **Job** | A graph node: one unit of work with a brief, owned paths, gates and a report. |
| **Edge** | A relation between jobs: `needs`, `verifies`, `fixes`, `escalates`, `blocks`. |
| **Graph** | Jobs plus edges. A DAG except for bounded `fixes` loop edges. |
| **Gate** | A machine-checkable condition with recorded evidence. |
| **Loop** | A repeating process with a tick, an input, actions and bounds (L1–L9). |
| **Supervisor** | The deterministic program that runs the loops (`vowp up`). |
| **Role** | `planner`, `keeper`, `worker` or `verifier`. |
| **Slot** | A concurrent worker place, also a tmux window. |
| **Inbox** | Questions and approvals waiting for a human. |
| **Ledger** | Append-only record of dispatches, gate results, merges, costs and switches. |
| **Environment** | Where code runs: `work` (staging) and `prod`, each with its own checkout and guards. |
| **Promotion** | Moving a verified change to an environment through guards, with a snapshot and rollback. |

## The life of one job

1. **Added.** `vowp job add` validates the id and dependencies, parses the gate list, inserts the job and
   its `needs` edges, and writes the brief. Custom command gates are validated when they run, not at add
   time. The planner can do the same through the CLI.
2. **Ready.** L1 sees that every `needs` job is `done` and moves the job from `pending` to `ready`.
3. **Dispatched.** When a worker slot is free, the job's owned paths do not overlap another running job,
   and its provider is not limited, L1 creates the worktree `<worktree_root>/<id>` and branch
   `job/<id>`, opens a tmux window and starts the worker session.
4. **Running.** The worker reads `AGENTS.md` and its brief, edits only `owns`, runs its gates, commits
   and writes the report. Large tool output is spilled to content-addressed blobs in the session log.
5. **Gating.** Command gates run in the job worktree and record evidence: command, exit code, duration and
   the last 200 lines, with long output stored as a blob.
6. **Reviewing.** A separate verifier job reads the brief, the diff and the gate evidence, runs what it
   needs to, and writes `verdict: pass` or concrete findings. Failure creates a bounded fix job in the
   same worktree.
7. **Merging.** The single merge queue rebases onto the default branch, re-runs the repository-wide
   checks, scans the diff for secrets and fast-forwards `main`. Conflicts become a `merge` job.
8. **Done.** The worktree and branch are removed, dependents unblock, and the ledger has a merge record.

A failure never silently disappears: each outcome moves through the state machine, and exhausted retries
create a decision job or an inbox item. The full table is in
[graph and loops](graph-and-loops.md#the-job-state-machine).

## Where state lives

The CLI resolves the node home from `--home <dir>`, else `$VOWP_HOME`, else `~/.vowp`; tests always
pass an explicit temporary directory.

```
~/.vowp/
├─ vowp.db                     SQLite in WAL mode — decisions and indexes
├─ config.toml                 optional node-wide defaults
├─ supervisor.lock             pid of the running supervisor
├─ heartbeat                   unix seconds, rewritten every tick
├─ board.json                  snapshot for external readers
├─ sessions/<sessionId>/
│    ├─ log.jsonl              authoritative append-only event log
│    ├─ blobs/<sha256>         spilled tool output and attachments
│    ├─ capsules/…json         handoff documents
│    └─ adapters.json          harness-side session ids
├─ runs/<runId>.jsonl          raw harness stdout (debug only)
├─ logs/supervisor.log         structured loop log
├─ releases.log                append-only promotion and rollback history
├─ backups/                    `vowp backup` output
└─ inbox/<inboxId>.md          human-readable copy of inbox items
```

Inside a project, `vowp init` writes `vowp.toml`, the embedded framework files sourced from
[`framework/`](../../framework/README.md), and a `.vowp/` directory for job briefs, reports, optional graph
snapshots and planner checkpoints. The release log lives in the node home (`~/.vowp/releases.log`), not
in the repository. Job worktrees live outside the repository, by default in
`<repo>/../<repo-name>-wt/<job-id>`.

The session log is the authority for conversation content. The corresponding rows in `vowp.db` are an
index that `vowp reindex` can rebuild. Deleting the database never loses session content;
deleting a session directory does.

## What v0.1 includes

- The protocol types, SQLite store, graph engine, safe tmux manager, Claude Code/opencode/generic CLI
  adapters, supervisor loops L1–L8, schedules L9, gates, failover, capsule switch, the CLI, framework
  files, docs, the [install script](../../scripts/README.md), the
  [hello-vowp example](../../examples/hello-vowp/README.md) and CI.
- Work → Production promotion and the optional read-only `vowp session watch` importer are also
  included.

## What is deliberately deferred

- The HTTP+SSE daemon and web/PWA window (`v0.2`), the ACP adapter, cross-node sync, teams/multi-user and
  IANA timezones beyond `UTC` and fixed offsets. Event and job schemas are frozen in v0.1 so the daemon
  can be added without rewriting the core.

Next: [graph and loops](graph-and-loops.md), or the [quickstart](../guides/quickstart.md) if you would
rather run it.
