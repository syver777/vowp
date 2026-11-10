# The job map — jobs, loops and gates

VOWP turns work into a graph of jobs. A deterministic supervisor walks that graph every tick: it
reconciles live work, harvests outcomes, runs gates, asks an independent verifier when a job requires
review, merges through a single-writer queue and dispatches the next ready jobs. The loops are the
control plane; the graph is the plan.

This page is the map. The exact transition table, gate algorithms and loop timings are in
[loops.md](loops.md); the types are in [types.md](types.md); the user-facing introduction is
[Graph and loops](../concepts/graph-and-loops.md).

## The job contract

A job is one node in the graph and one unit of work. It has a contract before it starts and a result
when it stops. The contract is the brief; the result is the report and the recorded evidence.

| Part | Meaning | Where it lives |
|---|---|---|
| Brief | The contract: front-matter plus the work instructions | `.vowp/jobs/<job-id>/brief.md`, committed on the job branch |
| `owns` | Repo-relative globs the job may change; its write lock | Brief front-matter and the job row |
| `must_not_touch` | Everything outside `owns`; normally "everything else" | Brief front-matter |
| `needs` / `needs_done` | Jobs that must be `done` before this job becomes ready | `needs` edges in the graph; `needs_done` in the brief |
| `gates` | Ordered checks that decide whether the result counts | Brief front-matter and the job row |
| `done_when` | The plain-language completion condition | Brief front-matter |
| Report | The result: status, branch, commits, tests, changed files, open items and owner questions | `.vowp/jobs/<job-id>/report.md`, committed on the job branch |
| Branch and worktree | `job/<job-id>` in a separate worktree | Created at dispatch, removed after merge |
| Evidence | Gate results: command, exit code, output tail, duration, changed files | `gate_results` and the append-only ledger in the node database |

A brief's front-matter is machine-checked; the body tells the worker what to do and what not to do:

```yaml
---
job: <job-id>
owns: ["path/to/owned/**"]
must_not_touch: ["everything else"]
needs_done: [<dependency-id>]
gates: [report, "cargo test --workspace", owns_respected, secret_scan, review]
done_when: "the report has status: done and every gate passes"
---
```

Three rules make the contract hold:

- **One owner per path.** Dispatch refuses to start two jobs whose `owns` globs overlap, and
  `owns_respected` re-checks the diff. `reads` is not a lock. `[project].always_allowed` lists shared
  generated files (normally the lockfile) that any job may touch. If two jobs need the same file, add a
  `needs` edge or split the work; do not widen `owns` until the conflict disappears.
- **Commits stay on the job branch.** A worker commits to `job/<job-id>` in its worktree and does not
  write or push the default branch. The supervisor's merge queue is the only writer of the default
  branch, so `main` (or the configured default) stays linear.
- **One run per job.** A job has at most one attached run at a time. Continues and retries are new runs
  in the same job and session, not a second concurrent worker.

The `report` gate checks more than existence: the report must parse, carry the required fields, name
the job and branch, list commits that exist, and have `status: done`. A `partial` report is an honest
result but does not pass; a `blocked` report parks the job and moves its `questions_for_owner` into
the inbox.

## The lifecycle

The main path for a job that requires review is linear:

```
pending ──needs done──► ready ──dispatch──► running ──report──► gating
                                                                  │
                                                        gates pass │
                                                                  ▼
                                                             reviewing ──verify pass──► merging ──merge──► done
```

A job with no `review` gate goes from `gating` straight to `merging`. Gate, verify and merge failures
move the job to `fixing`; a fix returns it to `gating` (or to `merging`, when it repairs a merge).

