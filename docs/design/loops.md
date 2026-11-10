# loops.md — L1–L9, the job state machine, gates, merge queue, failover

Behavioural spec for `vowp-core::{loops, gates, ledger, schedules}` and everything that reads their rows. Vocabulary is
`types.md` / `schema.sql`; pure logic (transitions, dispatch, stall judging) is `crates/vowp-graph.md`; adapters are `adapters.md`;
sessions/switch are `sessions.md`.

## 1. Vocabulary and the L1 tick

| Term | Meaning |
|---|---|
| tick | one pass of L1, every `[node].tick_secs` (60 s). Never blocks on a long operation: gates, merges, verifies, planner turns and harness runs execute on **runner threads** (`std::thread`) that report completion through an `mpsc` channel + DB rows; L1 only polls them. |
| request | a CLI → supervisor message: a `kv` row with key `req:<ulid>` and JSON value `{"kind":"<snake_case request kind>","payload":{…}}` exactly as defined by `vowp_core::requests::Request` (crates/vowp-core.md §2: kinds `retry_job park_job cancel_job resolve_job nudge_job restart_job replan_job fail_job answer_inbox pause_loop resume_loop run_loop wake`); consumed (deleted) at tick step 0. |
| outcome | `RunOutcome` of a finished run (types.md §9). |
| rung | index into a profile's ladder. |

### 1.1 L1 tick, exact order (`Supervisor::tick(&mut self)`)

Each step is wrapped so one failing step logs, records `loops.last_status`, bumps `fail_streak`, and does not skip later steps.
All time from `Clock`.

0. **Pause + requests.** If `kv['paused']='1'`, do only steps 0, 1, 12, 13 (no dispatch, no merges, no planner). Read and apply `req:*` rows
   per the `Request` kinds of crates/vowp-core.md §2 (kv value `{"kind","payload"}`), **every** kind handled:

   | `kind` | Applied as |
   |---|---|
   | `retry_job` | state-dependent: `failed` → `JobEvent::Retry` (T32, resets `attempts`, `continues`, `hang_restarts`), `stalled` → `KeeperRestart` (T11); otherwise rejected (logged) |
   | `resolve_job` | `Resolved` (T31) |
   | `park_job` | `HumanPark` (T30) with `parked_reason=Manual`, note = reason |
   | `cancel_job` | `Cancel` (T33) |
   | `nudge_job` | `KeeperNudge` (T10): start a normal continue run (no extra text) |
   | `restart_job` | `KeeperRestart` (T11) |
   | `replan_job` | `KeeperReplan` (T12/T34) |
   | `fail_job` | `KeeperFail` (T13); note = reason |
   | `answer_inbox` | `l7::on_answer` (§9: resolves the item, sends `Resolved` to the parked job) |
   | `pause_loop` / `resume_loop` | set `loops.state` (never `l1-supervisor`) |
   | `run_loop` | mark that loop due now (`next_run_at = now`) |
   | `wake` | no-op; only ends the idle wait early |

   An event the state machine rejects (`InvalidTransition`) is logged and dropped, never retried. Delete each row after applying; log a ledger `state` entry. Test: `request_rows_apply_every_kind_end_to_end` (core).
1. **Reconcile open runs.** For every `runs` row with `ended_at IS NULL` owned by this node: if no live runner thread has it **and** its
   `pid` is not alive (`kill(pid,0)` equivalent via `/proc/<pid>` existence check; no `unsafe`) → the supervisor restarted or the child
   vanished. Kill the process group if the pid is alive but unowned (orphan), finalise the run with `outcome=crash`, `detail="orphaned at supervisor start"`,
   and let step 2 handle it like any crash. Also free `workers` rows whose `run_id` is gone.
2. **Harvest finished runs.** For each run finished since the last tick (runner thread reported, or step 1): write `ended_at/exit_code/outcome/cost/...`
   (cost from `RunSummary`), append a `ledger` `cost` row (`usd`, tokens, `cost_source`), classify `outcome`:
   - `ok` → role-specific follow-up (§3, §5): worker run ok but no valid report ⇒ `JobEvent::RunEndedNoReport`; report present ⇒ `ReportReady`.
   - `limit|credits|auth|rate|context` → L6 (§8) before anything else; the job event is `RunEndedNoReport` only if the run is not rerouted by L6.
   - `crash|error|killed|stalled` → `RunEndedNoReport` (L2 continue) and update the provider's unknown-failure streak (§8.6).
3. **Stall detection.** For each open run (role worker/verifier/planner/keeper) call `StallJudge::judge` with `last_event_at` = latest session event ts (via the
   session index), `last_commit_at` = `git log -1 --format=%ct job/<id>` for job runs, `output_bytes` = size of the run's raw output file. On
   `Stalled{silent}`: stop the run (`AdapterRun::stop`) and emit `JobEvent::Stalled{silent}`; planner/keeper runs are simply stopped and retried next heartbeat.
4. **Job reconciliation by state** (the only place states change besides requests and harvest):
   - `pending`: if `Graph::ready_jobs()` contains it → `NeedsSatisfied`.
   - `running`: nothing to do (runner thread owns it); but if a valid report file appeared while the run is still going, wait for run end (never gate a live run).
   - `gating`: step 5. `reviewing`: step 6. `merging`: step 7. `fixing`: wait for the fix/merge job (its `FixDone` arrives from step 2/5 of that job).
   - `ready`: step 8. `stalled|failed|parked`: L5 wakeups (step 9) or human requests; nothing automatic except §9 resolution.
