# crates/vowp-graph.md — DAG, state machine, ownership locks, dispatch

Depends on `vowp-protocol`, `globset`, `thiserror`, `serde_yaml_ng`. **Pure functions only**: no I/O, no clock reads (time is passed in), no
threads, no DB. `vowp-core` feeds it rows from `vowp-store` and applies the effects it returns.

## 1. Module layout

```
crates/vowp-graph/
  src/lib.rs        // re-exports; #![forbid(unsafe_code)]
  src/error.rs      // GraphError
  src/graph.rs      // Graph, validation, topo order, ready_jobs
  src/machine.rs    // JobEvent, Effect, Transition, transition(), TransitionBounds
  src/owns.rs       // owns_overlap, path_allowed, OwnsLocks
  src/dispatch.rs   // DispatchCaps, DispatchChoice, plan_dispatch
  src/render.rs     // render_ascii, render_mermaid
  src/yaml.rs       // export_yaml, import_yaml, ImportedGraph
  src/guards.rs     // attempts_left, StallJudge, RepeatGuard
  tests/            // machine_table.rs, graph_props.rs, dispatch_props.rs, owns.rs, render.rs, yaml.rs, fixtures/*.yaml
```

## 2. Graph (`graph.rs`)

```rust
pub struct Graph { /* jobs: BTreeMap<JobId, Job>, edges: Vec<Edge>, indexes */ }

impl Graph {
    /// Validates and indexes. Errors: DuplicateJob, UnknownJob (edge endpoint not in `jobs`), SelfEdge, Cycle.
    /// Cycle detection uses edges of kind Needs, Verifies, Escalates, Blocks in **precedence direction** (rules 2–3 below: `a needs b` ⇒ b→a, `a blocks b` ⇒ a→b, `a verifies|escalates b` ⇒ b→a), the same direction `topo_order` uses; so `a needs b` plus `b blocks a` (both demand b before a) is valid, not a cycle;
    /// `Fixes` edges are the bounded loop edge and are ignored. Every `job.needs` entry must be mirrored by a `Needs` edge:
    /// `Graph::new` ADDS missing `Needs` edges from `job.needs` and errors (EdgeMismatch) if a Needs edge exists that is not in `job.needs`.
    pub fn new(jobs: Vec<Job>, edges: Vec<Edge>) -> Result<Graph, GraphError>;
    pub fn job(&self, id: &JobId) -> Option<&Job>;
    pub fn jobs(&self) -> impl Iterator<Item = &Job>;                    // ordered by id
    pub fn edges(&self) -> &[Edge];
    pub fn needs_of(&self, id: &JobId) -> Vec<&Job>;                     // jobs `id` needs (Needs edges, src=id), ordered by id
    pub fn dependents_of(&self, id: &JobId) -> Vec<&Job>;                // jobs that need `id`
    pub fn edges_from(&self, id: &JobId, kind: EdgeKind) -> Vec<&Edge>;  // src == id
    pub fn edges_to(&self, id: &JobId, kind: EdgeKind) -> Vec<&Edge>;    // dst == id
    pub fn fix_parent(&self, fix_job: &JobId) -> Option<&JobId>;         // dst of its `Fixes` edge
    /// Kahn's algorithm over the acyclic edge set (rule 2.3 gives the direction of each kind); ties broken by (priority desc, id asc) so the result is deterministic.
    pub fn topo_order(&self) -> Vec<JobId>;
    /// Jobs in state Pending whose every Needs target is Done AND that have no Blocks edge (src blocks this job) whose src job is non-terminal.
    /// Sorted by (priority desc, created_at asc, id asc).
    pub fn ready_jobs(&self) -> Vec<JobId>;
    /// Longest Needs-path length from any root (jobs without needs = layer 0). Used by render_ascii.
    pub fn layer(&self, id: &JobId) -> usize;
    pub fn with_job_replaced(&self, job: Job) -> Result<Graph, GraphError>;   // functional update; revalidates
}
```