| State | Meaning |
|---|---|
| `pending` | Waiting for its `needs` jobs. |
| `ready` | Dependencies are done; waiting for a slot, an ownership lock and a provider. |
| `running` | A worker run is attached. |
| `stalled` | The run stopped making progress; the keeper decides what to do. |
| `gating` | The job's command gates are executing. |
| `reviewing` | An independent verifier is checking the report, diff and evidence. |
| `fixing` | A fix or merge job is repairing the original in the same worktree. |
| `merging` | The merge queue is rebasing, checking and merging. |
| `done` | Merged, or finished successfully for a verify, fix or merge job. Terminal. |
| `failed` | Retries are exhausted or the keeper failed it; it can be retried or replanned. Not terminal. |
| `parked` | Waiting for a human, a provider or a manual decision. Not terminal. |
| `replanned` | The keeper handed the work back to the planner. Terminal. |
| `cancelled` | A human cancelled it; branch and worktree are kept for inspection. Terminal. |

The transitions follow the job's evidence:

- `pending` becomes `ready` when every `needs` job is `done`.
- `ready` becomes `running` on `Dispatched`.
- `running` becomes `gating` when a valid report appears. If the run ends without a report, L2
  continues in the same session. A crash is continued the same way. A silent hang is restarted with a
  fresh process. After the bounds, the job becomes `stalled` and the keeper takes over.
- `gating` becomes `reviewing` when the command gates pass and the job has a `review` gate; otherwise
  it becomes `merging`.
- `reviewing` becomes `merging` on a verifier pass. A verifier failure creates a fix job and moves the
  original to `fixing`.
- `merging` becomes `done` on a successful merge. A conflict or a repository-wide check failure creates
  a bounded fix or merge job and moves the original to `fixing`.
- `fixing` returns to `gating` after a fix, and to `merging` after a merge repair.

Failures are bounded loops, not silent retries: a job gets a bounded number of automatic continues,
hang restarts and fix rounds. When the fix rounds are exhausted, `on_exhausted` applies: `escalate`
creates a decision job and an inbox item, `park` parks the job, and `fail` fails it and wakes the
keeper. A human question parks only the asking job; everything else keeps running. A credits or auth
block parks every job on that provider.

A human can park any non-terminal job, cancel it, or retry a `failed`, `parked` or `stalled` job. A
retry returns a failed job to `ready` with its counters cleared, a parked job to `ready`, and a
stalled job to `running`.

## Dispatch and slots

The supervisor dispatches only `ready` jobs. Each tick it builds the runnable set from the graph and
filters it by:

- every `needs` job being `done`;
- a free worker or verifier slot — `[node].max_workers` (default 4) and `[node].max_verifiers`
  (default 2);
- provider capacity — `[providers.<name>].max_concurrent` minus open runs, reduced while a rate limit
  is active;
- no ownership overlap with a job that holds a lock (`running`, `stalled`, `gating`, `reviewing`,
  `fixing` or `merging`);
- no pause or active provider block.

Choices are ordered by priority (higher first), then by age. Each choice becomes one `Dispatched`
event, one worktree on branch `job/<job-id>`, one session, one tmux window named `job-<job-id>` and one
runner thread. The window is a read-only follower of the session log: a view, never a control channel.
The supervisor never sends keystrokes to a harness.

A job's worktree lives outside the project checkout (`<worktree_root>/<job-id>`, default
`<repo>/../<repo-name>-wt/<job-id>`) and is removed after a successful merge. Verify, fix and merge
jobs reuse the original job's worktree and branch; they never create a second worktree for the same
change. Nothing important lives only in memory: after a crash or restart, the tick reconciles open runs
and continues from database rows, session logs and git.

## Gates

Gates turn a claim into evidence. They run in the job worktree, in the order listed in the job, and
each evaluation writes a `gate_results` row plus a ledger row. Long output is spilled to a blob; the
evidence keeps the last 200 lines.

