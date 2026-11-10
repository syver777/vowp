# Graph and loops

The graph is the plan; the loops are what keep it moving. A job is a node in the graph, a role is who
does it, and a gate is how the result is checked. The supervisor is a deterministic program that walks
the graph every tick, starts work, records evidence and moves states. Models are asked for judgment only.

> **Status: current.** The graph engine and the `vowp job`/`vowp graph` commands are implemented; the
> supervisor commands such as `vowp up` and `vowp status` that drive the loops are implemented too.

## Jobs

A job is one unit of work with a contract. It is stored in the `jobs` table and edited only through
validated CLI commands, never by hand.

| Field | Meaning |
|---|---|
| `id` | Stable slug, e.g. `example-graph`. Full slug required on the CLI. |
| `title` | Short human description. |
| `kind` | `plan`, `spec`, `build`, `test`, `verify`, `fix`, `merge`, `docs`, `research`, `promote`, `release`, `decision`. |
| `role` | `planner`, `keeper`, `worker` or `verifier`; defaults from the kind. |
| `profile` | Name of the model ladder used to run the job. Defaults to the role name. |
| `state` | One of the thirteen states below. |
| `priority` | Dispatch order among ready jobs; default 50, higher first, then older. |
| `needs` | Jobs that must be `done` before this one can become `ready`. |
| `owns` | Repo-relative globs the job may change. The only write lock it takes. |
| `reads` | Paths the job should read; not a lock. |
| `brief` | Path to the job contract, normally `.vowp/jobs/<id>/brief.md`. |
| `report` | Path to the result, normally `.vowp/jobs/<id>/report.md`. |
| `gates` | Ordered gate list. Defaults depend on the kind; commands are added per job. |
| `loop` | `until: gates`, `max_attempts` (default 3), `on_exhausted` (`escalate`, `park` or `fail`). |
| `branch`, `worktree` | Created at dispatch; verify, fix and merge jobs reuse the original's. |
| `attempts`, `continues`, `hang_restarts` | Bounded retry counters. |
| `parked_reason`, `note`, `created_by` | Why it waits, a human note, and who created it. |

### Edges

An edge reads **source → target**: `src needs dst`, `src verifies dst`, `src fixes dst`,
`src escalates dst`, `src blocks dst`.

| Edge | Meaning |
|---|---|
| `needs` | `src` depends on `dst` being done. Ordinary DAG edge. |
| `verifies` | `src` is the automatic verifier for `dst`. |
| `fixes` | `src` is a bounded fix job for `dst`. This is the only **loop edge** allowed in the DAG. |
| `escalates` | `src` is a decision job created when attempts are exhausted. |
| `blocks` | `src` (normally a decision job) blocks the jobs that need the original. |

A `needs` cycle is rejected at `vowp job add` time. Fix jobs run in the original job's branch and
worktree, so a fix never races the job it repairs.

## The job state machine

```
                  needs done             dispatched
   pending ───────────────► ready ─────────────────► running
                                                       │  report ready
                                                       ▼
                                                    gating ──gates pass──► reviewing
                                                       ▲                       │
                                         fix done      │                       │ verify pass
                                                       │                       ▼
                                                    fixing ◄──gates/verify fail│
                                                       ▲                       │
                                                       │                       ▼
                                                       └──conflict/checks fail─ merging ──► done
```

A fix job returns the original to `gating` after a gate or verify failure, and to `merging` after a merge
conflict. The diagram is the main path; the full transition table has the guards and side effects. The
real machine also has `stalled`, `failed`, `parked`, `replanned` and `cancelled`. `gating` and
`cancelled` are deliberate additions to the original plan: they make the loop idempotent across crashes
and give the board something honest to show.

| State | Meaning |
|---|---|
| `pending` | Waiting for its `needs` jobs. |
| `ready` | Dependencies are done; waiting for a slot, an ownership lock and a provider. |
| `running` | A worker run is attached. |
| `stalled` | The run stopped making progress; the keeper decides what to do. |
| `gating` | Command gates are executing. |
| `reviewing` | A verifier is checking the report, diff and evidence. |
| `fixing` | A fix or merge job is repairing the original in the same worktree. |
| `merging` | The merge queue is rebasing, checking and pushing. |
| `done` | Merged (or, for verify/fix/merge jobs, finished successfully). Terminal. |
| `failed` | Retries exhausted or the keeper failed it; can be retried or replanned. Not terminal. |
| `parked` | Waiting for a human, a provider or a manual decision. Not terminal. |
| `replanned` | The keeper handed the work back to the planner. Terminal. |
| `cancelled` | A human cancelled it; branch and worktree are kept for inspection. Terminal. |