Rules
1. **Edge reading** is `src <kind> dst` (types.md §7). For readiness: `a needs b` ⇒ `b` must be Done before `a` is ready. `a blocks b` ⇒ `a` must reach a terminal state before `b` is ready (decision jobs). `a verifies b` / `a fixes b` / `a escalates b` never gate readiness of `b`.
2. **Cycle detection**: DFS with colour marks over `Needs ∪ Verifies ∪ Escalates ∪ Blocks`. The error carries one cycle path (`Vec<JobId>`, first == last).
3. `topo_order` order constraints: `a needs b` ⇒ b before a; `a blocks b` ⇒ a before b; `a verifies b` and `a escalates b` ⇒ b before a (subject first); `Fixes` edges impose nothing. The cycle check uses exactly these constraints.
4. `ready_jobs` is the only readiness definition; core calls it in tick step 4 and `plan_dispatch` only considers jobs in state `Ready`.
   (`Pending → Ready` is the state change; `ready_jobs()` lists the Pending jobs that qualify.)

## 3. State machine (`machine.rs`)

The complete transition table is in **loops.md §2** (rows T1–T37); this module **implements that table exactly** and there is deliberately no second copy of it. Tests in §9 iterate over table rows by id.

```rust
/// Events the supervisor feeds into `transition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    NeedsSatisfied,
    Dispatched,
    ReportReady,
    RunEndedNoReport,
    RunRateLimited { until: Timestamp },   // T35: provider 429; delays the continue, consumes no counter
    Stalled { silent: bool },
    KeeperNudge, KeeperRestart, KeeperReplan, KeeperFail,
    GatesPassed, GatesFailed,
    VerifyPassed, VerifyFailed,
    FixDone { kind: JobKind },           // kind of the finished fix job: Fix or Merge
    FixFailed,
    VerifierFailed,                        // T37: the verify job failed or yielded no verdict
    MergeSucceeded, MergeConflict, MergeChecksFailed,
    Blocked { reason: ParkedReason },
    HumanPark, Resolved, Retry, Cancel,
}

/// Side effects the caller must perform (the machine never does I/O). Counter effects mean "update the job row".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    CreateWorktree,
    StartRun { purpose: RunPurpose },                // Start | Continue | Fix
    DelayRun { purpose: RunPurpose, until: Timestamp }, // start that run only once `now >= until` (T35)
    StopRun,
    IncrementContinues, ResetContinues, IncrementHangRestarts, IncrementAttempts, ResetCounters,
    RunGates,
    StartVerify,
    CreateFixJob { kind: JobKind },                  // Fix or Merge
    EnqueueMerge,
    CleanupWorktree,
    UnblockDependents,
    CreateDecisionJob,                               // kind decision, `escalates` edge, `blocks` edges to dependents
    NotifyInbox { kind: InboxKind },
    NotifyParent,                                    // deliver FixDone/FixFailed to the job this fix job `fixes`
    WakeKeeper, WakePlanner,
    SetParkedReason(ParkedReason), ClearParkedReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition { pub to: JobState, pub effects: Vec<Effect> }

/// Bounds the guards use; `Default` = MAX_CONTINUES 2, MAX_HANG_RESTARTS 3. max_attempts comes from `job.loop_`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionBounds { pub max_continues: u32, pub max_hang_restarts: u32 }
impl Default for TransitionBounds { /* 2, 3 */ }

pub fn transition(job: &Job, ev: &JobEvent) -> Result<Transition, GraphError>;      // == transition_with(job, ev, &TransitionBounds::default())
pub fn transition_with(job: &Job, ev: &JobEvent, b: &TransitionBounds) -> Result<Transition, GraphError>;