| Gate | Passes when | Evidence |
|---|---|---|
| `report` | The report exists, parses, has the required fields, matches the job and branch, every listed commit exists, and `status: done` | Parsed front-matter, missing fields, problems |
| `cmd` | The command exits with the expected code and, if set, matches the expected output pattern | Command, exit code, timeout flag, output tail, duration |
| `script` | A custom checker exits as expected; same mechanics as `cmd` | Command, exit code, tail, duration |
| `owns_respected` | The merge-base diff and uncommitted files touch only `owns` (plus `always_allowed`) | Changed files and violations |
| `pushed` | The remote branch exists and its commit equals the local branch | Remote and local shas |
| `review` | An independent verifier passed the latest attempt | Verifier job, verdict, findings |
| `deploy` | The deploy command succeeds and the post-deploy probe passes | Versions before and after, probe output, rollback command |
| `secret_scan` | No secret rule matches the added lines or a secret-looking file path | Rule name, path and line; never the matched text |

Command gates are deterministic: a formatter check, clippy with warnings denied, a test command, or
any custom checker. The runner executes the command in the worktree, merges stdout and stderr, records
the exit code and kills the process group at the timeout. A gate result is evidence, not a claim: the
runner executes the check itself, so a report that says tests passed does not satisfy a `cmd` gate.

A gate may not merely check that a file exists for something that must work. `test -f`, `ls`, `stat`,
`cat`, `true` and `:` are rejected at validation time; the `report` gate is the exception because
parsing content is its purpose. Evaluation continues after a failure, so one fix prompt can list every
finding; a failed `report` gate short-circuits the rest because the other gates would be meaningless.
`review` is not evaluated by the command runner; a verifier satisfies it.

Default gate sets depend on the job kind:

| Kind | Default gates |
|---|---|
| `build` | `report`, `owns_respected`, `secret_scan`, `review` |
| `test`, `fix` | `report`, `owns_respected`, `secret_scan` |
| `docs`, `spec` | `report`, `owns_respected`, `secret_scan`, `review` |
| `verify`, `research`, `plan`, `merge` | `report` |
| `decision`, `promote`, `release` | none; promotion has its own guards |

Default sets contain no command gates: a build, test or project check is added per job. The
repository-wide checks (formatting, clippy, tests) belong to the merge queue and run again there.

## The independent verifier

A `review` gate is satisfied by a separate job, not by the worker and not by the command runner. When
a job reaches `reviewing`, the supervisor creates a verify job with kind `verify`, role `verifier`,
`owns: []`, gates `[report]` and a `verifies` edge to the original.

The verifier runs in its own session, in the original's worktree, under a read-only permission preset.
It reads the brief, the diff against the default branch and the gate evidence, and it runs the
commands itself rather than trusting the worker's report. Its report carries `verdict: pass` or
`verdict: fail` and, on failure, concrete findings: file, command, expected, actual.

It cannot edit the change: it has no owned paths, it runs in a separate session under a read-only
policy, and its own work is judged by a `report` gate. Its job is to judge the change, not to repair
it.

A pass moves the original to `merging`. A fail creates a fix job with the findings, a `fixes` edge and
the original's `owns` and worktree; the original moves to `fixing` and later re-runs its command gates.
A fail without findings is treated as `partial`: the verifier is retried once, then the original is
parked with a question. If the verifier itself fails or cannot produce a verdict, the original is
parked, never merged. Fix rounds are bounded by `max_attempts` (default 3); exhaustion applies
`on_exhausted`.

## The merge queue

The merge queue is the only writer of the default branch. One merge runs at a time per project, on a
runner thread. Verify, fix and merge jobs never enter the queue; they finish at their parent and hand
control back.

1. Fetch the remote and record the current default-branch commit.
2. Rebase the job worktree onto the remote default branch. A conflict aborts the rebase, lists the
   conflicted files and creates a merge job in the same worktree. A conflict only in the project's
   lockfile takes the default-branch version, continues the rebase, regenerates the file with its own
   tool, and makes the one supervisor-authored lockfile commit.
3. Run the repository-wide checks — `[merge].check_cmds`, falling back to `[project].check_cmds` —
   within the merge timeout. These are the formatting, clippy, test or custom checks that must hold
   for the whole repository.
4. Run the secret scan over the rebased diff when enabled. A hit blocks the push.
5. Fast-forward the main checkout, push the default branch, delete the job branch, remove the
   worktree, close the job window and write a ledger row.