5. **Gates.** For each job in `gating` with no gate runner active: spawn a gate runner thread (§4). On completion: `GatesPassed` or `GatesFailed`; write `gate_results` rows + ledger `gate`.
6. **Verify scheduling (L3).** For each job in `reviewing` with no open `verify` job: create the verify job (§5.1). A verify job runs the normal path `running → gating → done` (T3, T14; gates `[report]`) and is **never merged**; on T14 the caller parses its report (`l3::parse_verdict`), writes the `review` `GateResult` (evidence `Review{verify_job, verdict, findings}`) for the original and delivers `VerifyPassed|VerifyFailed` to it. A failed verify job (T36/T13) delivers `VerifierFailed` (T37).
7. **Merge queue.** If no merge is running and some job is `merging`: pick the oldest (`state_changed_at`) and spawn the merge runner (§6). Result: `MergeSucceeded|MergeConflict|MergeChecksFailed`.
8. **Dispatch.** Build `DispatchCaps` (free slots from `workers`, provider capacity from `[providers.*].max_concurrent` minus open runs minus active `limits.concurrency_cap`, `limited` map from `limits`, profiles from config); `plan_dispatch(&graph,&running,&caps)`;
   for each choice: `JobEvent::Dispatched` (state `running`), create worktree (`git worktree add <worktree_root>/<id> -b job/<id> <default_branch>`; **verify, fix and merge jobs never create one: they run in the original's worktree**, whose path/branch are copied into their `worktree`/`branch` columns at creation), create tmux window `job-<id>`, write brief, spawn the runner (§3).
9. **Planner/Keeper triggers (L4/L5).** Evaluate `planner_due` / `keeper_due` (§7) and start at most one planner turn and one keeper turn.
10. **L6 probes.** For each `limits` row with `kind IN (quota,rate,unavailable)` and `now >= resets_at` (or `now >= last_probe_at + 15 min` after a failed probe) and no probe run open → start a probe (§8.5).
11. **L9 schedules.** Run due schedules (§11).
12. **Board and reports.** Write `~/.vowp/board.json` (jobs by state, slots, current planner/keeper executor, limits, cost totals, inbox count) every tick; every `LoopBounds.checkpoint_minutes` (default 120) write a checkpoint file `<project>/.vowp/checkpoints/<ts>.md` (what finished/failed, cost from receipts, limit/failover events). `vowp board` recomputes the same snapshot from the DB (`BoardSnapshot::collect`); `board.json` is only a convenience copy.
13. **Loop rows + heartbeat.** Update `loops` rows (`last_run_at,next_run_at,last_status,fail_streak`) and write `~/.vowp/heartbeat` (the unix time as text, written to a temp file then renamed). Also `kv['supervisor_pid']`.

Loop rows created on first `vowp up` (`name`, `kind`, `interval_secs`): `l1-supervisor` L1 60 · `l2-worker` L2 0 · `l3-verify` L3 0 · `l4-planner` L4 1800 · `l5-keeper` L5 1200 · `l6-failover` L6 0 · `l7-inbox` L7 0 · `l8-watchdog` L8 300 · `l9-schedules` L9 60. A `paused` loop is skipped by its step (L1 itself cannot be paused except via `kv['paused']`).

## 2. The job state machine (`vowp_graph::transition` implements this table exactly)

Defaults: `MAX_CONTINUES=2` (per attempt), `MAX_HANG_RESTARTS=3`, `max_attempts = job.loop.max_attempts` (3). L1 reads the first two from
`LoopBounds.max_continues` / `max_hang_restarts` (these defaults when unset); `attempts` counts **fix rounds started**;
`continues` is reset to 0 whenever a new attempt or a retry begins. "Counter effects" are applied by the caller when the effect is listed; guards read the **current** job row.
Every transition also writes one ledger row (`kind=state`, summary `"<job>: <from> -> <to> (<event>)"`) — not listed as an effect.

Events are `JobEvent` variants (crates/vowp-graph.md §3). `Exhausted(x)` means `attempts >= max_attempts` and uses `on_exhausted`:
**Escalate** ⇒ `Failed` + `CreateDecisionJob` + `NotifyInbox(Decision)` + `WakeKeeper`, **Park** ⇒ `Parked(Exhausted)` + `NotifyInbox(Question)`, **Fail** ⇒ `Failed` + `WakeKeeper`.

| # | From | Event | Guard | To | Effects |
|---|---|---|---|---|---|
| T1 | pending | `NeedsSatisfied` | — | ready | — |
| T2 | ready | `Dispatched` | — | running | `CreateWorktree` (**kind Verify/Fix/Merge: no new worktree** — they reuse the target job's worktree/branch found through their `verifies`/`fixes` edge; the caller skips the effect), `StartRun{Start}` (kind Fix/Merge jobs: `StartRun{Fix}`; if the job already has a session and `continues>0` or a prior run: `StartRun{Continue}`) |
| T3 | running | `ReportReady` | — | gating | `RunGates` |
| T4 | running | `RunEndedNoReport` | `continues < 2` | running | `IncrementContinues`, `StartRun{Continue}` |
| T5 | running | `RunEndedNoReport` | `continues >= 2` | stalled | `WakeKeeper` |
| T6 | running | `Stalled{silent:false}` | `continues < 2` | running | `StopRun`, `IncrementContinues`, `StartRun{Continue}` |
| T7 | running | `Stalled{silent:false}` | `continues >= 2` | stalled | `StopRun`, `WakeKeeper` |
| T8 | running | `Stalled{silent:true}` | `hang_restarts < 3` | running | `StopRun`, `IncrementHangRestarts`, `StartRun{Start}` (fresh process, same session) |
| T9 | running | `Stalled{silent:true}` | `hang_restarts >= 3` | stalled | `StopRun`, `WakeKeeper` |
| T10 | stalled | `KeeperNudge` | — | running | `ResetContinues`, `StartRun{Continue}` |
| T11 | stalled | `KeeperRestart` | — | running | `ResetContinues`, `StartRun{Start}` |
| T12 | stalled | `KeeperReplan` | — | replanned | `WakePlanner` |
| T13 | stalled | `KeeperFail` | — | failed | `NotifyInbox(Notice)`; for kind ∈ {Fix, Merge, Verify} also `NotifyParent` (delivers the child-failure event of the table below) |
| T14 | gating | `GatesPassed` | job kind ∈ {Fix, Merge, Verify} | done | `NotifyParent`: parent = `fixes` dst (Fix/Merge) or `verifies` dst (Verify). Delivers `FixDone{kind}` for Fix/Merge; for Verify the caller parses the verify report and delivers `VerifyPassed` (`verdict: pass`) or `VerifyFailed` (`verdict: fail`) to the original (T19–T21). No `CleanupWorktree` (the parent owns the worktree). These kinds **never** reach `reviewing`/`merging` |
| T15 | gating | `GatesPassed` | kind ∉ {Fix, Merge, Verify} and job has a `review` gate | reviewing | `StartVerify` |
| T16 | gating | `GatesPassed` | kind ∉ {Fix, Merge, Verify}, no `review` gate | merging | `EnqueueMerge` |
| T17 | gating | `GatesFailed` | kind ∉ {Fix, Merge, Verify}, `attempts < max_attempts` | fixing | `IncrementAttempts`, `CreateFixJob{Fix}` |
| T18 | gating | `GatesFailed` | kind ∉ {Fix, Merge, Verify}, `Exhausted` | per `on_exhausted` | as above |
| T19 | reviewing | `VerifyPassed` | — | merging | `EnqueueMerge` |
| T20 | reviewing | `VerifyFailed` | `attempts < max_attempts` | fixing | `IncrementAttempts`, `CreateFixJob{Fix}` |
| T21 | reviewing | `VerifyFailed` | `Exhausted` | per `on_exhausted` | as above |
| T22 | fixing | `FixDone{kind:Fix}` | — | gating | `ResetContinues`, `RunGates` (all gates re-run, incl. a new verify) |
| T23 | fixing | `FixDone{kind:Merge}` | — | merging | `EnqueueMerge` |
| T24 | fixing | `FixFailed` (the fix/merge job ended `failed`) | — | failed | `NotifyInbox(Notice)`, `WakeKeeper` |
| T25 | merging | `MergeSucceeded` | — | done | `CleanupWorktree`, `UnblockDependents` |
| T26 | merging | `MergeConflict` | `attempts < max_attempts` | fixing | `IncrementAttempts`, `CreateFixJob{Merge}` |
| T27 | merging | `MergeChecksFailed` | `attempts < max_attempts` | fixing | `IncrementAttempts`, `CreateFixJob{Fix}` |
| T28 | merging | `MergeConflict` / `MergeChecksFailed` | `Exhausted` | per `on_exhausted` | as above |
| T29 | pending, ready, running, stalled, failed | `Blocked{reason}` | — | parked | `StopRun` (if running), `NotifyInbox(Question)` for Question/Auth/Credits/ProviderBlocked, `SetParkedReason` |
| T30 | any non-terminal | `HumanPark` | — | parked | `StopRun` (if running), `SetParkedReason(Manual)` |
| T31 | parked | `Resolved` | — | ready | `ClearParkedReason`, `ResetContinues` |
| T32 | failed | `Retry` | — | ready | `ResetCounters` (attempts, continues, hang_restarts = 0) |
| T33 | any non-terminal | `Cancel` | — | cancelled | `StopRun` (if running). The worktree and branch are **kept** for inspection (no `CleanupWorktree`) |
| T34 | parked, failed | `KeeperReplan` | — | replanned | `WakePlanner` |
| T36 | gating | `GatesFailed` | kind ∈ {Fix, Merge, Verify} (**never creates a fix-of-fix**) | failed | `NotifyParent`: Fix/Merge → parent receives `FixFailed` (T24); Verify → parent receives `VerifierFailed` (T37); `NotifyInbox(Notice)` |
| T37 | reviewing | `VerifierFailed` (verify job ended `failed`, or its report had no usable verdict twice) | — | parked | `SetParkedReason(Question)`, `NotifyInbox(Question)` ("verifier could not produce a verdict for <job>"); `Resolved` later returns the job to `ready` |
| T35 | running | `RunRateLimited{until}` | — | running | `DelayRun{purpose: Continue, until}` (no counter changes; the run that hit the 429 does **not** consume `continues`; the caller stores `kv['delay:<job>']=until` and L1 starts the continue run when `now >= until`) |

Terminal states: `done`, `replanned`, `cancelled`. **Any (state,event) pair not listed returns `GraphError::InvalidTransition`**; in particular no event leaves a terminal state.
`NeedsSatisfied` is only emitted by L1 step 4; `Retry` on a non-`failed` job is invalid. The CLI `vowp job retry` therefore maps by state: `failed` → `Retry` (T32), `parked` → `Resolved` (T31), `stalled` → `KeeperRestart` (T11); any other state exits 4.
Differences from the design summary §6.3: D1 (`gating`, `cancelled`), D5 (fix jobs on the original branch, no verify of their own), and `fixing → gating` (T22) instead of `fixing → reviewing`: the fix must pass the cheap
command gates again before another verifier is spent (recorded as D17 in decisions.md).

Fix/merge jobs are normal `jobs` rows (`kind` fix/merge, `fixes` edge `fix -> original`, `role` = the original's role, `owns`/`worktree`/`branch` copied from the original so ownership locks see them as the same owner; they are **not** counted as an overlapping lock against the original, which is in `fixing`: `OwnsLocks` treats `fixes` pairs as one holder).
Their `gates` = `[report, cmd gates of the original]` (no `review`, no `owns_respected` is needed but included).

## 3. L2 — the worker loop

1. **Start.** Dispatch (step 8) resolves the executor from `Profile::pick` (rung), creates a session (`SessionOps::create_session`, role worker, title = job id), writes the brief if missing,
   opens window `job-<id>` running `vowp __window job <id>` (a read-only pretty follower of the job's latest session), spawns a runner thread calling `SessionOps::run_turn` with the **start prompt**:

   ```text
   You are the worker for job {job_id} of project {project}. Your working directory is the job worktree {worktree} on branch {branch}.
   Read AGENTS.md and your brief at {brief_path} first, and do exactly what the brief says. Change only the paths in `owns`.
   When finished, commit on this branch (do not push), run the gates listed in the brief, and write your report to {report_path}
   with the front-matter required by the brief. Do not stop before the report exists. If you need the owner, write the question under
   questions_for_owner and set status: blocked.
   ```
2. **Continue nudge** (T4/T6/T10, `RunPurpose::Continue`, same session, `--resume` when the harness supports it, else capsule seed):

   ```text
   You stopped before writing your report ({report_path}). Continue the job from where you left off: finish the work, run the gates,
   commit, and write the report. Do not announce next steps — do them. ({n} of 2 automatic continues used; after that a supervisor will diagnose.)
   ```
   If the provider changed since the last run (limit failover), the turn runs on a fresh native session seeded with the capsule (§8.4).
3. **Fix prompt** (T17/T20/T27, `RunPurpose::Fix`, the fix job's brief is generated with the findings):

   ```text
   Job {parent_id} failed its checks (round {attempt} of {max}). Findings:
   {findings as a numbered list: gate name, command, exit code, last 40 lines of output / verifier findings}
   Fix exactly these problems in the same worktree, re-run the failing commands, commit, and update {report_path}. Do not widen scope.
   ```
4. **Bounds.** Continues ≤ 2 per attempt (`jobs.continues`); then the job is `stalled` and the keeper (L5) is woken. Hung-before-first-output restarts ≤ 3 (`hang_restarts`). Fix rounds ≤ `loop.max_attempts` (3).
5. **After a worker run ends** (step 2 of the tick): if `report_path` exists in the worktree and parses (`Report::parse`) with `status ∈ {done, partial, blocked}`: `status=blocked` → `Blocked{Question}` (questions_for_owner → inbox body) else `ReportReady`. Report missing or unparsable → `RunEndedNoReport`.
6. A **report that predates the current attempt** (mtime < run start of this attempt) is ignored; fix attempts require the report be rewritten.

## 4. Gates

Gate evaluation runs in a runner thread, in the job worktree, **in the order of `jobs.gates_json`, then the project repo-wide checks are NOT run here (they belong to the merge queue)**. Evaluation continues after a failure (so the fix prompt lists all findings) except that a failed `report` gate short-circuits the rest (nothing else is meaningful). `review` gates are not evaluated here: they are satisfied by T19/T20 (§5); in `gating` they are skipped (passed=true, evidence `Review{verify_job:null}` is **not** written).
Each evaluation writes one `gate_results` row (types.md §8 `GateResult`) and one ledger `gate` row; long output is spilled to a blob (sha256) referenced in `output_blob`. `Evidence.tail` = last 200 lines.

| Gate | Algorithm | Passes when |
|---|---|---|
| `report` | 1. read `report_path`; 2. `Report::parse`; 3. `missing_fields(fields)`; 4. check `front_matter.job == job.id`, `branch == job.branch`; 5. every `commits[]` sha exists (`git cat-file -e`); 6. status ∈ {done}; (`partial|blocked` fail the gate with problem text) | no missing fields, no problems |
| `cmd` | `sh -c <run>` in `worktree/<cwd?>`, stdout+stderr merged, kill the process group at `timeout_secs` (1800) → `timed_out=true`, fail | `exit_code == expect_exit` **and** (if set) output matches `expect_pattern` |
| `script` | identical to `cmd`; evidence kind `Script`. Used for custom checkers | same |
| `owns_respected` | `git diff --name-only <default_branch>...job/<id>` (merge-base diff) plus uncommitted files from `git status --porcelain`; each path must satisfy `path_allowed(owns, project.always_allowed, path)` | `violations` empty |
| `pushed` | `git ls-remote <remote> refs/heads/<branch>`; compare the sha with `git rev-parse <branch>` | remote ref exists and equals local sha |
| `review` | satisfied by a `verify` job with `verdict: pass` (§5); evidence `Review{verify_job,verdict,findings}` | the latest verify job for this attempt passed |
| `deploy` | 1. run `run` (cwd = env checkout); 2. record `version_before/after` if the env defines `probe`; 3. run the `probe` CmdGate **after** the deploy; both must pass; evidence includes `rollback_cmd` from `[env.*].rollback` | deploy exit ok **and** probe passes |
| `secret_scan` | scan the added lines of `git diff <default_branch>...job/<id>` with the rules below; **never store matched text** | zero hits |

**Secret rules — the single canonical table** (names are what `SecretHit.rule` stores and what tests assert; matched text is never stored):

| `rule` | Pattern (regex, applied to added lines) |
|---|---|
| `sk_api_key` | `sk-[A-Za-z0-9_-]{20,}` |
| `supabase_secret` | `sb_secret_[A-Za-z0-9_-]{10,}` |
| `github_token` | `ghp_[A-Za-z0-9]{20,}` |
| `github_pat` | `github_pat_[A-Za-z0-9_]{20,}` |
| `aws_access_key` | `AKIA[0-9A-Z]{16}` |
| `private_key_block` | `-----BEGIN [A-Z ]*PRIVATE KEY-----` |
| `slack_token` | `xox[abp]-[A-Za-z0-9-]{10,}` |
| `secret_file` | not a content rule: a file **path** in the diff matching `.env*`, `*.pem`, `id_rsa*`, `id_ed25519*` (hit has `line: 0`) |

A hit stores `{path, line, rule}` only. `vowp_core::gates::secret_scan::rules()` returns exactly the seven content rules above in this order; `scan_paths` implements `secret_file`.

**Anti-pattern ban (enforced in `Gate::validate`, types.md §8):** a gate may not merely check that a file exists for anything that must *work*. Concretely `validate` rejects: a `cmd`/`script` whose `run` is only `test -f …`/`[ -e … ]`/`ls …`/`stat …`/`cat …` (regex `^\s*(test\s+-[fed]|\[\s+-[fed]|ls|stat|cat|true|:)\b`), a `cmd` with `expect_exit` ≠ 0 and no `expect_pattern` (negative gates need a pattern), and a `deploy` gate without a `probe` whose `run` is non-trivial. The `report` gate is exempt (existence **plus parsed content** is its purpose).

## 5. L3 — verify

### 5.1 Creating the verify job (step 6, T15 → `StartVerify`)
1. Create job `<id>-verify-<attempt>` (`kind=verify, role=verifier, profile=verifier`, `owns=[]`, `needs=[]`, `reads=owns+reads of the original`, gates `[report]`), edge `verify verifies original`.
   Its brief states: the original brief path, the diff command (`git diff <default_branch>...job/<id>`), the gate evidence summary (all `gate_results` of this attempt, tails included), and the rule "judge only against evidence and the brief; run the commands yourself; do not edit files".
2. The verifier runs in **its own session**, in the original's worktree (the verify job's `worktree`/`branch` columns are copies; it has `owns=[]`, so it takes no ownership lock and the original — in `reviewing` — keeps the lock). Read-only by policy (permission preset), on a `verifier` slot (`max_verifiers`, 2).
3. Verifier prompt ends with: "Write your report with front-matter `verdict: pass|fail` and, on fail, `findings:` as a list of concrete, checkable problems (file, command, expected, actual). Report `status: done` when you finished judging."

### 5.2 Result
4. Verify job reaches `done` (T14), then its report is parsed: `verdict: pass` → `VerifyPassed` for the original. `verdict: fail` (≥1 finding required; a fail without findings is treated as `partial` → the verify job is retried once, then `Blocked{Question}`) → `VerifyFailed`.
5. Fix creation (T20): job `<id>-fix-<attempt>` (`kind=fix`, role/profile/owns copied, `needs=[]`, edge `fix fixes original`, brief = fix prompt + findings). Fix job `done` ⇒ `FixDone{Fix}` to the original (T22).
6. Bound: `attempts` ≤ `loop.max_attempts` (3). Exhausted → `on_exhausted` (§2): **escalate** creates `<id>-decision` (`kind=decision`, edge `decision escalates original`, `decision blocks <dependents>` for every job that `needs` the original) and an inbox item (kind `decision`, options `["retry","replan","cancel"]`).
7. A verify job that itself ends `stalled` is handled by the same L2/keeper rules (T4–T13); when it ends `failed` the original gets `VerifierFailed` (T37); a verifier provider limit goes through L6.

## 6. Merge queue (step 7)

One merge at a time per project (`workers`-independent; runs on a runner thread). Input: job in `merging` (never a fix/merge/verify job: those go `done` at T14).

1. `git fetch <remote>`; record `main_before = rev-parse <remote>/<default_branch>`.
2. In the job worktree: `git rebase <remote>/<default_branch>` (if `merge.rebase=false`: skip, require ff-ability).
   - **Conflict** (rebase exits ≠ 0): `git rebase --abort`; list conflicted files; emit `MergeConflict` (T26). The merge job's brief lists the files and tells the agent to `git rebase origin/main`, resolve, `git add`, `git rebase --continue`, rerun gates. It runs **in the same worktree** with the original's `owns` plus the conflicted files.
   - **Lockfile only** (every conflicted path is in `project.lock_files`, e.g. `Cargo.lock`): no merge job. Take the main version (`git checkout <remote>/<default_branch> -- <file>`), `git add`, `git rebase --continue`; after the rebase regenerate with `cargo update -w` (never `generate-lockfile`, which bumps versions) and, if the file changed, commit `chore: refresh lockfile (merge queue)` with the job's trailer. These are the only commits the supervisor authors; the ledger `merge` row records `lockfile_refreshed=true`.
3. Re-run the **repo-wide checks**: `merge.check_cmds` (falls back to `project.check_cmds`, e.g. `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`), each with `merge.timeout_secs` (2700) total budget. A failure → `MergeChecksFailed` (T27); evidence `Cmd` rows with `gate_index=-1`.
4. If `merge.secret_scan`: run the `secret_scan` gate over `<remote>/<default_branch>...job/<id>`; hits → `MergeChecksFailed`, and the job is **never pushed**.
5. Fast-forward: in the main checkout `git merge --ff-only job/<id>` (the main checkout is clean by invariant; if not, abort, write a ledger `merge` failure and park the job `Manual` with an inbox notice).
6. `git push <remote> <default_branch>` (if `project.push`); a rejected push (remote moved) → `git reset --hard <main_before>` in the main checkout and restart from step 1 (≤ 3 times, then `MergeChecksFailed`).
7. Delete branch `job/<id>` remote+local only if it was pushed by us; `git worktree remove <worktree>`; window `job-<id>` killed (tagged windows only).
8. Ledger `merge` row: job, `main_before`, `main_after`, files changed count, durations of each step. Emit `MergeSucceeded` (T25) → `UnblockDependents` → dependents may become `ready` on the next tick.

Pushes go only to `job/*` branches and to the default branch through this queue.

## 7. L4 planner and L5 keeper

Both are **sessions** with a fixed role (`SessionRole::Planner|Keeper`), one per project, created on first use, resumed every turn. One turn at a time per role (`runs` open check). Their runs use `RunRole::Planner|Keeper`, `RunPurpose::Plan|Keep`, no job.

**Planner due (`planner_due`)** — all must hold: not paused; no open planner run; provider available (else the keeper covers, below); `now - last_planner_turn >= LoopBounds.planner_min_gap_minutes` (default 5 min); and
(a) an unseen event for the planner exists (job `done|failed|stalled|parked`, new inbox answer, finished verify, limit/failover event, schedule firing) **or** (b) the `l4-planner` loop interval (`[loops.l4-planner].interval_secs`, default 30 min) elapsed **or** (c) there is no active job (`pending|ready|running|gating|reviewing|fixing|merging`) but non-terminal jobs exist and `LoopBounds.idle_gap_minutes` (default 15 min) elapsed.
**Keeper due**: an unseen stall/failure event (`stalled`, `failed`, hang, run crash with continues exhausted, rate-limit re-queue) **or** the `l5-keeper` loop interval (`[loops.l5-keeper].interval_secs`, default 20 min) elapsed; min gap `LoopBounds.planner_min_gap_minutes` (default 5 min); also when the planner's provider is limited (the keeper then *covers planning*: its prompt gains the planner duties and the planner digest).

**Digest** (`vowp_core::loops::digest::Digest::build`, ≤ 24 000 chars, each item ≤ 600 chars): new jobs done/failed/stalled/parked since the last turn with gate evidence summaries and report `open_items`, open inbox items and answers, limit/failover state (who is limited until when), counts by state, cost totals (receipts), slot usage, the last 5 ledger errors, and (planner only) the checkpoint text. Keeper digest additionally has the last 60 lines of each stalled job's session log (pretty).
**Actions** are CLI calls the agent runs itself in its turn: `vowp job add|update|retry|park|cancel`, `vowp inbox answer`, writing brief files, and for the keeper `vowp job nudge <id>` / `vowp job restart <id>` (= requests `KeeperNudge|KeeperRestart`), `vowp job replan <id>`, `vowp job fail <id>`. These CLI commands only write `req:` rows (D2) — the next tick applies them. The planner/keeper never edits the DB or merges.
**Failure of a planner turn:** counted in `limits` streak (§8.6); a failed turn is re-attempted at the next heartbeat, never in a tight loop.
**Takeover and hand-back:** while the planner's rung-0 provider is limited the keeper session is given the planner's capsule (SessionOps::write_capsule on the planner session, trigger `limit`) and the planner duties; when the probe succeeds (§8.5) the planner session resumes on rung 0 seeded with a fresh capsule written from the keeper's covering turns.

## 8. L6 — failover and probe-back

### 8.1 Inputs
`RunSummary.limit: Option<LimitEvent>` from the adapter (classification: `adapters.md` §5) plus `outcome`. L6 never parses text itself.

### 8.2 Actions by kind (applied at tick step 2, before job events)
| `outcome` / kind | Action |
|---|---|
| `Limit`/`quota` | write `limits(provider)`: `kind=quota`, `resets_at = event.resets_at + 2 min` (the single +2 min margin; adapters report exact times, adapters.md §2.4), `detail`; ledger `limit`; **hand off** (§8.4) the affected sessions to the next ladder rung whose provider is not limited, **only if** that rung is listed in the profile; else park the affected jobs with `Blocked{ProviderBlocked}` and one inbox notice. `profile.on_quota=park` → park; `next` → switch, no probe-back. No `resets_at` known: `now + 60 min`. |
| `Credits` | `limits(provider) kind=credits, resets_at=NULL`; **park every job whose profile's *current* rung provider is that provider** (`Blocked{Credits}`, §8.3), one inbox item; stop dispatching to it. Never switch to another paid provider (L6); `on_credits_exhausted` has only `park` in v0.1. |
| `Auth` | `limits kind=auth, resets_at=NULL`; park jobs of that provider (§8.3, `Blocked{Auth}`); no retries. |
| `Rate` | `limits kind=rate`, `resets_at = now + backoff` with `backoff = min(10 min × 2^streak, 60 min)` ±20 % jitter (deterministic LCG owned by the loop, seedable in tests). Set `concurrency_cap = max(1, running_on_provider − 1)` until `cap_until = resets_at`. The interrupted run does **not** consume `continues`: core feeds `JobEvent::RunRateLimited{until: resets_at}` (T35) instead of `RunEndedNoReport`, which delays the continue run until `resets_at` and sets `jobs.note="rate-limited; retry after <t>"`. |
| `Context` | write a capsule (`trigger=context`) then start a **fresh native session on the same harness** seeded with it (`SessionOps::switch` with `to = same executor`, reason `Context`); continue count unchanged. |
| `Crash`/`Error`/`Killed` | no limit row (except §8.6); L2 continue path (T4). |

### 8.3 Parking all jobs of a provider (credits and auth — one policy)
Design summary §10: credits exhausted ⇒ **park every job on that provider**; it is never a failover trigger (unlike `quota`). Algorithm:
1. For each job in `{pending, ready, running, stalled}` — this set **includes the verify/fix/merge child jobs** (they are ordinary `jobs` rows with their own profile, so a pending verify job of a `reviewing` original, or a fix job of a `fixing` original, is parked like any other) — whose profile's **current rung** (the rung it is running on, or `Profile::pick` for jobs not yet started) has `Executor::provider() == provider`: send `JobEvent::Blocked{Credits|Auth}` (T29). A job whose ladder happens to contain another rung is **still parked**: credits/auth never advance the ladder (the owner decides after topping up/logging in).
2. Jobs whose current rung is on a different provider are untouched. Parents in `gating`, `reviewing`, `fixing` or `merging` hold no harness run of their own (they wait for command gates, the merge queue, or their child job), so T29 deliberately has no such source states: once their child is parked they simply keep waiting, and `Resolved` on the child resumes the flow. Command gates and the merge queue use no provider and are unaffected.
3. One inbox item per provider/kind (dedupe on `title`), text: "credits exhausted for <provider>; top up and run `vowp inbox answer <id> resolved`" (auth: "log in again with the harness's own login, then `… resolved`").
4. Resolution: `vowp inbox answer <id> resolved` deletes the `limits` row and sends `Resolved` (T31) to every job parked for that provider/reason.

### 8.4 Capsule hand-off
1. For a **session-backed role (planner, keeper)**: `SessionOps::switch(SwitchRequest{session, to: next rung ExecutorRef, reason: Limit|Failover, drain_secs: 5})` — freeze, capsule, stop, start target with capsule + last 2 turns as plain text, record `switch` (sessions.md §5).
2. For a **job run** that was interrupted: the session continues on the next rung via the same `switch` call (new native session seeded with the capsule), then `StartRun{Continue}` (T4) runs there; the job's `rung` in `runs` is the new index. A job never changes node in v0.1 failover.
3. If `SessionOps::switch` fails (probe of target fails, seed rejected twice): the session stays `blocked`, job is `Blocked{ProviderBlocked}`, inbox notice, ledger `switch` failure.

### 8.5 Probe-back
1. A probe is a run (`RunRole::Probe`, `RunPurpose::Probe`) of one tiny prompt (`"Reply with the single word: ok"`) on the limited provider's executor in a scratch directory, `timeout 60 s`, cost recorded.
2. Scheduled at `resets_at`, then every `probe_interval_minutes` (15) after a failed probe; `limits.last_probe_at` updated; `probe_fail_streak++` on failure.
3. Success (`outcome=ok`): delete the `limits` row; ledger `limit` "provider back"; for planner/keeper whose current rung > 0 and whose rung-0 provider is that provider: `SessionOps::switch(reason=ProbeBack)` to rung 0 (capsule hand-off), only when **no turn is running** (wait for the next quiet tick; never mid-turn). Running jobs stay where they are until their next natural start (no mid-run flip-flops).
4. A probe failing with a *new* limit text updates `resets_at` from it.

### 8.6 Unknown failures (D15)
Per provider the loop counts consecutive runs ended `error|crash` with **no** limit classification. At **3** → `limits(kind=unavailable, resets_at=now+15 min)` (and `claude_retry`-style probe-back at expiry). Any `ok` run resets the streak. The streak lives in `limits.probe_fail_streak` (row created with `resets_at=NULL` and kind `unavailable` only when tripped; before that, in `kv['fail_streak:<provider>']`).

## 9. L7 — inbox

1. Items are created by: a worker report with `status: blocked` / `questions_for_owner` (kind `question`, `job_id` set, job → `parked(Question)`); L6 credits/auth/rate notices; exhausted attempts with escalate (kind `decision`); promotion approvals (kind `approval`, created by `vowp promote` when `approval = "human"`); keeper/planner `vowp inbox ask`; `notice` items for FYIs (never park anything).
2. Only the asking job waits; everything else continues (principle 8).
3. `vowp inbox answer <id> <text>`: stores `answer`, `answered_by=$USER`, state `answered`; if `job_id` set: the answer text is written to `<worktree>/.vowp/jobs/<id>/answer-<n>.md` and appended to the job note, and `Resolved` is queued so the job re-enters `ready` and its next run's prompt includes the answer. Approvals: answer `approve` unblocks the promote job; anything else refuses it.
4. `vowp inbox dismiss` marks `dismissed` and resolves nothing.
5. Phone/ntfy notification hook: **deferred to v0.2** (config key `[notify]` reserved, unparsed in v0.1 → `deny_unknown_fields` error).

## 10. L8 — watchdog

- **systemd user unit** `vowp.service` (file shipped by the install script): `ExecStart=%h/.local/bin/vowp up --foreground`, `Restart=always`, `RestartSec=5`, `WantedBy=default.target`; `loginctl enable-linger` documented. `vowp up --foreground` runs the supervisor loop in-process (no tmux attach) and also ensures the tmux session.
- **Heartbeat contract:** L1 step 13 writes `~/.vowp/heartbeat` (unix seconds, ASCII). The cron check (`*/5 * * * *`, script `scripts/vowp-watchdog.sh`): read the file; if missing or `now - value > 180` s → `systemctl --user restart vowp.service` (or `vowp up --detach` when systemd is unavailable), then append a line to `~/.vowp/logs/watchdog.log`. `vowp status --json` exposes `{"heartbeat_age_secs":..,"paused":..,"pid":..}` for it.
- On restart the supervisor performs the §1.1 step-1 reconcile (orphans → crash → L2 continues). Nothing else is needed: DB + logs are the state.

## 11. L9 — schedules

1. **Dialects.** `spec_kind=cron`: standard 5-field `min hour dom mon dow`. The `cron` crate requires 6/7 fields (seconds first), so the loop **prefixes `"0 "`** before parsing (`Schedule` parsed with `cron::Schedule::from_str`); only 5-field cron is accepted in v0.1 (any other field count is a `Config` error, in line with architecture.md §5.3 rule 3). `spec_kind=interval`: `every <n><s|m|h|d>` (e.g. `every 30m`), minimum 60 s. `tz`: `UTC` or a fixed offset `+HH:MM` in v0.1 (D-open-question 2).
2. **Next-run computation** `next_after(schedule, t: Timestamp) -> Option<Timestamp>`: cron → first match strictly after `t` evaluated in the schedule's offset, converted back to UTC; interval → `anchor + k·period` with `anchor = created_at`, smallest `k` such that result `> t`.
3. **Each tick (step 11)**, for every `active` schedule (and not `paused` loop `l9-schedules`):
   a. if `next_run_at IS NULL`: set it to `next_after(now)`; continue.
   b. if `next_run_at > now`: continue.
   c. **Due.** `due = next_run_at`. Idempotency key `key = "<schedule_id>@<due>"`. If a ledger `schedule` row with `detail_json.key == key` exists → do not fire again.
   d. **Missed-run policy** (the node may have been down): lateness `late = now - due`. If `late <= 2 × tick_secs` → fire normally. Else if `missed_policy = run_once` → fire **once** (for the earliest missed `due`, not N times); `skip` → don't fire, ledger `schedule` row "skipped missed run".
   e. **Advance before firing** (the order implemented by `ScheduleEngine::tick`): set `last_run_at=now`, `last_status`, and `next_run_at = next_after(now)` (strictly in the future: all missed occurrences collapse), then persist the row. A crash after this step loses at most one firing; it can never double-fire.
   f. Fire (only when step d says so and no ledger row with `key` exists) = write the ledger `schedule` row with the key **first**, then execute the action (below) and its effects. A crash between the ledger row and the action leaves "key present, action not confirmed" (logged on restart); v0.1 accepts at-most-once.
4. **Actions.** `JobTemplate{template}` → `Store::insert_job(template with id suffixed "-<yyyymmddhhmm>" of `due` UTC)`, `created_by="schedule:<id>"`, the job goes through the normal graph; `Loop{loop_name}` → mark the loop due now (a `run_loop` request); `Command{argv,cwd,timeout_secs}` → spawn without a shell, capture the last 200 lines into the ledger `detail_json`, exit ≠ 0 ⇒ `last_status="failed: exit N"` plus an inbox `notice` (once per schedule per day).
5. Defaults installed by `vowp init` (all `state=paused` until the user enables them): planner heartbeat is **not** a schedule (it is L4's own heartbeat).

## 12. Stall and no-progress rules

Defaults below. `LoopBounds` fields are configurable under `[loops.*]`; other values come from the config field named in
the table or are fixed v0.1 constants.

| Rule | Value | Source/config |
|---|---|---|
| Tick | 60 s | `[node].tick_secs` |
| Stalled: no new session event and no new commit | 20 min (`stall_minutes`) | `LoopBounds.stall_minutes` |
| Hung before first output (0 bytes) | 8 min (`silent_minutes`) | `LoopBounds.silent_minutes` (D7) |
| Automatic continues per attempt | 2 (`max_continues`) | `LoopBounds.max_continues` (L2) |
| Hang restarts | 3 (`max_hang_restarts`) | `LoopBounds.max_hang_restarts` (D6) |
| Fix rounds (`attempts`) | 3 = `job.loop.max_attempts` | per job (`JobLoop`, not `LoopBounds`); L3 |
| Same action repeated without new evidence | 3 (`RepeatGuard`) → stop the loop, escalate (`NotifyInbox(Decision)`) | `LoopBounds.same_action_max` |
| Planner heartbeat / keeper heartbeat | 30 min / 20 min | `[loops.l4-planner].interval_secs` / `[loops.l5-keeper].interval_secs`; L4/L5 |
| Min gap between planner/keeper turns | 5 min | `LoopBounds.planner_min_gap_minutes` |
| Idle gap (nothing active, work remaining) | 15 min | `LoopBounds.idle_gap_minutes` |
| Checkpoint | every 120 min | `LoopBounds.checkpoint_minutes` |
| Gate timeout | 1800 s | `CmdGate.timeout_secs` (default) |
| Merge budget | 2700 s | `[merge].timeout_secs` |
| Quota reset margin | +2 min | fixed v0.1 constant |
| Probe cadence after failed probe | 15 min | `LoopBounds.probe_interval_minutes`; L6 |
| Unknown-failure streak → unavailable | 3 runs → 15 min | fixed v0.1 constant; D15 |
| Drain on switch | 5 s | `SwitchRequest.drain_secs`; sessions.md §5 |
| Rate backoff | 10 min doubling to 60 min, ±20 % jitter | fixed v0.1 constants; §8.2 |

`RepeatGuard` keys: `(job, action-kind)` with a fingerprint of the evidence (gate tails hash, report sha, run outcome). Three consecutive identical fingerprints for the same key ⇒ the job is moved to `failed` with `note="no progress: <action> x3"`, the loop trips (`loops.last_status`), inbox `decision`.

## 13. Drill tests (`crates/vowp-core/tests/drills*.rs`) — fake harness scripts and `FakeClock` only

All drills use: temp project dir with a bare `origin` + clone, a `Store` on a temp dir, `FakeWindows`, `FakeClock`, and fake harness scripts from `vowp-adapters/tests/fakes/` (copied path via a `FAKES_DIR` helper in the test). No network, no real tmux needed.

| Test | Given | When | Then |
|---|---|---|---|
| `drill_failover_and_probe_back` | planner profile with two rungs on different providers (`claude-code` first, `opencode` second); fake claude emits `Claude AI usage limit reached\|<unix t+3h>` and exits 1 | planner turn runs; tick; clock advances past `resets_at+2m`; fake claude then succeeds; ticks continue | `limits(<first provider>)` row kind `quota`, `resets_at = t+3h+2m`; a `switch` row reason `limit` with a capsule; the next planner turn used the opencode fake; after the reset exactly one `probe` run is made, row deleted, a second `switch` reason `probe_back`; ledger shows both; no jobs parked |
| `drill_crash_continue` | worker job running; fake opencode kills itself (`kill -9`) after 1 event, no report | tick ×2 | run outcome `crash`, job stays `running` with `continues=1`, a `Continue` run started; after 2 more crashes job `stalled`, keeper woken (`WakeKeeper`) |
| `drill_stall_hang` | fake harness prints nothing and sleeps | clock +9 min, tick | run stopped, `hang_restarts=1`, fresh start; after 3 → `stalled` |
| `drill_merge_conflict` | two jobs edit the same file (owns disjoint by mistake), A merged first | B reaches `merging` | `MergeConflict`; job B `fixing`, a `merge` job created with `fixes` edge, same worktree; fake worker resolves + reports; B returns to `merging` and merges ff-only; `origin/main` contains both; repo-wide check cmd (`true` replaced by a script) ran once per merge |
| `drill_merge_lockfile` | both jobs change `Cargo.lock` only | merge B | no merge job; lockfile commit by queue; ledger `merge` row has `lockfile_refreshed=true` |
| `drill_credits_park` | two jobs on one provider; fake opencode prints a 402 / `insufficient balance` error | tick | `limits(<that provider>) kind=credits resets_at NULL`; both jobs `parked(credits)`; exactly one inbox item; no run started on any other provider; after `inbox answer … resolved`, jobs `ready` |
| `drill_gate_fail_fix_loop` | job whose `cmd` gate fails twice then passes (fake worker edits a file based on the fix prompt) | run to completion | attempts=2, two fix jobs, then `reviewing`→`merging`→`done`; a third failing config ends `failed` + decision job + inbox item (`on_exhausted=escalate`) |
| `drill_schedule_missed_run` | schedule `every 30m`, `missed_policy=run_once`; clock stopped 3 h, then ticks | restart supervisor, tick | exactly one job created (not 6), key recorded, `next_run_at` in the future; with `skip` zero jobs; double tick at the same `due` creates one job |
| `drill_orphan_reconcile` | open `runs` row with a dead pid | tick | run finalised `crash`, job continues via L2 |
| `drill_secret_scan_blocks_push` | worker commits a file containing a fake `ghp_` token | merge | `MergeChecksFailed`, nothing pushed (`origin/main` unchanged), evidence has `rule=github_token` and no token text anywhere in the DB |