pub const MAX_CONTINUES: u32 = 2;
pub const MAX_HANG_RESTARTS: u32 = 3;
```

Rules
1. `transition` is a pure function of `(job.state, job.kind, job.continues, job.hang_restarts, job.attempts, job.loop_, job.gates, ev)`. It never mutates `job`.
2. Unlisted `(state, event)` ⇒ `Err(GraphError::InvalidTransition { state, event })` (event as `format!("{ev:?}")`).
3. The **Exhausted** branch (`attempts >= loop_.max_attempts`) of T18/T21/T28 returns by `loop_.on_exhausted`: Escalate ⇒ `Failed`, effects `[CreateDecisionJob, NotifyInbox{Decision}, WakeKeeper]`; Park ⇒ `Parked`, `[SetParkedReason(Exhausted), NotifyInbox{Question}]`; Fail ⇒ `Failed`, `[WakeKeeper]`.
4. `GatesPassed` for jobs whose `kind ∈ {Fix, Merge, Verify}` ⇒ `Done` with `[NotifyParent]` (T14); `GatesFailed` for those kinds ⇒ `Failed` with `[NotifyParent, NotifyInbox{Notice}]` (T36, never a fix-of-fix); `KeeperFail` on those kinds adds `NotifyParent` (T13); for all other kinds `Reviewing` if any `Gate::Review` is present in `job.gates` else `Merging`.
5. `Blocked`/`HumanPark` effect lists: include `StopRun` iff `job.state == Running`; `NotifyInbox{Question}` for reasons `Question|Auth|Credits|ProviderBlocked`, none for `Manual|Exhausted`; plus `SetParkedReason(reason)`.
6. `Cancel` returns `Cancelled` with `[StopRun]` only if the job is `Running` (else no effects); the worktree is kept (no `CleanupWorktree`).
7. `JobState::is_terminal` jobs (`Done|Replanned|Cancelled`) accept **no** event (all return `InvalidTransition`).

```rust
/// Fix-loop bookkeeping.
pub fn attempts_left(job: &Job) -> u32;            // loop_.max_attempts.saturating_sub(attempts)
pub fn is_exhausted(job: &Job) -> bool;            // attempts_left == 0
```

## 4. Ownership (`owns.rs`)

```rust
/// Conservative overlap test between two sets of `owns` globs. May return true for sets that cannot actually collide; never false for sets that can.
pub fn owns_overlap(a: &[String], b: &[String]) -> bool;
/// Is `path` (repo-relative, '/'-separated) writable under `owns`, or listed in `always_allowed`? Used by the owns_respected gate.
pub fn path_allowed(owns: &[String], always_allowed: &[String], path: &str) -> bool;
pub fn literal_prefix(pattern: &str) -> Vec<String>;   // exposed for tests; the component list described below

#[derive(Debug, Default, Clone)]
pub struct OwnsLocks { /* BTreeMap<JobId, (Vec<String>, Option<JobId> /*group*/)> */ }
impl OwnsLocks {
    pub fn from_running(jobs: &[Job], graph: &Graph) -> OwnsLocks;           // every job with state.holds_lock()
    /// Ok if `job.owns` overlaps no held lock (fix/merge jobs share their `fixes` parent's group and never conflict with it).
    pub fn try_acquire(&mut self, job: &Job, graph: &Graph) -> Result<(), Vec<JobId>>;   // Err = holders it conflicts with
    pub fn release(&mut self, id: &JobId);
    pub fn conflicts(&self, job: &Job, graph: &Graph) -> Vec<JobId>;
    pub fn holders(&self) -> Vec<JobId>;
}
```

### `owns_overlap` algorithm (exact)
1. **Normalise** each pattern: trim; strip leading `./`; replace `\` with `/`; collapse `//`; strip a trailing `/` (a trailing `/` means "directory", treated as `<dir>/**`). A pattern that is absolute (starts with `/`), contains a `..` component, or is empty gets the **empty prefix** (it overlaps everything).
2. **Literal prefix** = components of the pattern (split on `/`) up to, **excluding**, the first component containing any glob metacharacter `* ? [ ] { }`. A pattern with no metacharacter has all components as prefix (it may name a file or a directory: treated as an ancestor of anything below it).
3. Two patterns **overlap** iff one prefix is an ancestor-or-equal of the other, comparing component-wise (`["crates","x"]` is an ancestor-or-equal of `["crates","x","src"]`; `["a","b.rs"]` vs `["a","c.rs"]` do not overlap). The empty prefix is an ancestor of everything.
4. `owns_overlap(a,b)` is true iff any pattern in `a` overlaps any pattern in `b`. Empty `a` or `b` (jobs that own nothing, e.g. verify jobs) never overlap anything.

Examples: `crates/vowp-graph/**` vs `crates/vowp-store/**` → false; `crates/**` vs `crates/vowp-graph/src/lib.rs` → true; `docs/*.md` vs `docs/design/**` → true (conservative); `**/*.rs` vs anything → true; `Cargo.toml` vs `Cargo.lock` → false.

### `path_allowed` algorithm
1. Normalise `path` as above; if absolute or contains `..` → `false`.
2. Build a `GlobSet` from `owns ∪ always_allowed` using `GlobBuilder::new(p).literal_separator(true).build()` (so `*` does not cross `/`, `**` does). For each pattern also add `"<pattern>/**"` when the pattern has no metacharacter or ends with `/` (a bare directory name).
3. Return `set.is_match(path)`. An empty `owns` allows only `always_allowed`.
Invalid glob ⇒ treated as no-match (and `Gate::validate` has already rejected it at job creation).