6. A rejected push because the remote moved rewinds the main checkout and restarts from step 1, at
   most three times.

The result is `MergeSucceeded`, `MergeConflict` or `MergeChecksFailed`. A conflict or a failed check
creates a bounded fix or merge job; it never bypasses the queue. The main checkout is fast-forward
only, so the default branch stays linear.

## Loops

### The supervisor tick

The supervisor is a deterministic program started by `vowp up`. L1 runs every 60 seconds
(`[node].tick_secs`) and never stops by itself; the watchdog restarts it if the process or heartbeat
dies. Each step is isolated: one failing step is logged and the rest still run. Long work — gates,
merges, verifier turns, planner and keeper turns, harness runs — executes on runner threads, and L1
only starts them and harvests their results.

One tick, in order:

0. Apply the pause flag and CLI requests: retry, park, cancel, nudge, restart, replan, fail, inbox
   answer, loop pause or resume, and wake.
1. Reconcile open runs: a run whose process is gone is finalised as `crash`; an orphan process is
   killed, then handled like any crash.
2. Harvest finished runs: record the outcome, append the ledger row, then deliver the job event —
   report ready, failure, limit or rate limit.
3. Detect stalls from the last session event, the last commit and the output size.
4. Reconcile jobs by state; move `pending` jobs whose needs are done to `ready`.
5. Run command gates for `gating` jobs.
6. Create a verify job for each `reviewing` job that does not have one.
7. Run at most one merge.
8. Dispatch ready jobs within slots, ownership and provider capacity.
9. Start a planner turn and a keeper turn when due, at most one of each.
10. Probe limited providers whose reset time has passed.
11. Fire due schedules.
12. Write the board snapshot and, at the checkpoint interval, a checkpoint file.
13. Update the loop rows and rewrite the heartbeat.

When the pause flag is set, the tick performs only requests, open-run reconciliation, the board and
the heartbeat; dispatch, gates, merges and planner or keeper turns wait until the node is resumed.

A request is a row written by the CLI; the next tick consumes it and applies it through the state
machine. This is why the CLI never reaches into a running process and why a keeper command is safe to
issue while the supervisor is down.

### The nine loops

| Loop | Cadence | Responsibility |
|---|---|---|
| L1 supervisor | Every 60 s | Reconcile, harvest, gate, verify, merge, dispatch, trigger the planner and keeper, probe, schedule, write the board and heartbeat. |
| L2 worker | Per job run | Start the worker from its brief. If a run ends without a report, continue in the same session. |
| L3 verify | After a report | Run the independent verifier; pass the change or return concrete findings. |
| L4 planner | Events plus a 30-minute heartbeat | Own the graph: decompose work, write briefs, set priorities, review reports, write checkpoints. One turn at a time. |
| L5 keeper | Stalls and failures plus a 20-minute heartbeat | Diagnose a stuck job and nudge, restart, replan, park or fail it. Covers planning while the planner's executor is limited. One turn at a time. |
| L6 failover | On limit events | Move a session to the next profile rung with a capsule; probe the limited provider and switch back when it answers. |
| L7 inbox | Continuous | Ask the human; only the asking job waits. Answering resumes it. |
| L8 watchdog | systemd plus a 5-minute check | Keep L1 alive across crashes and reboots; a heartbeat older than 180 s triggers a restart. |
| L9 schedules | Every tick | Fire due cron or interval rows: create a job, trigger a loop or run a command. |

Full loop definitions, step order, guards and timings are in [loops.md](loops.md).

### Planner and keeper

The planner and keeper are sessions with fixed roles, one per project, one turn at a time. The
planner owns the graph: it decomposes work, writes briefs, adjusts priorities, reviews reports against
evidence and writes checkpoint summaries. The keeper handles stalls and failures: it reads the job's
logs and decides whether to nudge, restart, replan, park or fail. When the planner's executor is
limited, the keeper takes over the planning duties using the planner's capsule; the planner resumes
after the provider probe succeeds.