The transition table, guards and counter effects are in
[`docs/design/loops.md` §2](../design/loops.md). The CLI exposes the safe subset: `vowp job retry` maps
`failed → ready`, `parked → ready` and `stalled → running`; anything else is a conflict. `vowp job park`
and `vowp job cancel` stop a run and keep the evidence. Keeper commands (`nudge`, `restart`, `replan`,
`fail`) enqueue a request row that the next tick applies; the CLI never reaches into a running process.
These commands are implemented today, including the supervisor commands (`vowp up`,
`vowp status`) that apply keeper requests.

## Scheduling rules

1. **Only `ready` jobs are dispatched**, by priority and then age.
2. **Ownership locks.** No two running jobs may have overlapping `owns` globs. Conflicts wait. A fix job
   and the job it fixes are treated as one holder, so the fix does not deadlock the original.
3. **Slots and provider concurrency.** `[node].max_workers` (default 4) and `max_verifiers` (default 2)
   bound concurrency; `[providers.<p>].max_concurrent` bounds each provider and is lowered after rate
   limits.
4. **One worktree per job** on branch `job/<id>`. Workers run in that worktree. Verify, fix and merge jobs
   reuse the original's worktree and branch.
5. **Single writer to the default branch.** Only the supervisor's merge queue writes and pushes the
   default branch. A project that requires a `pushed` gate may publish `job/*` branches, but no job
   writes the default branch directly.

## The loops

| Loop | Tick | What it does | Bound |
|---|---|---|---|
| **L1 Supervisor** | 60 s | Reconcile runs, harvest outcomes, detect stalls, run gates, schedule verifies and merges, dispatch, trigger planner/keeper, run schedules, write the board and heartbeat. | Never stops; the watchdog restarts it. |
| **L2 Worker** | per job | Start the worker from its brief; if a run ends without a report, continue in the same session. | 2 continues per attempt, then the keeper. |
| **L3 Verify** | after a report | A separate verifier reads the brief, diff and evidence, and returns pass or concrete findings. | 3 fix rounds, then escalate. |
| **L4 Planner** | events + 30 min heartbeat | Decompose, reprioritise, review reports, write checkpoints. | One planner turn at a time. |
| **L5 Keeper** | stalls/failures + 20 min heartbeat | Diagnose, then nudge, restart, replan, park or fail. Covers planning when the planner's provider is limited. | One keeper turn at a time. |
| **L6 Failover** | on limit events | Hand off to the next rung with a capsule, probe the limited provider after its reset. | Never moves work to an unlisted paid provider. |
| **L7 Inbox** | continuous | Park only the asking job; answer unparks it. | One question never blocks unrelated work. |
| **L8 Watchdog** | systemd + cron | Keep L1 alive across crashes and reboots. | Heartbeat older than 180 s triggers a restart. |
| **L9 Schedules** | every tick | Fire due cron/interval rows: create a job, trigger a loop or run a command. | Missed runs collapse to one or are skipped. |

### The L1 tick

Each tick is wrapped so one failing step never aborts the rest. The order is:

0. Apply pause and CLI request rows (`retry`, `park`, `cancel`, `nudge`, `restart`, `replan`, `fail`,
   inbox answers, loop pause/resume, wake).
1. Reconcile open runs whose process disappeared; finalise them as `crash`.
2. Harvest finished runs: outcome, cost, ledger row, then the state-machine event.
3. Detect stalls from the last session event, last commit and output size.
4. Reconcile jobs by state; move `pending` jobs whose needs are done to `ready`.
5. Run command gates for `gating` jobs.
6. Create a verify job for `reviewing` jobs that do not have one.
7. Run at most one merge.
8. Dispatch ready jobs within slots, ownership and provider capacity.
9. Start a planner turn and a keeper turn when due.
10. Probe limited providers whose reset time has passed.
11. Fire due schedules.
12. Write `board.json` and, every 120 minutes, a checkpoint.
13. Update loop rows and rewrite `heartbeat`.

Nothing important lives only in memory: on restart the same steps reconcile from the database, session
logs and git.

## Gates

Gates run in the job worktree, in the order listed in `jobs.gates_json`. A failed gate does not stop the
remaining gates (except `report`, because the rest would be meaningless), so a fix prompt can list every
problem at once. Each result is a `gate_results` row plus a ledger entry.