## 5. Dispatch (`dispatch.rs`)

```rust
#[derive(Debug, Clone)]
pub struct DispatchCaps {
    pub free_workers: u32,                           // idle worker slots (role Worker|Planner|Keeper jobs of kind != verify)
    pub free_verifiers: u32,                         // idle verifier slots (role Verifier)
    pub provider_free: std::collections::HashMap<String, u32>,   // remaining concurrent sessions per provider (max_concurrent − open runs, minus cap)
    pub limited: std::collections::HashMap<String, Option<Timestamp>>,   // providers currently limited (provider -> resets_at)
    pub profiles: std::collections::BTreeMap<String, Profile>,
    pub now: Timestamp,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchChoice { pub job_id: JobId, pub executor: Executor, pub rung: u32, pub provider: String, pub node: Option<NodeName> }

pub fn plan_dispatch(graph: &Graph, running: &[Job], caps: &DispatchCaps) -> Vec<DispatchChoice>;
```

Algorithm
1. Candidates = graph jobs in state `Ready`, sorted by `priority` desc, `created_at` asc, `id` asc.
2. `locks = OwnsLocks::from_running(running, graph)`; slot budgets copied from `caps`; `provider_left = caps.provider_free.clone()`.
3. For each candidate in order:
   a. Skip if role is Verifier and `free_verifiers == 0`, or role is Worker and `free_workers == 0`. Planner/Keeper-role jobs consume **no** slot but at most one such job per role is chosen per call and none if a job of that role is already in `running`.
   b. Skip if `locks.try_acquire(job)` fails (conflict waits; no error).
   c. `profiles[job.profile]` missing ⇒ skip. `Profile::pick(&caps.limited, caps.now)` ⇒ `None` ⇒ skip (every rung limited; the job stays `ready`). Otherwise `(rung, exec)`.
   d. `provider_left[exec.provider()]` must be > 0 (absent key = unlimited); decrement.
   e. Accept: decrement slot budget, record the choice (the lock stays held so later candidates see it).
4. Never returns two choices whose `owns` overlap, and never more choices than slots. Output order = acceptance order.

## 6. Rendering (`render.rs`)

```rust
pub fn render_ascii(g: &Graph) -> String;
pub fn render_mermaid(g: &Graph) -> String;
```
**ASCII**: one line per job ordered by `(layer, id)`: `L{layer} {id} [{state}] {title≤48 chars}{"  <- " + needs comma-separated if any}`; a trailing line per non-needs edge group is **not** printed (ASCII shows needs only). Output ends with `\n`.
```
L0 skeleton [done] Cargo workspace skeleton
L1 protocol [running] protocol types  <- skeleton
L2 graph-engine [pending] graph engine  <- protocol
```
**Mermaid**: first line `graph TD`; one node line per job ordered by id: `    graph_engine["graph-engine (pending)"]` (node key = id with `-` → `_`, label escaped: `"` → `'`); then edge lines ordered by `(kind, src, dst)`: `needs` as `dst_key --> src_key` (dependency flows into dependent); `verifies` `src -.->|verifies| dst`; `fixes` `src ==>|fixes| dst`; `escalates` `src -.->|escalates| dst`; `blocks` `src -->|blocks| dst`. Then `classDef` lines are NOT emitted (state colours are v0.2).
```
graph TD
    skeleton["skeleton (done)"]
    protocol["protocol (running)"]
    skeleton --> protocol
```

## 7. YAML export/import (`yaml.rs`)

```rust
pub struct ImportedGraph { pub jobs: Vec<NewJob>, pub edges: Vec<Edge> /* non-needs edges only */ }
pub fn export_yaml(g: &Graph) -> Result<String, GraphError>;
pub fn import_yaml(yaml: &str) -> Result<ImportedGraph, GraphError>;
```
Format (stable, sorted by job id, 2-space indent):
```yaml
version: 1
jobs:
  - id: graph-engine
    title: graph engine
    kind: build
    role: worker
    profile: worker
    priority: 50
    needs: [protocol]
    owns: ["crates/vowp-graph/**"]
    reads: ["docs/design/**"]
    gates: [{report: {fields: [status, branch, commits, tests]}}, {cmd: {run: "cargo test -p vowp-graph", expect_exit: 0}}, {owns_respected: true}, {review: {by: verifier}}]
    loop: {until: gates, max_attempts: 3, on_exhausted: escalate}
edges:
  - {src: graph-engine-verify-0, dst: graph-engine, kind: verifies}
```
State, counters, timestamps, branch/worktree are **not** exported (a graph file is a plan, not a run). Import validates gates (`Gate::validate`), ids (`JobId::parse`), `version == 1`, and runs `Graph`-style cycle/unknown-id checks over the imported set.