Both act through the same validated CLI as a human. `vowp job add` and `vowp job update` change the
graph, and `vowp inbox answer` answers a question. Keeper actions (`vowp job nudge`,
`vowp job restart`, `vowp job replan`, `vowp job fail`) write request rows that the next tick
applies. Neither the planner nor the keeper edits the database by hand or merges.

### Failover

L6 handles provider limits as classified by the adapters. A quota limit hands the affected session to
the next rung of its profile with a capsule, then probes the limited provider after the reset and
switches back when it answers. Credits exhausted or an auth failure parks every job on that provider
and opens one inbox item; it never silently moves work to another paid provider. A rate limit backs
off with jitter, lowers that provider's concurrency and delays the interrupted continue run instead of
counting it as a failure. A context overflow writes a capsule and starts a fresh session on the same
harness. Repeated unknown failures mark the provider unavailable for 15 minutes.

Only the rungs listed in the profile are used. The full algorithms are in [loops.md](loops.md) §8;
the product view is [Providers and failover](../concepts/providers-and-failover.md).

## Schedules and Work → Production

### Schedules

L9 turns schedule rows into work. A schedule is a five-field cron expression or an interval such as
`every 30m`, with UTC or a fixed offset as its timezone. A due schedule creates a job from a template,
marks a loop due, or runs a command without a shell. Missed runs while the node was down either run
once or are skipped, according to the schedule's policy. An idempotency key prevents a double fire,
and the next run time advances before the action, so a crash can lose at most one firing.

`vowp schedule` adds, lists, pauses, resumes and removes rows. The full rules are in
[loops.md](loops.md) §11; the practical guide is [Schedules](../guides/schedules.md).

### Work → Production

A merge puts a change on the default branch: that is Work. Production is a separate, guarded
promotion. `vowp promote <change> --to <env>` runs the environment guards (clean tree, HEAD equals
remote, build, no active jobs), snapshots the serving version, checks that the candidate diff matches
the intended change, asks for approval when the environment requires a human, deploys, probes the
result, and records the release, the rollback command and a release-log line.
`vowp rollback <release>` redeploys the banked serving copy.

A refused deploy is reported and staged; it is never worked around. The environments and guards are
configuration — `[env.work]` and `[env.prod]` in `vowp.toml` — not code paths. See
[plan.md](plan.md) §9, [Work to Production](../concepts/work-to-production.md) and the commands in
[cli.md](cli.md).

## Sessions and capsules

Every run belongs to a universal session: an append-only event log with a stable id. A job keeps its
session across continues and retries; a verifier or fix job has its own session. Session content lives
in the log and its blobs; the database is an index that can be rebuilt.

When a session moves to another executor — a quota limit, context pressure, or a user request — the
switch protocol writes a capsule: a structured handoff with intent, decisions, state, files, gotchas
and next steps. The target starts from the capsule plus the last turns, and the switch is recorded as
lineage. The full format, switch protocol and export/import are in [sessions.md](sessions.md); the
product view is [Sessions and switching](../concepts/sessions-and-switching.md).

## Related documents

| Document | Read it for |
|---|---|
| [plan.md](plan.md) | The design summary: principles, concepts and the wider architecture. |
| [architecture.md](architecture.md) | Components, threading and on-disk layout, and `vowp.toml`. |
| [loops.md](loops.md) | The exact job state machine, L1 tick, gates, merge queue, failover and schedules. |
| [sessions.md](sessions.md) | Event logs, capsules, the switch protocol and export/import. |
| [types.md](types.md) | Job, gate, brief, report and session types with their serde shapes. |
| [cli.md](cli.md) | Every `vowp` command and its output. |
| [schema.sql](schema.sql) | The database tables behind jobs, runs, gates, loops and the ledger. |
| [decisions.md](decisions.md) | Places where the detailed specification clarifies the design summary. |
| [Graph and loops](../concepts/graph-and-loops.md) | A shorter, task-oriented introduction. |
| [Writing jobs](../guides/jobs.md) | Practical briefs, gates and reports. |