| Gate | Passes when | Evidence |
|---|---|---|
| `report` | The report exists, parses, has the required fields, matches the job and branch, every commit exists, and `status: done`. | Parsed front-matter, missing fields, problems. |
| `cmd` | The command's exit code equals `expect_exit` and, if set, output matches `expect_pattern`. | Command, exit code, timeout flag, last 200 lines, duration. |
| `owns_respected` | The merge-base diff plus uncommitted files touch only `owns` (and project `always_allowed`). | Changed files and violations. |
| `pushed` | The remote branch exists and its sha equals the local branch. | `git ls-remote` output and local sha. |
| `review` | A verifier's latest report for this attempt says pass. | Verifier job, verdict, findings. |
| `script` | A custom checker exits as expected. Same mechanics as `cmd`, separate evidence kind. | Command, exit, tail, duration. |
| `deploy` | The deploy command succeeds **and** the post-deploy probe passes. | Versions before/after, probe evidence, rollback command. |
| `secret_scan` | No secret rule matches the added lines or secret-looking file paths. | Files scanned and rule names only — never the matched text. |

The **anti-pattern is banned**: a gate may not merely check that a file exists for anything that must
work. `test -f`, `ls`, `stat` and `cat`-only commands are rejected at validation time. A gate must execute
the thing and check the result. The `report` gate is the one exception, because its job is to parse
content, not merely find a file.

Default gate sets by kind:

| Kind | Default gates |
|---|---|
| `build` | `report`, `owns_respected`, `secret_scan`, `review` |
| `test`, `fix` | `report`, `owns_respected`, `secret_scan` |
| `docs`, `spec` | `report`, `owns_respected`, `secret_scan`, `review` |
| `verify`, `research`, `plan` | `report` |
| `merge` | `report` |
| `decision`, `promote`, `release` | none |

Default sets contain no `cmd` gates: crate- or project-specific commands are added per job. The
repository-wide checks (`[project].check_cmds`) belong to the merge queue, not to every worker.

## Bounds and no-progress rules

| Rule | Default |
|---|---|
| Tick | 60 s |
| Stalled: no new event and no new commit | 20 min |
| Hung before any output | 8 min |
| Automatic continues per attempt | 2 |
| Hang restarts before `stalled` | 3 |
| Fix rounds (`max_attempts`) | 3 |
| Same action repeated without new evidence | 3 → stop and escalate |
| Planner heartbeat / keeper heartbeat | 30 min / 20 min |
| Minimum gap between planner/keeper turns | 5 min |
| Gate timeout | 1800 s |
| Merge budget | 2700 s |
| Quota reset margin | +2 min |
| Probe cadence after a failed probe | 15 min |
| Unknown-failure streak → unavailable for 15 min | 3 runs |

Every number is configurable under `[loops.*]` or `[project]`.

## Merge queue

One merge runs at a time per project. It never merges a verify, fix or merge job directly; those finish
at their parent and hand control back.

1. Fetch the remote and record the current default-branch commit.
2. Rebase the job worktree onto the remote default branch.
   - **Conflict:** abort the rebase, list the conflicted files and create a `merge` job in the same
     worktree.
   - **Lockfile-only conflict:** take the main version, continue the rebase, regenerate the lockfile and
     make the single supervisor-authored commit (`chore: refresh lockfile (merge queue)`).
3. Run the repository-wide `[merge].check_cmds` (falling back to `[project].check_cmds`) with the merge
   timeout.
4. Run `secret_scan` over the rebased diff when `[merge].secret_scan` is true. A hit blocks the push.
5. Fast-forward the main checkout, push the default branch, delete the branch, remove the worktree,
   close the tmux window and write a ledger row.
6. A rejected push rewinds the main checkout and restarts from step 1, at most three times.

## The job contract: brief and report

A brief is the job's contract. The generated front-matter comes from the graph node; the body is written
by the planner or the person creating the job.

```markdown
---
job: example-graph
owns: ["crates/vowp-graph/**"]
must_not_touch: ["everything else"]
needs_done: [setup-protocol]
gates: [report, "cargo test -p vowp-graph", owns_respected, review]
done_when: "report has status: done and every gate passes"
---

# Goal

...
```

A report is the result. The front-matter is machine-checked; the body is for humans and verifiers.

```markdown
---
job: example-graph
status: done            # done | partial | blocked
branch: job/example-graph
commits: [abc1234]
tests: { command: "cargo test -p vowp-graph", passed: 31, failed: 0 }
files_changed: [crates/vowp-graph/src/lib.rs]
open_items: []
questions_for_owner: []
---
```

The `report` gate rejects `partial` and `blocked` reports as gate passes — they are honest states, but
the job is not done. A blocked report parks the job and puts its `questions_for_owner` into the inbox.

Next: [sessions and switching](sessions-and-switching.md), or the [jobs guide](../guides/jobs.md) for
practical examples.