## 8. Progress guards (`guards.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallThresholds { pub stall_minutes: u32 /* 20 */, pub silent_minutes: u32 /* 8 */ }
impl Default for StallThresholds { /* 20, 8 */ }

#[derive(Debug, Clone)]
pub struct StallInput {
    pub now: Timestamp,
    pub run_started_at: Timestamp,
    pub last_event_at: Option<Timestamp>,     // newest session event of the run
    pub last_commit_at: Option<Timestamp>,    // newest commit on the job branch
    pub output_bytes: u64,                    // bytes the run's output file holds
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallVerdict { Healthy, Stalled { idle_minutes: u32, silent: bool } }

pub struct StallJudge;
impl StallJudge {
    /// progress = max(last_event_at, last_commit_at, run_started_at); idle = now − progress (whole minutes, floor).
    /// silent = output_bytes == 0. Threshold = silent_minutes if silent else stall_minutes. Stalled iff idle >= threshold.
    pub fn judge(input: &StallInput, th: &StallThresholds) -> StallVerdict;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatVerdict { Ok, Tripped { count: u32 } }
/// Trips when the same (key, fingerprint) is observed `max` times in a row (default 3). A different fingerprint or key resets the count.
#[derive(Debug, Clone)]
pub struct RepeatGuard { /* max, last: Option<(String,String)>, count */ }
impl RepeatGuard {
    pub fn new(max: u32) -> Self;
    pub fn observe(&mut self, key: &str, fingerprint: &str) -> RepeatVerdict;   // 3rd identical observation returns Tripped{count:3}
    pub fn reset(&mut self);
}
```

## 9. Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("duplicate job id {0}")]                         DuplicateJob(JobId),
    #[error("unknown job {0} referenced by an edge")]        UnknownJob(JobId),
    #[error("self edge on {0}")]                             SelfEdge(JobId),
    #[error("dependency cycle: {0:?}")]                      Cycle(Vec<JobId>),
    #[error("edge/needs mismatch on {0}")]                   EdgeMismatch(JobId),
    #[error("invalid transition: {event} in state {state:?}")] InvalidTransition { state: JobState, event: String },
    #[error("invalid owns pattern {0:?}")]                   InvalidOwns(String),
    #[error("yaml: {0}")]                                    Yaml(String),
    #[error(transparent)]                                    Protocol(#[from] vowp_protocol::ProtocolError),
}
impl GraphError { pub fn kind(&self) -> vowp_protocol::ErrorKind; }
```

## 10. Named tests (hermetic: no files except `tests/fixtures/*.yaml`, helper `fn job(id, state, owns, needs, priority) -> Job`)

Graph
- `graph_new_rejects_unknown_edge_endpoint` — `UnknownJob`.
- `graph_new_rejects_duplicate_ids` / `graph_new_rejects_self_edge` — respective errors.
- `graph_new_detects_needs_cycle_and_reports_path`, `graph_needs_plus_blocks_same_direction_is_not_a_cycle` — a→b→c→a gives `Cycle` with first==last.
- `graph_fixes_edge_may_form_loop` — `fix fixes orig` plus `orig needs fix`-free graph is accepted; a `fixes` back-edge never counts as a cycle.
- `graph_adds_needs_edges_from_job_needs` / `graph_rejects_needs_edge_missing_in_job` — EdgeMismatch.
- `ready_jobs_requires_all_needs_done` / `ready_jobs_respects_blocks_edge` / `ready_jobs_ignores_non_pending` / `ready_jobs_order_priority_then_age`.
- `topo_order_deterministic_ties` — fixed expected vector for a diamond graph.
- `layer_is_longest_needs_path`.
- proptest `prop_topo_order_is_valid` — for random DAGs (≤ 30 jobs), every job appears once and after all jobs it needs.
- proptest `prop_ready_jobs_matches_brute_force` — compare with a naive O(n²) implementation over random states.

State machine (one test per table row of loops.md §2, named `t01_pending_needs_satisfied_to_ready` … `t12_stalled_keeper_replan_to_replanned`, `t34_parked_or_failed_keeper_replan_to_replanned`, `t35_run_rate_limited_delays_continue_without_counter_change`; plus `t33_cancel_keeps_worktree_and_stops_only_running` (rows T29–T33 are covered by `blocked_effects_depend_on_reason_and_state` and these); each builds a `Job` in the from-state with the guard inputs, calls `transition`, and asserts the exact `to` and the exact `effects` vector)
- `t04_t05_continue_guard_boundary` — `continues=1` → T4, `continues=2` → T5.
- `t08_t09_hang_restart_boundary` — `hang_restarts=2` → T8, `3` → T9.
- `t17_t18_attempts_boundary_and_on_exhausted_variants` — three `on_exhausted` values produce the three documented outcomes.
- `t14_fix_merge_verify_job_gates_passed_goes_done`, `t36_gates_failed_on_fix_merge_verify_job_fails_with_notify_parent_no_fix_of_fix`, `t24_fix_failed_fails_original`, `t37_verifier_failed_parks_original_with_question`, `t13_keeper_fail_notifies_parent_for_child_kinds` / `t15_t16_review_gate_selects_reviewing_or_merging`.
- `blocked_effects_depend_on_reason_and_state`.
- `invalid_pairs_are_rejected_for_every_state` — exhaustive loop over all (state, event) pairs: ok iff the pair appears in the table-derived allow-list embedded in the test.
- proptest `prop_terminal_states_accept_nothing` — any terminal-state job + any event ⇒ `InvalidTransition`.
- proptest `prop_transition_never_panics_and_is_pure` — random job/event, calling twice returns equal results.

Ownership
- `overlap_table` — the examples in §4 plus ≥ 12 more pairs asserted exactly.
- `overlap_empty_owns_never_overlaps` / `overlap_dotdot_and_absolute_are_global`.
- `literal_prefix_examples` — `crates/x/**`→`["crates","x"]`, `Cargo.toml`→`["Cargo.toml"]`, `**/a`→`[]`, `docs/*.md`→`["docs"]`.
- `path_allowed_matches_glob_semantics` — `*` does not cross `/`, `**` does, bare dir allows contents, `always_allowed` honoured, `../x` and `/etc/x` refused.
- proptest `prop_overlap_is_conservative` — for random literal paths p and patterns q built from p's prefix, if `path_allowed([q],[],p)` then `owns_overlap([q],[p])`.
- `locks_fix_job_shares_parent_group` / `locks_release_frees_conflict`.

Dispatch
- `dispatch_orders_by_priority_then_age`, `dispatch_skips_overlapping_owns_with_running`, `dispatch_skips_overlap_among_chosen`, `dispatch_respects_worker_and_verifier_slots`, `dispatch_respects_provider_free`, `dispatch_skips_job_when_all_rungs_limited`, `dispatch_picks_next_rung_when_first_limited` (asserts `rung==1` and executor), `dispatch_one_planner_job_at_a_time`.
- proptest `prop_dispatch_never_two_overlapping_owns` — random ready sets/owns/caps: chosen ∪ running owns pairwise non-overlapping (except fix-group), `choices.len() <= slots`.

Rendering / YAML / guards
- `render_ascii_golden` and `render_mermaid_golden` — compare with `tests/fixtures/*.txt` goldens for a 4-job graph.
- `yaml_roundtrip_preserves_jobs_and_edges` — export → import → equal `NewJob`s; `yaml_import_rejects_bad_gate_and_cycle` ; `yaml_export_excludes_runtime_fields`.
- `stall_judge_matrix` — table of (silent?, idle minutes, commit/event recency) → verdict, incl. boundary 7/8 and 19/20 minutes; `stall_judge_commit_resets_idle`.
- `repeat_guard_trips_on_third_identical`, `repeat_guard_resets_on_new_fingerprint`, `repeat_guard_resets_on_new_key`.
- `attempts_left_saturates`.

## 11. Out of scope (v0.2)
Weighted/critical-path scheduling, mermaid state colours, per-edge metadata, graph diffing, multi-project graphs.
