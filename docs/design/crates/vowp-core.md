# crate `vowp-core`

Orchestration library: universal sessions (capsule, switch), the supervisor loops L1–L8 and merge queue, gates, ledger/cost, schedules (L9), promotion.
Depends on `vowp-protocol`, `vowp-store`, `vowp-graph`, `vowp-tmux`, `vowp-adapters`. Behaviour is specified in `loops.md`, `sessions.md`, `adapters.md`; this file is
**module layout, public API and tests**. Dependencies: see `conventions.md` §3. No `unsafe`, no tokio.

## 1. Modules

```
crates/vowp-core/src/
  lib.rs          pub mod error, deps, requests, session, loops, gates, ledger, schedules, promote; re-exports
  error.rs        CoreError
  deps.rs         Deps, CommandRunner, SystemRunner, GitOps, SystemGit, AdapterRegistry, Rng
  requests.rs     Request enum + (de)serialisation for CLI→supervisor kv requests
  session/        mod.rs service.rs capsule.rs seed.rs switch.rs recovery.rs bundle.rs importer.rs(optional)
                  asif.rs asif/{export,import,report}.rs
  gates/          mod.rs runner.rs report.rs cmd.rs owns.rs pushed.rs review.rs deploy.rs secret_scan.rs
  ledger/         mod.rs record.rs price.rs board.rs report.rs
  schedules/      mod.rs spec.rs engine.rs
  loops/          mod.rs supervisor.rs reconcile.rs l2_worker.rs l3_verify.rs merge_queue.rs dispatch.rs runner.rs
                  l4_planner.rs l5_keeper.rs l6_failover.rs l7_inbox.rs l8_watchdog.rs digest.rs prompts.rs
  promote/        mod.rs guards.rs bank.rs promoter.rs releases_log.rs
  agent/          mod.rs code.rs enroll.rs auth.rs gateway.rs pair.rs spool.rs dispatch.rs result.rs
  capacity/       mod.rs sampler.rs history.rs admit.rs advisor.rs brakes.rs estimates.rs fair.rs scope.rs targets.rs usage.rs
  surface/        mod.rs lease.rs native.rs orchestrator.rs mirror.rs opencode_host.rs claude_remote.rs
                  takeover.rs follow.rs
tests/            session_*.rs, gates_*.rs, ledger_*.rs, schedules_*.rs, loops_*.rs, drills_*.rs, promote_*.rs,
                  agent.rs, offload_dispatch.rs, offload_result.rs, offload_e2e.rs, capacity_measure.rs
tests/fixtures/   shared recorded fixtures are copied from the vowp-adapters fixtures
```

## 2. Foundation: `error`, `deps`, `requests`

```rust
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)] Store(#[from] vowp_store::StoreError),
    #[error(transparent)] Graph(#[from] vowp_graph::GraphError),
    #[error(transparent)] Protocol(#[from] vowp_protocol::ProtocolError),
    #[error(transparent)] Adapter(#[from] vowp_protocol::AdapterError),
    #[error(transparent)] Tmux(#[from] vowp_tmux::TmuxError),
    #[error("io {path}: {source}")]            Io { path: String, source: std::io::Error },
    #[error("git {args:?}: exit {code:?}: {stderr}")] Git { args: Vec<String>, code: Option<i32>, stderr: String },
    #[error("command failed: {0}")]            Command(String),
    #[error("capsule invalid after repair: {0}")] CapsuleInvalid(String),
    #[error("switch aborted: {0}")]            SwitchAborted(String),
    #[error("provider limited: {0:?}")]        Limited(vowp_protocol::LimitEvent),
    #[error("refused: {0}")]                   Refused(String),   // cwd allow-list, guard failures that must not be worked around
    #[error("guard failed: {name}: {detail}")] Guard { name: String, detail: String },
    #[error("not found: {0}")]                 NotFound(String),
    #[error("invalid: {0}")]                   Invalid(String),
    #[error("cancelled")]                      Cancelled,
}
impl CoreError { pub fn kind(&self) -> vowp_protocol::ErrorKind; }
impl From<CoreError> for vowp_protocol::ServiceError {}   // Limited→ServiceError::Limited, Cancelled→Cancelled, NotFound→NotFound, rest→Failed(msg)

/// Everything a loop or service needs from the outside world, injected so tests use fakes. Cheap to clone (Arcs).
#[derive(Clone)]
pub struct Deps {
    pub paths: vowp_store::NodePaths,
    pub config: std::sync::Arc<vowp_protocol::VowpConfig>,
    pub node: vowp_protocol::NodeName,
    pub clock: std::sync::Arc<dyn vowp_protocol::Clock>,
    pub log: std::sync::Arc<dyn vowp_protocol::Log>,
    pub adapters: std::sync::Arc<AdapterRegistry>,
    pub windows: std::sync::Arc<dyn vowp_tmux::WindowManager>,
    pub git: std::sync::Arc<dyn GitOps>,
    pub cmd: std::sync::Arc<dyn CommandRunner>,
    pub rng: std::sync::Arc<dyn Rng>,
}
impl Deps {
    pub fn open_store(&self) -> Result<vowp_store::Store, CoreError>;   // each thread opens its own connection
    pub fn config_now(&self) -> std::sync::Arc<vowp_protocol::VowpConfig>; // live cell; `config` stays the startup snapshot
}

pub trait Rng: Send + Sync { fn jitter_secs(&self, max: u64) -> u64; }          // SystemRng (time-seeded xorshift) / FixedRng(n) in tests

// `capacity::reload` keeps the effective config in a plain RwLock<Arc<VowpConfig>> per
// (node root, project). The L1 `config` step hashes `vowp.toml` plus `<node>/config.toml`,
// parses and validates a changed file into one complete VowpConfig, and swaps the cell. An
// invalid file keeps the previous config, logs the path and error, and opens one node-inbox
// notice. `provider_free` counts open runs against the new cap: a raise admits on the next
// tick, a lower only stops new admissions, and no run is finished or interrupted.
pub mod capacity::reload {
    pub struct ConfigCell { /* current: RwLock<Arc<VowpConfig>>, applied fingerprint */ }
    pub fn effective_config(deps: &Deps) -> std::sync::Arc<VowpConfig>;
    pub fn provider_free(config: &VowpConfig, open_runs: &[Run]) -> BTreeMap<String, u32>;
    pub fn preview(deps: &Deps, store: &Store, now: Timestamp) -> Result<ConfigPreview, CoreError>; // reads, never writes
    pub fn reload_project(deps: &Deps, store: &Store, now: Timestamp, force: bool) -> Result<ReloadOutcome, CoreError>;
    pub fn serve_reload_requests(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError>;
}

/// Maps HarnessKind (+ `[harnesses.*]` config) to a ready adapter. Fakes: `AdapterRegistry::with(kind, Arc<dyn Adapter>)`.
pub struct AdapterRegistry { /* BTreeMap<String, Arc<dyn Adapter>> keyed by "claude-code" | "opencode" | "acp" | "cli:<name>" */ }
impl AdapterRegistry {
    pub fn new() -> Self;
    pub fn from_config(cfg: &vowp_protocol::VowpConfig) -> Result<Self, CoreError>;   // builds ClaudeCodeAdapter/OpencodeAdapter/AcpAdapter/CliAdapter from [harnesses.*], tz_offset_minutes
    pub fn with(self, key: &str, adapter: std::sync::Arc<dyn vowp_protocol::Adapter>) -> Self;
    pub fn get(&self, exec: &vowp_protocol::Executor) -> Option<std::sync::Arc<dyn vowp_protocol::Adapter>>;
}

#[derive(Debug, Clone)]
pub struct CommandSpec { pub shell: Option<String> /* run via sh -c */, pub argv: Vec<String> /* used when shell is None */,
                         pub cwd: Option<std::path::PathBuf>, pub env: Vec<(String, String)>, pub timeout: std::time::Duration }
#[derive(Debug, Clone)]
pub struct CommandOutput { pub exit_code: Option<i32>, pub timed_out: bool, pub output: Vec<u8> /* stdout+stderr interleaved, capped at 4 MiB */,
                           pub duration: std::time::Duration }
impl CommandOutput { pub fn tail_lines(&self, n: usize) -> String; pub fn text(&self) -> String }
pub trait CommandRunner: Send + Sync { fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CoreError>; }
pub struct SystemRunner;   // std::process::Command, process group, kills group on timeout (adapters::proc::signal_group is reused via vowp_adapters::proc)

pub trait GitOps: Send + Sync {
    fn worktree_add(&self, repo: &Path, path: &Path, branch: &str, base: &str) -> Result<(), CoreError>;   // existing worktree/branch reused (idempotent)
    fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<(), CoreError>;
    fn branch_delete(&self, repo: &Path, branch: &str) -> Result<(), CoreError>;
    fn fetch(&self, repo: &Path, remote: &str) -> Result<(), CoreError>;
    fn diff_names(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError>;  // `git diff --name-only base...head`
    fn diff_text(&self, repo: &Path, base: &str, head: &str) -> Result<String, CoreError>;
    fn commits_between(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError>;   // short shas
    fn head_sha(&self, repo: &Path, rev: &str) -> Result<String, CoreError>;
    fn ls_remote(&self, repo: &Path, remote: &str, branch: &str) -> Result<String, CoreError>;      // raw output
    fn status_clean(&self, repo: &Path) -> Result<bool, CoreError>;
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<RebaseResult, CoreError>;
    fn rebase_abort(&self, worktree: &Path) -> Result<(), CoreError>;
    fn rebase_continue(&self, worktree: &Path) -> Result<RebaseResult, CoreError>;                   // `git rebase --continue` (GIT_EDITOR=true): lockfile path, loops.md §6 step 2
    fn add(&self, worktree: &Path, paths: &[String]) -> Result<(), CoreError>;                        // `git add -- <paths>`
    fn checkout_path(&self, worktree: &Path, rev: &str, path: &str) -> Result<(), CoreError>;        // `git checkout <rev> -- <path>`
    fn commit(&self, worktree: &Path, message: &str) -> Result<String, CoreError>;                    // returns the new short sha; message already ends with the AI-Assisted trailer
    fn reset_hard(&self, repo: &Path, rev: &str) -> Result<(), CoreError>;                            // rejected-push recovery, loops.md §6 step 6
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>, CoreError>;                    // `git diff --name-only --diff-filter=U`
    fn merge_ff_only(&self, repo: &Path, branch: &str) -> Result<(), CoreError>;
    fn push(&self, repo: &Path, remote: &str, branch: &str) -> Result<(), CoreError>;
    fn last_commit_time(&self, repo: &Path, rev: &str) -> Result<Option<Timestamp>, CoreError>;     // for stall "no new commits"
}
pub enum RebaseResult { Clean, Conflict { files: Vec<String> } }
pub struct SystemGit { /* CommandRunner */ }   // runs real `git`; tests use real git in temp repos (bare origin + clone)
```

### `requests.rs` — CLI→supervisor requests (decision D2)
Tests (`tests/requests.rs`): `request_wire_form_matches_loops_md_for_every_kind` (serialises each variant, asserts `{"kind":…,"payload":…}` and decodes back), `request_push_drain_roundtrip_via_store`, `undecodable_request_is_dropped_and_logged`, `apply_direct_retry_resolve_and_park`.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]   // wire form = the kv value: {"kind":"retry_job","payload":{"job":"graph-engine"}}
pub enum Request {
    ResolveJob { job: JobId },                       // parked -> ready (Resolved, T31); used by `vowp job retry` on a parked job
    RunLoop { name: String },                        // run that loop's step on the next tick (schedules, `Loop` action)
    RetryJob { job: JobId }, ParkJob { job: JobId, reason: String }, CancelJob { job: JobId },
    NudgeJob { job: JobId }, RestartJob { job: JobId }, ReplanJob { job: JobId }, FailJob { job: JobId, reason: String },   // keeper commands (loops.md §7)
    AnswerInbox { id: InboxId }, PauseLoop { name: String }, ResumeLoop { name: String },
    RestartWebWindow,                                // restart the optional vowp-daemon child without touching runs
    ReloadProject { project: ProjectName },          // re-open one drained project's supervisor and store
    Wake,
}
/// Stored as kv `req:<ULID>` = `serde_json::to_string(req)`; the store's `push_request(kind, payload)` receives `kind`/`payload` split from that JSON (so store and core agree byte-for-byte).
pub fn push(store: &Store, req: &Request) -> Result<RequestId, CoreError>;      // thin wrapper over Store::push_request
pub fn drain(store: &Store) -> Result<Vec<(RequestId, Request)>, CoreError>;     // take_requests + decode; undecodable rows are logged & dropped
pub fn apply_direct(deps: &Deps, store: &Store, req: &Request) -> Result<(), CoreError>;   // used by the CLI when no supervisor runs
```

## 3. `session` — spec `sessions.md`

```rust
pub struct SessionService { /* Deps, session-level locks map */ }
impl SessionService {
    pub fn new(deps: Deps) -> Self;
    pub fn create_session(&self, req: NewSession) -> Result<SessionId, CoreError>;  // fresh ULID; delegates to create_session_with_id
    pub fn create_session_with_id(&self, id: SessionId, req: NewSession, created_at: Timestamp) -> Result<SessionId, CoreError>;  // same-id import; conflict when the row or log exists, log created with create_new before the row; stamps the row and session.created with created_at
    pub fn recover(&self, store: &Store) -> Result<RecoveryReport, CoreError>;      // startup: resolve `switches` rows left in 'started', close dangling runs (sessions.md §5, crash recovery)
    pub fn switch(&self, req: &SwitchRequest) -> Result<Switch, CoreError>;         // opens Deps::open_store
    pub fn switch_with_store(&self, store: &Store, req: &SwitchRequest) -> Result<Switch, CoreError>;   // daemon: caller supplies a writable, non-creating store
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport { pub switches_completed: u32, pub switches_aborted: u32, pub runs_closed: u32, pub sessions_unlocked: u32 }
impl vowp_protocol::SessionOps for SessionService { /* create_session, run_turn, switch, write_capsule */ }
```

Delegation is explicit: `SessionService::switch` calls `switch::run_switch` (which opens
`Deps::open_store`), `SessionService::switch_with_store` calls `switch::run_switch_with_store`
over the caller's store, and `SessionService::recover` calls `recovery::recover`. The
`SessionOps::switch` impl calls the inherent `SessionService::switch`, so the trait, the CLI and
the loops share one switch implementation. The daemon is the store-injected caller: it opens the
existing home with `Store::open_writable` and passes that handle, so a web switch has the same
rows and events as the CLI but cannot create or migrate a home.

`run_turn` algorithm (numbered, normative):
1. Check `cwd` against the allow-list (`Refused` otherwise). Resolve adapter via `AdapterRegistry::get`; missing → `ServiceError::Failed("harness not installed")`.
2. Open the session's `SessionLog` (lockfile; a second concurrent writer → `Conflict`). Append **first** `session.executor` (attach; every run, not only the first on an executor) and **then** `turn.started` (turn = last turn + 1); the attach carries `native_session_id` from `session_natives` if resuming the same harness; if the session has no native session on this harness and a capsule exists, the prompt is `seed_prompt(capsule, tail, tail_turns, prompt)` where `tail` = the events of the last `[capsule].tail_turns` complete turns; else `prompt`.
3. Append the `user.message`; insert the `runs` row (`started_at=now`, `purpose`, `profile`, `rung`, `window`); `Adapter::start(StartInput{ resume, prompt, cwd, env: configured env_pass values, … })`.
4. Loop on `run.events()` with `recv_timeout(200 ms)`: append each event through `SessionLog::append` (blob spill >8192 B), update `sessions.last_seq`/`updated_at` and `session_events` index every 20 events and at the end; poll `cancel` → `interrupt()`.
5. `wait()` → `RunSummary`; append, **in this order** (sessions.md §1 rule 8): `cost` event (if cost known), then `limit` (if any) or `error` (if any), then `turn.ended` (stop_reason from summary), then `session.executor` detach carrying `summary.native_session_id` (also written to `session_natives` / `adapters.json`); `sync()`. (Adapters already emit `cost`/`limit`/`error` before their terminal `TurnEnded`; the session layer forwards them unchanged and only appends the ones the adapter did not emit.)
6. Finish the run row (`finish_run`), upsert/clear `limits`, write the ledger rows (`ledger::record_run_cost`, price-table fallback when no receipt), return `TurnResult`. A `Limit/Credits/Auth/Context/Rate` outcome is **returned**, not raised: callers (loops) decide.

```rust
pub mod capsule {
    pub struct CapsuleWriter { /* Deps */ }
    impl CapsuleWriter {
        pub fn new(deps: Deps) -> Self;
        /// Compacts input, runs the capsule executor, validates, repairs once, stores file + row + `capsule` event. sessions.md §3.
        pub fn write(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, CoreError>;
        pub fn build_prompt(&self, last: Option<&Capsule>, events: &[EventEnvelope], from: &ExecutorRef) -> String;   // pure, golden-tested
        pub fn parse_draft(&self, text: &str) -> Result<CapsuleDraft, CoreError>;    // tolerates ``` fences and leading prose; first JSON object
    }
}
pub mod seed { pub fn seed_prompt(capsule: &Capsule, tail: &[EventEnvelope], tail_turns: u32, next_input: &str) -> String; }   // sessions.md §4
pub mod switch {
    /// Sleeps between bounded drain polls; tests inject one that advances a `FakeClock`.
    pub trait DrainSleeper: Send + Sync { fn sleep(&self, duration: std::time::Duration); }
    /// Production sleeper: blocks the current thread. [`run_switch`] uses it.
    pub struct SystemSleeper;
    pub fn run_switch(deps: &Deps, req: &SwitchRequest, cancel: &CancelToken) -> Result<Switch, CoreError>;        // sessions.md §5; opens Deps::open_store
    pub fn run_switch_with_store(deps: &Deps, store: &Store, req: &SwitchRequest, cancel: &CancelToken) -> Result<Switch, CoreError>;   // daemon path; uses the supplied store
    pub fn run_switch_with_sleeper(deps: &Deps, req: &SwitchRequest, cancel: &CancelToken, sleeper: &dyn DrainSleeper) -> Result<Switch, CoreError>;   // public test hook for deterministic drain deadlines
    pub fn run_switch_with_input(deps: &Deps, req: &SwitchRequest, next_input: &str, cancel: &CancelToken) -> Result<Switch, CoreError>;   // seeded failover/context variant
    // Drain waits are bounded by DrainDeadline (injected clock plus a monotonic backstop).
}
pub mod bundle {
    pub const BUNDLE_FORMAT: &str = "vowp-bundle-v1";
    pub struct Bundle { /* fields per sessions.md §6.1 */ }
    pub fn export(deps: &Deps, session: &SessionId, tail: usize) -> Result<Bundle, CoreError>;
    pub fn to_json(b: &Bundle) -> Result<String, CoreError>;  pub fn from_json(s: &str) -> Result<Bundle, CoreError>;  // verifies checksum + format
    pub enum ImportMode { Child, SameId }
    pub fn import(deps: &Deps, b: &Bundle, executor: Option<ExecutorRef>, cwd: Option<&std::path::Path>) -> Result<SessionId, CoreError>;   // child session; cwd = `vowp session import --cwd`
    pub fn import_with_mode(deps: &Deps, b: &Bundle, executor: Option<ExecutorRef>, cwd: Option<&std::path::Path>, mode: ImportMode) -> Result<SessionId, CoreError>;   // SameId: origin id only when absent, no parent, copies the tail with contiguous local seqs
}
pub mod importer { /* optional (nice-to-have): ClaudeTranscriptImporter::scan_once(dir) -> ImportReport, read-only, incremental by byte offset; parses each transcript file with vowp_adapters::claude_code::import_transcript(path, tz_offset_minutes) (crates/vowp-adapters.md §5) */ }
pub mod asif {
    pub const MAX_ASIF_ARCHIVE_BYTES: u64;   // the pinned adapter's MAX_ARCHIVE_BYTES
    #[derive(Default)] pub struct AsifExportOptions {
        pub redact: Vec<String>, pub keep_host_paths: bool, pub strict: bool,
        pub allow_loss: bool, pub report_path: Option<PathBuf>,
    }
    pub struct AsifExport { pub output: PathBuf, pub bytes: u64, pub report: AsifReport }
    pub fn export_asif(paths: &NodePaths, session: &SessionId, out: &Path,
                       opts: &AsifExportOptions) -> Result<AsifExport, CoreError>;
    pub struct AsifImportOptions {
        pub cwd: PathBuf, pub branch: Option<String>, pub strict: bool,
        pub allow_loss: bool, pub session_id: Option<SessionId>,
        pub report_path: Option<PathBuf>,
    }
    pub struct AsifImport { pub session: Session, pub report: AsifReport,
                            pub warnings: Vec<String> }
    pub fn import_asif(paths: &NodePaths, store: &Store, input: &Path,
                       opts: &AsifImportOptions) -> Result<AsifImport, CoreError>;
}
```

`session::asif` is the only VOWP code that calls the pinned `vowp-asif` library; the library is a
dependency of `vowp-core` only. Export writes one deterministic `.asif.zip` through a private
`<home>/tmp/asif-<ULID>/` directory, checks the adapter report's source session, applies the VOWP
strict filter (`Blocking`/`Integrity` only; the adapter always runs with `strict: false`), and
renames the package to the requested output. Import passes a pre-chosen fresh ULID to the adapter,
checks the report target, runs the same strict filter, pre-checks the staged log/capsules/adapters
file (`session.created` first, contiguous sequences, clean tail, globally unique event ids),
rejects a target row or path entry, renames the stage into `sessions/<id>/`, reindexes, and keeps
only the expected `project ... left null` / `job ... left null` warnings. Every failure before the
rename removes the staging directory and leaves no row; a reindex failure removes the new session
directory. `CoreError::Asif` carries the adapter's stable code and maps it to `InvalidInput`,
`Conflict`, `Refused` or `Internal`; messages stay path-free. No export or import starts a turn,
creates a lease, writes a capsule or claims continuation.

Tests (`tests/asif.rs`, hermetic temp `NodePaths` and fixture packages built with
`vowp_asif::pack`): `asif_export_writes_a_deterministic_package_and_summary`,
`asif_export_redacts_exact_literal_and_never_echoes_it`,
`asif_export_strict_refuses_blocking_loss_and_removes_output`,
`asif_export_keeps_inherent_losses_non_fatal`,
`asif_import_creates_one_indexed_session_and_no_turn`,
`asif_import_round_trip_preserves_text_blobs_and_event_order`,
`asif_import_invalid_package_leaves_no_directory_or_row`,
`asif_import_existing_id_is_a_conflict_and_changes_nothing`,
`asif_import_same_package_twice_conflicts_on_event_ids_and_keeps_the_first_session`,
`asif_import_duplicate_event_id_conflicts_without_a_partial_index`,
`asif_import_strict_removes_staging_and_reports_the_loss`,
`asif_import_allow_loss_accepts_a_recoverable_integrity_fixture`,
`asif_import_multiple_branches_requires_a_selection`,
`asif_import_selected_branch_maps_only_that_branch`,
`asif_adapter_version_and_pin_match_the_documented_contract`.

Tests (hermetic: temp `NodePaths`, `FakeClock`, `AdapterRegistry::with("opencode", FakeAdapter)` where `FakeAdapter` replays a fixture through the real `Adapter` trait — defined in `crates/vowp-core/tests/support/fake_adapter.rs`):
`run_turn_appends_events_and_finishes_run`, `run_turn_frames_attach_then_turn_started_once_per_run_without_adapter_duplicates` (real parsers over claude/opencode fixtures: exactly one `session.executor` attach, one `turn.started`, order attach → turn.started → user.message), `limit_resets_at_gets_single_two_minute_margin` (fixture `|1794164400` → `limits.resets_at = 19:02:00Z`), `run_turn_resumes_native_session_same_harness`, `run_turn_seeds_capsule_on_new_harness`, `run_turn_cwd_outside_allow_list_refused`, `run_turn_cancel_interrupts_adapter`,
`run_turn_limit_outcome_returned_not_raised_and_limits_row_written`, `run_turn_large_tool_output_spilled_to_blob`, `second_writer_on_same_session_conflicts`,
`capsule_prompt_golden`, `capsule_parse_draft_with_fences_and_prose`, `capsule_write_valid_first_try`, `capsule_compaction_keeps_first_user_and_last_20` (oversize input drops tool results first; never the first user message or the last 20 events), `capsule_write_repairs_once_then_ok`, `capsule_write_twice_invalid_keeps_previous_and_logs_error_event`,
`capsule_debounce_120s_except_manual_switch_limit_export`, `seed_prompt_golden_with_two_turn_tail`, `seed_prompt_never_contains_tool_calls_as_native`,
`switch_happy_path_events_rows_and_lineage`, `switch_target_probe_fails_stays_on_source`, `switch_drain_timeout_interrupts_after_5s_fakeclock`, `switch_seed_rejected_retries_capsule_only_then_aborts`,
`switch_with_store_guards_from_the_supplied_store_without_opening_deps`, `switch_with_store_completes_on_the_supplied_store`,
`recover_started_switch_never_attached_is_aborted`, `recover_started_switch_attached_is_completed`, `bundle_export_import_roundtrip_child_session`, `bundle_checksum_mismatch_rejected`, `bundle_unknown_format_rejected`, `bundle_inlines_small_blobs_only`,
`same_id_import_keeps_the_origin_id_without_a_parent`, `same_id_import_refuses_an_existing_session_and_changes_nothing`, `same_id_import_rejects_a_duplicate_event_id_before_writing`, `same_id_import_rejects_a_corrupt_bundle_before_writing`, `child_import_still_creates_a_child_with_the_parent`,
`failover_roundtrip_claude_to_opencode_and_back_preserves_capsule_content` (end-to-end with two fake adapters).

`tests/switch_drain.rs` covers the bounded-drain hook: `drain_is_bounded_when_writer_never_released`
(an unowned writer makes the switch abort within the deadline), `drain_is_bounded_when_runner_never_finishes`
(a runner that ignores INT/TERM still returns `SwitchAborted`), `drain_releases_promptly_when_runner_finishes`
(the positive path returns well inside the deadline), and `recover_switch_event_carries_recovered_capsule_id`
(recovery links the capsule it recovered on both the event and the row).

## 4. `gates` — spec `loops.md` §4

```rust
pub struct GateCtx<'a> { pub deps: &'a Deps, pub job: &'a Job, pub run_id: Option<RunId>, pub worktree: &'a Path, pub repo: &'a Path,
                         pub store: &'a Store, pub attempt: u32 }
pub struct GateRunner { /* Deps */ }
impl GateRunner {
    pub fn new(deps: Deps) -> Self;
    pub fn evaluate(&self, ctx: &GateCtx, index: i32, gate: &Gate) -> Result<GateResult, CoreError>;   // never panics on bad input: failures are `passed=false` + evidence
    /// All gates of the job in `gates_json` order, **continuing after a failure** (so the fix prompt lists every finding) except that a failed `report` gate short-circuits the rest; `review` is not evaluated here (L3 does it). Persists each result (`insert_gate_result`).
    pub fn evaluate_all(&self, ctx: &GateCtx) -> Result<GatesOutcome, CoreError>;
    /// Repo-wide check for the merge queue (index -1): project check_cmds + secret_scan on the rebased tree.
    pub fn evaluate_merge_checks(&self, ctx: &GateCtx) -> Result<Vec<GateResult>, CoreError>;
}
pub struct GatesOutcome { pub results: Vec<GateResult>, pub all_passed_before_review: bool, pub first_failure: Option<usize> /* index of the first failed gate */, pub needs_review: bool }
pub mod secret_scan {
    pub struct Rule { pub name: &'static str, pub regex: &'static str }
    pub fn rules() -> &'static [Rule];                                   // exactly the 7 content rules of loops.md §4 "Secret rules", in that order: sk_api_key, supabase_secret, github_token, github_pat, aws_access_key, private_key_block, slack_token
    pub fn scan_text(path: &str, text: &str) -> Vec<SecretHit>;           // hits carry rule NAME + line only
    pub fn scan_paths(paths: &[String]) -> Vec<SecretHit>;                // rule `secret_file` (line 0) for .env*, *.pem, id_rsa*, id_ed25519*
    pub fn scan_diff(diff: &str) -> Vec<SecretHit>;                       // only added lines (`+`), path from `+++ b/…`
}
pub fn parse_report_file(path: &Path) -> Result<Report, CoreError>;      // thin wrapper, for gates + loops
```
Tests: `report_gate_passes_with_required_fields`, `report_gate_fails_missing_field_lists_names`, `report_gate_missing_file_fails_with_evidence`, `report_gate_status_partial_fails_done_requirement` (a `report` gate requires `status: done` for `done`; `partial|blocked` never advance a job), `cmd_gate_exit_zero_passes`, `cmd_gate_wrong_exit_fails_with_tail`, `cmd_gate_expect_pattern`, `cmd_gate_timeout_kills_group_and_fails` (sleep 30 with timeout 1 s), `cmd_gate_tail_is_last_200_lines`,
`owns_respected_pass_and_fail_with_real_git`, `owns_respected_allows_always_allowed_cargo_lock`, `pushed_gate_against_local_bare_remote`, `pushed_gate_fails_when_branch_absent`,
`review_gate_reads_verifier_report_verdict`, `review_gate_pending_when_no_verify_job_yet`, `secret_scan_detects_each_rule_by_exact_name_and_never_stores_value`, `secret_scan_flags_secret_file_paths`, `secret_scan_only_added_lines`, `deploy_gate_runs_command_then_probe`, `deploy_gate_probe_mismatch_fails`,
`file_exists_only_gate_rejected_by_validate` (anti-pattern ban), `evaluate_all_continues_after_failure_except_report_and_persists_results`, `evaluate_merge_checks_uses_project_check_cmds`.

## 5. `ledger`

```rust
pub struct Ledger { /* Deps */ }
impl Ledger {
    pub fn new(deps: Deps) -> Self;
    pub fn record(&self, store: &Store, entry: LedgerEntry) -> Result<i64, CoreError>;
    pub fn record_dispatch(&self, store: &Store, job: &Job, run: &Run) -> Result<(), CoreError>;
    /// One `cost` row per run (with `role = run.role`, `provider`, `harness`, `model` copied so `cost_totals` never needs a join): receipt cost from the summary, else `price::estimate`, else `unknown`. For claude-code runs
    /// `detail_json = {"billed":false,"basis":"list"}` (subscription; adapters.md §6).
    pub fn record_run_cost(&self, store: &Store, run: &Run, summary: &RunSummary) -> Result<Cost, CoreError>;
    pub fn record_gate(&self, store: &Store, r: &GateResult) -> Result<(), CoreError>;
    pub fn record_state(&self, store: &Store, job: &JobId, from: JobState, to: JobState, why: &str) -> Result<(), CoreError>;
}
pub mod price {
    pub fn estimate(cfg: &VowpConfig, exec: &Executor, tokens: &Tokens) -> Cost;   // PriceTable if `[providers.p.prices."model"]` present (model key = text after '/' for opencode), else Cost::unknown()
}
pub struct BoardSnapshot { pub project: ProjectName, pub at: Timestamp, pub jobs_by_state: BTreeMap<JobState, u32>, pub running: Vec<BoardRun>,
    pub slots_busy: u32, pub slots_total: u32, pub planner_executor: Option<Executor>, pub keeper_executor: Option<Executor>, pub limits: Vec<Limit>,
    pub cost_today: Cost, pub inbox_open: u32, pub last_checkpoint: Option<Timestamp> }
pub struct BoardRun { pub job: JobId, pub executor: Option<Executor>, pub age_secs: u64, pub last_event_age_secs: Option<u64> }
impl BoardSnapshot { pub fn collect(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<Self, CoreError>; pub fn render_text(&self, color: bool) -> String; }
pub mod report {
    pub fn night_report(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<String, CoreError>;   // Markdown summary: finished/failed jobs and why, cost from receipts, limit/failover counts, switches, inbox
    pub fn checkpoint(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<String, CoreError>;      // short; written to .vowp/checkpoints/<ts>.md by L4
}
```
Tests: `record_run_cost_receipt_vs_price_table_vs_unknown`, `price_estimate_math_per_million`, `claude_cost_marked_not_billed`, `ledger_rows_are_append_only_via_store_trigger`, `board_snapshot_counts_and_render_golden`, `night_report_lists_limits_and_switches_golden`, `cost_totals_never_count_unknown_as_zero`.

## 6. `schedules` — spec `loops.md` §11

```rust
pub struct ScheduleSpec { /* parsed Cron(cron::Schedule + offset) | Interval(Duration) */ }
impl ScheduleSpec {
    pub fn parse(kind: ScheduleSpecKind, spec: &str, tz: &str) -> Result<Self, CoreError>;     // cron: 5 fields -> prefix "0 " seconds field for the `cron` crate
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp>;
}
pub struct ScheduleEngine { /* Deps */ }
impl ScheduleEngine {
    pub fn new(deps: Deps) -> Self;
    pub fn sync_from_config(&self, store: &Store, cfg: &VowpConfig) -> Result<(), CoreError>;       // upsert [[schedules]] rows by name; compute next_run_at
    pub fn catch_up(&self, store: &Store, now: Timestamp) -> Result<Vec<DueRun>, CoreError>;        // startup: missed runs per missed_policy (run_once → one DueRun, skip → none) and re-arm
    pub fn tick(&self, store: &Store, now: Timestamp) -> Result<Vec<DueRun>, CoreError>;            // due rows; advances last_run_at/next_run_at BEFORE firing (idempotency key = schedule id + due time); `fire` writes the ledger row before the action (loops.md §11)
    pub fn fire(&self, store: &Store, due: &DueRun) -> Result<FireOutcome, CoreError>;               // JobTemplate -> insert job (suffix -yyyymmddhhmm); Loop -> push Request::RunLoop{name} (the request handler sets loops.next_run_at=now); Command -> CommandRunner
}
pub struct DueRun { pub schedule: Schedule, pub due_at: Timestamp }
pub enum FireOutcome { JobCreated(JobId), LoopTriggered(String), CommandRan { exit_code: Option<i32> }, Skipped(String) }
```
Tests (all with `FakeClock`): `cron_five_field_next_runs_golden`, `cron_weekday_window`, `interval_every_30m`, `fixed_offset_tz_shifts_next_run`, `bad_cron_rejected`, `six_field_cron_rejected`, `tick_fires_due_and_advances_next_before_fire`, `double_tick_same_instant_fires_once`, `catch_up_run_once_after_three_missed_fires_once`, `catch_up_skip_fires_none_but_rearms`, `job_template_creates_job_with_suffix`, `command_action_runs_with_timeout`, `paused_schedule_never_fires`, `proptest_next_after_is_strictly_greater_and_matches_cron`.

## 7. `loops` — spec `loops.md`

```rust
pub struct Supervisor { /* Deps, project, Store, RunnerPool, GateRunner, Ledger, ScheduleEngine, SessionService, state */ }
impl Supervisor {
    pub fn new(deps: Deps, project: ProjectName) -> Result<Self, CoreError>;
    /// Startup reconciliation (loops.md §1 step 1): orphaned runs → crash, dead windows, `switches` recovery, schedule catch-up, loop rows created from defaults+config.
    pub fn startup(&mut self) -> Result<ReconcileReport, CoreError>;
    /// One full L1 tick, ordered steps 0–13 of loops.md §1. A failing step is logged and counted; the tick continues.
    pub fn tick(&mut self) -> Result<TickReport, CoreError>;
    /// startup() then tick() every `tick_secs` or on wake, until `cancel`; on exit interrupts runner threads and marks runs `stalled`.
    pub fn run(&mut self, cancel: &CancelToken) -> Result<(), CoreError>;
    pub fn wake_handle(&self) -> WakeHandle;     // Sender<()> clone for runner threads
}

/// One supervisor for every active project on the node. It owns the node-level hooks, the
/// registry reload, the drain marker and the optional web-window restart request.
pub struct NodeSupervisor { /* Deps, Store, projects, rotation, wake */ }
impl NodeSupervisor {
    pub fn new(deps: Deps) -> Result<Self, CoreError>;
    pub fn startup(&mut self) -> Result<NodeReconcileReport, CoreError>;
    /// One node pass. While `<home>/drain` exists every active project is forced into
    /// `ProjectTickMode::Maintenance`: harvest/gates/merge/inbox/snapshot/heartbeat run,
    /// dispatch/triggers/probes/schedules do not.
    pub fn tick(&mut self) -> Result<NodeTickReport, CoreError>;
    pub fn run(&mut self, cancel: &CancelToken) -> Result<(), CoreError>;
    /// Run the loop and call `after_tick` after each pass (the CLI restarts the web window here).
    pub fn run_with<F>(&mut self, cancel: &CancelToken, after_tick: F) -> Result<(), CoreError> where F: FnMut(&mut Self);
    pub fn drain_requested(&self) -> bool;       // <home>/drain exists
    pub fn active_children(&self) -> usize;      // runner threads + gate/merge tasks across projects
    pub fn drain_complete(&self) -> bool;        // drain requested, zero children, no run awaiting harvest
    pub fn drained(&self) -> bool;               // run_with exited because the drain completed
    pub fn take_web_window_restart(&mut self) -> bool;
    pub fn shutdown(&mut self) -> Result<(), CoreError>;
    pub fn project(&self, name: &ProjectName) -> Option<&ProjectSupervisor>;
}
pub struct TickReport { pub at: Timestamp, pub steps: Vec<StepReport>, pub dispatched: Vec<JobId>, pub transitions: Vec<(JobId, JobState, JobState)>, pub merged: Vec<JobId> }
/// Cloneable wake-up handle for L1: `wake()` sends `()` on an `mpsc::Sender<()>`; the loop `recv_timeout(tick)`s on the receiver. Never blocks.
#[derive(Clone)] pub struct WakeHandle(/* mpsc::Sender<()> */);
impl WakeHandle { pub fn wake(&self); pub fn channel() -> (WakeHandle, std::sync::mpsc::Receiver<()>); }
pub struct StepReport { pub name: &'static str, pub ok: bool, pub detail: String }
pub struct ReconcileReport { pub orphaned_runs: u32, pub switches_recovered: u32, pub schedules_caught_up: u32, pub windows_recreated: u32 }

/// Runner threads: one per active run; each opens its own Store; results are written to the DB then `Wake`.
pub struct RunnerPool { /* JoinHandles + CancelTokens by RunId */ }
impl RunnerPool { pub fn spawn(&mut self, job: Job, choice: vowp_graph::DispatchChoice, deps: Deps, wake: WakeHandle) -> Result<RunId, CoreError>; pub fn cancel(&self, run: &RunId); pub fn cancel_all(&self); pub fn active(&self) -> usize; }

pub mod l2 { pub fn start_prompt(job: &Job, brief: &str) -> String; pub fn continue_prompt(job: &Job) -> String; pub fn fix_prompt(job: &Job, findings: &[String]) -> String; }   // text templates: loops.md §3 (golden-tested)
pub mod l3 { pub fn on_verify_done(store: &Store, verify_job: &Job) -> Result<JobEvent, CoreError>;   // reads the verify report, writes the review GateResult for the original, returns VerifyPassed|VerifyFailed (T14/T19–T21)
    pub fn verify_prompt(job: &Job, brief: &str, evidence: &[GateResult], diff_summary: &str) -> String; pub fn parse_verdict(report: &Report) -> Result<(Verdict, Vec<String>), CoreError>; }
pub mod merge_queue { pub struct MergeQueue { /* Deps, GateRunner */ } impl MergeQueue { pub fn step(&self, store: &Store) -> Result<MergeStep, CoreError>; } pub enum MergeStep { Idle, Merged(JobId), Conflict { job: JobId, files: Vec<String> }, ChecksFailed { job: JobId }, InProgress } }
pub mod l6 {   // failover
    pub struct Failover { /* Deps */ }
    impl Failover {
        pub fn on_run_outcome(&self, store: &Store, run: &Run, summary: &RunSummary) -> Result<FailoverAction, CoreError>;   // writes `limits`, inbox, ledger; pure decision otherwise
        pub fn probe_due(&self, store: &Store, now: Timestamp) -> Result<Vec<String /*provider*/>, CoreError>;
        pub fn run_probe(&self, store: &Store, provider: &str) -> Result<ProbeVerdict, CoreError>;                              // one tiny turn via the adapter
    }
    /// Result of one probe run: `Available` clears the `limits` row (and triggers probe-back), `StillLimited{resets_at}` re-arms it, `Failed(detail)` counts toward `probe_fail_streak` (D15).
    pub enum ProbeVerdict { Available, StillLimited { resets_at: Option<Timestamp> }, Failed(String) }
    pub enum FailoverAction { None, Backoff { until: Timestamp }, NextRung { rung: usize }, ParkProvider { provider: String }, ParkJob { reason: ParkedReason }, FreshSessionWithCapsule, ProbeBack { provider: String } }
}
/// Input of `open_inbox`. `job_id = Some` parks that job with `ParkedReason::Question` for kinds Question/Approval (never Notice). `body` ≤ 4000 chars.
pub struct NewInboxItem { pub project: Option<ProjectName>, pub job_id: Option<JobId>, pub kind: InboxKind, pub title: String, pub body: String, pub options: Vec<String> }
pub mod l7 { pub fn open_inbox(store: &Store, deps: &Deps, item: NewInboxItem) -> Result<InboxId, CoreError>; pub fn on_answer(store: &Store, id: &InboxId) -> Result<(), CoreError>; }   // writes inbox/<id>.md copy too
pub mod l8 { pub fn write_heartbeat(deps: &Deps) -> Result<(), CoreError>; pub fn heartbeat_age(paths: &NodePaths, now: Timestamp) -> Option<std::time::Duration>; pub const SYSTEMD_UNIT: &str; pub const CRON_CHECK_SH: &str; }  // text templates installed by the install script
pub mod digest {   // loops.md §7 "Digest"; ≤ max_chars (default 24_000), each item ≤ 600 chars
    pub struct Digest;
    impl Digest {
        pub fn build(store: &Store, cfg: &VowpConfig, role: Role /* Planner|Keeper */, since: Timestamp, max_chars: usize) -> Result<String, CoreError>;
    }
}
```
Named tests live in `loops.md` §13 (drills) plus: `tick_step_failure_does_not_abort_tick`, `startup_marks_orphaned_runs_crash_and_continues_job`, `dispatch_respects_ownership_locks_and_slots`, `dispatch_skips_limited_provider_and_uses_next_rung`, `stall_judge_triggers_stop_and_continue`, `hang_before_first_output_restart_up_to_3`, `continue_twice_then_keeper`, `verify_fail_creates_fix_job_same_branch_max_3_then_escalates`, `verify_job_goes_done_via_gating_never_merged_and_original_receives_verdict`, `failed_fix_job_gates_fail_original_not_fix_of_fix`, `failed_verify_job_parks_original_with_question`, `merge_queue_happy_path_ff_push_to_local_bare_origin`, `merge_queue_conflict_creates_merge_job`, `merge_queue_cargo_lock_regenerated_not_merged`, `merge_queue_check_failure_returns_job_to_fixing`, `failover_drill_*`, `credits_park_all_jobs_of_provider_and_inbox`, `credits_park_parks_verify_and_fix_child_jobs_and_leaves_waiting_parents`, `rate_backoff_lowers_concurrency_cap`, `auth_parks_with_inbox`, `three_unknown_failures_mark_unavailable_15min`, `inbox_answer_moves_parked_job_to_ready`, `planner_one_turn_at_a_time`, `keeper_covers_planner_when_provider_limited_and_hands_back`, `repeat_guard_trips_on_third_identical_action`, `board_and_heartbeat_written_each_tick`, `requests_drained_and_applied`, `request_rows_apply_every_kind_end_to_end` (push one row of each kind through `requests::push`, run `tick`, assert the resulting job/loop/inbox state and that the rows are deleted), `paused_loop_is_skipped`.

## 8. `promote` — optional

```rust
pub struct Promoter { /* Deps, GateRunner */ }
impl Promoter {
    pub fn new(deps: Deps) -> Self;
    /// design summary §9: guards → serving bank → diff candidate vs serving → approval → deploy → probe → record. Returns the release in its final state for this call
    /// (`Approved`→`Live`, or stopped at `Guarded`/`Banked` awaiting an inbox approval, or `Refused`/`Failed`). Re-entrant: called again after the approval is answered.
    pub fn promote(&self, store: &Store, project: &ProjectName, env: &str, change: &str, opts: PromoteOpts) -> Result<Release, CoreError>;
    pub fn rollback(&self, store: &Store, release: &ReleaseId, opts: PromoteOpts) -> Result<Release, CoreError>;      // runs the banked rollback_cmd
    pub fn releases(&self, store: &Store, project: &ProjectName, env: Option<&str>, limit: u32) -> Result<Vec<Release>, CoreError>;
}
pub struct PromoteOpts { pub dry_run: bool, pub yes: bool /* only honoured when env.approval == policy */, pub approved_by: Option<String> }
pub mod guards {
    pub const BUILTIN: &[&str] = &["clean_tree", "head_equals_remote", "build", "no_active_jobs", "upload_set"];
    pub fn run_guard(deps: &Deps, env: &EnvConfig, name: &str, checkout: &Path) -> Result<GuardResult, CoreError>;   // names with ":cmd" suffix run a custom script: "no_active_jobs:./scripts/guard.sh"
}
pub mod bank { pub fn bank_serving(deps: &Deps, env: &EnvConfig, target: &str, to: &Path) -> Result<BankedServing, CoreError>; pub struct BankedServing { pub path: PathBuf, pub commit: Option<String>, pub rollback_cmd: String } }
```
Rules: (1) a failing guard ⇒ `Release.state = Refused`, ledger `promote`, exit 5 at the CLI; **no retry, no workaround** (design summary §9). (2) the diff between candidate and the banked serving copy must equal the files of the intended change; otherwise `Refused` ("live is not always git"). (3) the `upload_set` guard runs the project's upload-set check (e.g. `.vercelignore` listing) via `env.probe`-style custom guard: it exists to catch "a local build is not the deploy build". (4) approval `human` ⇒ inbox `approval` item, state `Banked` until answered; the supervisor calls `promote` again on answer. (5) `releases.log` line appended on every terminal state: `<ts> <env> <release id> <state> <from>-><to> rollback: <cmd>`.
Tests (temp git repos; deploy/bank/probe are fake shell scripts writing to temp files): `guards_clean_tree_dirty_fails`, `guards_head_equals_remote_behind_fails`, `guard_failure_marks_refused_and_never_deploys`, `serving_bank_runs_before_deploy_order_asserted_via_script_log`, `diff_unexpected_files_refused`, `human_approval_creates_inbox_and_waits`, `answer_approve_then_promote_goes_live_and_records_rollback_cmd`, `policy_approval_with_yes_deploys`, `yes_flag_never_bypasses_human_approval`, `probe_mismatch_marks_failed_with_rollback_available`, `rollback_runs_banked_command_and_links_releases`, `releases_log_lines_appended`, `dry_run_runs_guards_only`.

## 9. `agent` — v0.4 home side: enrollment, auth, surface pairing, lease control and offload dispatch

Spec: `docs/design/v0.4/local-orchestrator.md` §4.1–§4.6, §6.3, §7.1–§7.5, §8.2–§8.3, §9
and §12.1. The gateway runs as a home command with the same store access as `session ingest`;
Phase 1 adds the surface-token pairing service and the lease backend on the same authenticated
connection, Phase 2 adds the offload dispatcher and result return, and Phase 4 adds the rotation
policy, the durable envelope and the bundle result. The WSS listener in `vowp-agent` calls the
same `gateway::serve`, so the home has one frame path for SSH stdio and WSS.

```rust
pub mod agent {
    pub mod code {
        pub const ENROLLMENT_CODE_TTL: Duration;                    // 10 minutes
        pub struct NewEnrollmentCode { pub code: String, pub code_hash: String, pub expires_at: Timestamp }
        pub fn new_enrollment_code(now: Timestamp) -> NewEnrollmentCode;   // two ULIDs, SHA-256 hex
        pub fn hash_code(code: &str) -> String;                     // lowercase hex SHA-256
        pub fn has_code_shape(code: &str) -> bool;                  // 52 ASCII alphanumerics
    }
    pub mod enroll {
        pub struct PendingEnrollment { pub node: NodeName, pub code: String, pub code_hash: String, pub expires_at: Timestamp }
        pub struct EnrollRequest { pub code: String, pub public_key: String, pub agent_version: String,
                                   pub capabilities: AgentCapabilities, pub paths: Vec<AgentPathMap> }
        pub struct Enrolled { pub node: NodeName }
        pub fn create_pending(store: &Store, node: &NodeName, labels: &[String], now: Timestamp) -> Result<PendingEnrollment, CoreError>;
        pub fn redeem(store: &Store, request: &EnrollRequest, now: Timestamp) -> Result<Enrolled, CoreError>;
        pub fn revoke(store: &Store, node: &NodeName, now: Timestamp) -> Result<u64, CoreError>;
    }
    pub mod auth {
        pub fn issue_nonce() -> String;
        pub fn handshake_payload(nonce: &str, home_id: &str, connection_id: &str) -> Vec<u8>;   // nonce\nhome\nconnection
        pub fn verify(public_key_hex: &str, signature_hex: &str, nonce: &str, home_id: &str, connection_id: &str) -> bool;
        pub fn is_public_key_hex(value: &str) -> bool;               // 32 bytes, lowercase hex
        pub fn is_signature_hex(value: &str) -> bool;                // 64 bytes, lowercase hex
    }
    pub mod pair {
        pub const SURFACE_CODE_TTL: Duration;                       // 10 minutes
        pub const SURFACE_TOKEN_TTL: Duration;                      // 30 days
        pub const SURFACE_TOKEN_OVERLAP: Duration;                  // 10 minutes
        pub const SURFACE_TOKEN_MAX_GRACE: Duration;                // 24 hours
        pub struct RotationPolicy { pub grace: Duration, pub wait: Option<Duration> }
        pub struct SurfaceRotation { pub rotated: RotatedSurface, pub queued_at: Timestamp,
                                     pub old_expires_at: Timestamp, pub delivered: bool }
        pub struct SurfaceListRow { pub surface: SurfaceInfo, pub scopes: Vec<SurfaceScope>,
                                    pub prefix: Option<String>, pub expires_at: Option<Timestamp>,
                                    pub overlap_until: Option<Timestamp>, pub pending: bool,
                                    pub queued_at: Option<Timestamp>, pub delivery_deadline: Option<Timestamp> }
        pub struct NewSurfaceCode { pub code: String, pub code_hash: String, pub expires_at: Timestamp }
        pub struct PendingSurface { pub surface: SurfaceInfo, pub code: String, pub code_hash: String, pub expires_at: Timestamp }
        pub struct IssuedSurface { pub surface_id: SurfaceId, pub token: String, pub scopes: Vec<SurfaceScope>, pub expires_at: Timestamp }
        pub struct RotatedSurface { pub surface: SurfaceInfo, pub token: String, pub prefix: String, pub scopes: Vec<SurfaceScope>, pub expires_at: Timestamp, pub old_expires_at: Timestamp, pub node: NodeName }
        pub struct VerifiedSurface { pub surface: SurfaceInfo, pub token: SurfaceToken }
        pub fn create_pending_surface(store: &Store, node: &NodeName, session: &SessionId, kind: SurfaceKind, app: Option<&str>, now: Timestamp) -> Result<PendingSurface, CoreError>;
        pub fn new_surface_code(store: &Store, node: &NodeName, surface_id: &SurfaceId, now: Timestamp) -> Result<NewSurfaceCode, CoreError>;
        pub fn issue_surface_token(store: &Store, node: &NodeName, surface_id: &SurfaceId, session: &SessionId, app: &str, code: &str, now: Timestamp) -> Result<IssuedSurface, CoreError>;
        pub fn rotate_surface_token(store: &Store, deps: &Deps, surface_id: &SurfaceId, grace: Duration, now: Timestamp) -> Result<RotatedSurface, CoreError>;
        pub fn rotate_surface_with_policy(store: &Store, deps: &Deps, surface_id: &SurfaceId, policy: RotationPolicy, now: Timestamp) -> Result<SurfaceRotation, CoreError>;
        pub fn replace_pending_surface(store: &Store, deps: &Deps, replace: &SurfaceId, node: &NodeName, session: &SessionId, kind: SurfaceKind, app: Option<&str>, now: Timestamp) -> Result<PendingSurface, CoreError>;
        pub fn verify_surface_token(store: &Store, token: &str, now: Timestamp) -> Result<VerifiedSurface, CoreError>;
        pub fn revoke_surface_token(store: &Store, id: &str, now: Timestamp) -> Result<(), CoreError>;
        pub fn revoke_surface(store: &Store, deps: &Deps, surface_id: &SurfaceId, now: Timestamp) -> Result<u64, CoreError>;
    }
    pub mod spool {
        pub struct QueuedAgentFrame { pub request_id: String, pub queued_at: Option<Timestamp>,
                                      pub overlap_until: Option<Timestamp>, pub frame: AgentFrame }
        pub struct AgentFrameSpool { /* home root */ }
        pub fn new(root: impl Into<PathBuf>) -> AgentFrameSpool;
        pub fn queue(&self, node: &NodeName, request_id: &str, frame: &AgentFrame) -> Result<(), CoreError>;
        pub fn queue_rotation(&self, node: &NodeName, request_id: &str, frame: &AgentFrame, queued_at: Timestamp, overlap_until: Timestamp) -> Result<(), CoreError>;
        pub fn pending(&self, node: &NodeName) -> Result<Vec<QueuedAgentFrame>, CoreError>;
        pub fn remove(&self, node: &NodeName, request_id: &str) -> Result<(), CoreError>;
        pub fn complete(&self, node: &NodeName, request_id: &str) -> Result<(), CoreError>;
        pub fn wait(&self, node: &NodeName, request_id: &str, timeout: Duration) -> Result<bool, CoreError>;
    }
    pub mod lease_control {                                         // crate::surface::lease
        pub const HEARTBEAT_INTERVAL: Duration;                     // 5 seconds
        pub const STALE_AFTER: Duration;                            // 60 seconds
        pub struct AgentLeaseGrant { pub lease_token: String, pub expires_at: Timestamp, pub tail_seq: u64 }
        pub struct LeaseConflict { pub holder: String, pub since: Timestamp, pub surface_kind: SurfaceKind }
        pub enum AgentLeaseAcquire { Granted { grant: AgentLeaseGrant, previous: Option<Lease> }, Conflict(LeaseConflict) }
        pub enum AgentLeaseHeartbeat { Accepted, Lost(String), Revoked { reason: String, holder: String } }
        pub enum AgentLeaseRelease { Released, Lost(String), Revoked { reason: String, holder: String } }
        pub fn acquire_agent_lease(store: &Store, session: &SessionId, surface: &SurfaceInfo, mode: LeaseMode, now: Timestamp) -> Result<AgentLeaseAcquire, CoreError>;
        pub fn heartbeat_agent_lease(store: &Store, session: &SessionId, surface: &SurfaceId, token: &str, now: Timestamp) -> Result<AgentLeaseHeartbeat, CoreError>;
        pub fn release_agent_lease(store: &Store, session: &SessionId, surface: &SurfaceId, token: &str) -> Result<AgentLeaseRelease, CoreError>;
        pub fn is_live(now: Timestamp, lease: &Lease) -> bool;
    }
    pub mod gateway {
        pub const MAX_FRAME_BYTES: usize;                            // 1 MiB
        pub const HANDSHAKE_TIMEOUT: Duration;                       // 10 seconds
        pub const HEARTBEAT_SECS: u64;                               // 10
        pub struct GatewayOptions { pub home_id: String, pub heartbeat_secs: u64, pub max_frame_bytes: usize, pub handshake_timeout: Duration }
        pub enum GatewayOutcome { Enrolled { node: NodeName }, Closed { node: Option<NodeName> } }
        pub fn serve(store: &Store, deps: &Deps, reader: impl BufRead + Send + 'static, writer: impl Write) -> Result<GatewayOutcome, CoreError>;
        pub fn serve_with_options(store: &Store, deps: &Deps, reader: impl BufRead + Send + 'static, writer: impl Write, options: GatewayOptions) -> Result<GatewayOutcome, CoreError>;
    }
    pub mod dispatch { /* see §9.1 */ }
}
```

Behaviour:

- A code is 52 characters (two ULIDs, 160 random bits). Only its SHA-256 hex hash is stored;
  `vowp node enroll` prints the plaintext once. Codes expire after ten minutes and are consumed
  in one immediate transaction, so a race cannot redeem one twice.
- Redemption requires the node to be `pending`, or a code created after the node's last contact
  (an explicit fresh re-enrollment). It revokes all previous credentials, binds the new ed25519
  public key, sets `status=active`, `kind=agent`, `protocol=vowp-agent/1` and `agent_version`, and
  stores the reported path map in `node_paths`.
- Authentication is ed25519 over `nonce + "\n" + home_id + "\n" + connection_id`. The nonce is a
  fresh 52-character token per connection; a signature captured on one challenge does not verify
  on the next. Public keys and signatures are lowercase hex.
- The first frame is `Enroll` or `AgentHello`. `Enroll` returns `Enrolled` and closes; the agent
  reconnects and performs `AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome`. After the
  handshake, `Heartbeat` updates `nodes.last_seen_at`, agent version, protocol and capabilities
  (capacity under `capabilities.capacity`), and `Ping` is answered with `Pong`. A revoked or unknown
  node gets `Revoked` or a typed `Error` and the connection closes. Frames are bounded at 1 MiB and
  the handshake has a 10-second deadline.
- Surface pairing starts with `create_pending_surface`: the node must exist and not be revoked,
  and the session must exist. The new surface is `pending` and its metadata records the session
  id. A surface-kind code is a 52-character token with a ten-minute TTL; only its SHA-256
  hash is stored and the plaintext is returned once.
- `issue_surface_token` runs one immediate transaction: it consumes the code, checks
  `kind=surface`, the code's surface and node, the surface row's node, an active node, the
  `pending` state and the session/app binding, inserts the token row and sets the surface
  `attached`. A refusal rolls the transaction back, so a code presented for the wrong node,
  session or app is not burned. Default scopes are `session.read:<id>`, `session.write:<id>`,
  `project.read:<name>` when the session names a project, and `fleet.read`; there is no node,
  policy, secret or tool scope in this slice.
- The token is `vowp_surf_` plus 32 `/dev/urandom` bytes in unpadded base64url (43 characters).
  The home stores only the SHA-256 hash, a 12-character prefix of the random part and the
  scopes; the plaintext is returned once and never logged. Tokens expire after 30 days.
  `verify_surface_token` compares the hash in constant time, refuses a revoked or expired token
  and a non-attached or revoked surface, records `last_used_at` and returns the surface row so
  callers can compare its node with the connection node. `revoke_surface_token` revokes one
  token row; `revoke_surface` revokes every active token and marks the surface revoked.
- After authentication, `SurfacePair { code, surface_id, session, app }` is redeemed against the
  connection node and answered with `SurfaceIssued { surface_id, token, scopes, expires_at }`.
  `Refused`, `Invalid` and `NotFound` become a non-retryable typed `Error`; when the surface row
  is already revoked the answer is `SurfaceRevoked`. Those refusals keep the authenticated
  connection open so the agent can retry with a fresh code; an internal store failure closes it.
  The success log line carries only the surface id and session id, never the code or token.
- `rotate_surface_token` runs one immediate transaction over an `attached` surface and takes
  `&Deps` so it can append the rotation ledger note. It copies the scopes of the surface's most
  recent token, mints a fresh 30-day token with the same `vowp_surf_` format and inserts only its
  hash and 12-character prefix. Every other active token is shortened to `now + grace` through a
  conditional `UPDATE` that never extends an expiry; a token that already expires sooner keeps
  its earlier time. The grace is capped at `SURFACE_TOKEN_MAX_GRACE` (24 hours); the default is
  the ten-minute `SURFACE_TOKEN_OVERLAP`. `old_expires_at` is the earliest resulting expiry, or
  the end of the grace window when the surface has no other active token. A pending, revoked or
  unknown surface is a typed refusal. The plaintext replacement token is returned once; only its
  hash and prefix are stored. One `node_ledger` `note` records the action, surface, node, prefix
  and the two expiries, never the token or a path.
- `rotate_surface_with_policy` adds the delivery half used by `surface rotate --wait/--grace`: it
  calls the rotation, queues `SurfaceIssued` through `AgentFrameSpool::queue_rotation` with
  `queued_at` and the effective `overlap_until`, and waits for the frame to be drained when a
  timeout is present. The replacement token's `expires_at` is the delivery deadline; `wait`
  stops early when that deadline has passed. An offline node is not an error: the frame stays on
  disk for the next authenticated connection.
- The gateway also drains `agent-frames/<node>/<request>.json` at the top of the same connection
  loop that drains orchestrator commands. The `AgentFrameSpool` writes one envelope
  `{"queued_at": …, "overlap_until": …, "frame": {…}}` through a 0600 temporary file and a rename
  in a 0700 directory, sends only its authenticated node's directory and removes each file after
  the write succeeds. A bare `AgentFrame` written by the earlier rotation slice still reads as an
  envelope with both times absent. A failed or absent connection leaves the frame for the next
  authenticated connection; a frame for another node is never read. `vowp surface rotate` queues
  `SurfaceIssued` under the surface id, so a second rotation before delivery replaces the older
  token rather than leaving two plaintext tokens on disk.
- `replace_pending_surface` revokes the old surface, creates the replacement `pending` surface
  and its fresh code in one transaction, and removes the old surface's queued rotation after the
  transaction commits. The old surface must belong to the same node and session and must not
  already be revoked; the old token is refused immediately. No token is issued until the agent
  redeems the new code.
- `LeaseAcquire { surface, session, surface_token, mode }` first verifies the surface token
  against `surface_tokens`, requires the surface row to be `attached`, on the connection node,
  paired for this session and scoped `session.write:<id>`. It then runs one immediate
  transaction over the session's lease row. `mode=acquire` replaces only a missing or stale row
  (heartbeat at least 60 seconds old); a live row answers `Error { kind: conflict }` with the
  holder and acquisition time, never a token. `mode=takeover` replaces a live row and the
  gateway pushes `LeaseRevoked { reason, holder }` before `LeaseGrant`.
- `LeaseGrant` carries the only plaintext lease token, the expiry (`now + 60s`) and the session's
  `tail_seq`. The home stores only the token's SHA-256 in `leases.token_hash`; the token is never
  logged, echoed in an error or written to a note.
- `LeaseHeartbeat` hashes the presented token and advances `heartbeat_at` with the home's clock.
  A missing row answers `LeaseLost`; a row now held by another surface answers `LeaseRevoked`.
  `LeaseRelease` deletes the row when the hash matches, is idempotent when the row is gone, and
  never deletes another surface's row. An old surface's heartbeat or release after a takeover is
  therefore `LeaseRevoked`, and no write may append without the current lease.
- Per-frame authentication is deliberately minimal: `LeaseAcquire` carries the scoped surface
  token, while `LeaseHeartbeat` and `LeaseRelease` carry only the lease token and are bound by
  hashing that token and checking the surface row, its paired session and the connection node.
  The agent holds the lease token; local clients never read it.
- The v0.2 orchestrator keeper, startup reuse and stop paths now hash the plaintext value in
  `lease.token` before comparing it with the stored hash, and use `Store::release_lease_hash`
  only for the owner-name fallback when the token file is already gone. This keeps the existing
  orchestrator behavior without storing or accepting the hash itself as a credential.
- The gateway does not restart the supervisor, stop a live run or drain a harness on a takeover
  or an expired lease. Surface event appends, the takeover note/switch and run fencing are later
  slices; this slice only owns the lease row and the control frames.
- The gateway keeps a process-wide `(home root, node)` connection registry. A second
  authenticated connection supersedes the first: the first is answered `Superseded` and closes,
  and the new connection resumes the still-active assignments with `Resume`. When a *current*
  connection closes without a successor, its accepted or running assignments move to
  `disconnected` for the grace period. A revoked node fences every active assignment before it
  sends `Revoked`.

### 9.1 Offload dispatcher (`agent/dispatch.rs`)

The dispatcher is the home half of §6.3. It never creates a worktree, starts a harness, pushes a
branch or opens a second run log. It reuses `session::remote::begin_turn` for the session's run
row and framing events, and `surface::lease` for the token shape and hash rule. A run lease is
separate from the chat surface lease: it is stored as a `leases` row with `surface_kind=agent`
and no surface id, and only its SHA-256 is durable.

```rust
pub mod dispatch {
    pub const RUN_LEASE_TTL: Duration;      // 600 s; no run-lease heartbeat in this slice
    pub const SPOOL_CAP_BYTES: u64;         // 512 MiB, §6.5
    pub const REJECT_RETRY: Duration;       // 60 s rejection cooldown
    pub const WAITING_PREFIX: &str;         // "waiting: offload "

    pub struct EnqueueReport { pub enqueued: Vec<AgentAssignment>, pub waiting: Vec<WaitingJob>, pub home: usize }
    pub struct WaitingJob { pub project: ProjectName, pub job: JobId, pub reason: WaitingReason,
                            pub detail: String, pub node: Option<NodeName>, pub placement: Placement }
    pub struct ClaimedAssignment { pub assignment: AgentAssignment, pub frame: AgentFrame }
    pub struct ResumeOutcome { pub resume_ok: AgentFrame, pub assignments: Vec<ClaimedAssignment>,
                               pub lost: Vec<(RunId, String)> }

    pub fn enqueue_ready_jobs(deps: &Deps, store: &Store, now: Timestamp) -> Result<EnqueueReport, CoreError>;
    pub fn claim_for_node(deps: &Deps, store: &Store, node: &NodeName, now: Timestamp)
        -> Result<Vec<ClaimedAssignment>, CoreError>;
    pub fn claim_for_node_with_slots(deps: &Deps, store: &Store, node: &NodeName,
                                     max_slots: Option<u32>, now: Timestamp)
        -> Result<Vec<ClaimedAssignment>, CoreError>;
    pub fn accept(...) -> Result<AgentAssignment, CoreError>;
    pub fn reject(...) -> Result<AgentAssignment, CoreError>;
    pub fn update_state(...) -> Result<AgentAssignment, CoreError>;
    pub fn lose(...) -> Result<AgentAssignment, CoreError>;
    pub fn sweep_expired(...) -> Result<Vec<AgentAssignment>, CoreError>;
    pub fn resume_assignment(...) -> Result<Option<ClaimedAssignment>, CoreError>;
    pub fn supersede_assignment(...) -> Result<AgentAssignment, CoreError>;
    pub fn resume_connection(...) -> Result<ResumeOutcome, CoreError>;
    pub fn cancel_job(...) -> Result<Vec<AgentFrame>, CoreError>;
    pub fn cleanup_assignment(...) -> Result<Option<AgentFrame>, CoreError>;
    pub fn disconnect_node(...) -> Result<Vec<AgentAssignment>, CoreError>;
    pub fn fence_node(...) -> Result<Vec<AgentAssignment>, CoreError>;
}
```

- **Queue.** `enqueue_ready_jobs` reads every registered project's `ready` jobs. A job whose
  effective placement is `home` is skipped. For every other job it builds the §7.2 request from
  the selected profile executor, the harness `env_pass` names (as environment *and* required
  secret names) and the node cards, then runs the pure `offload::evaluate`. The home facts are
  rebuilt for every job, so the fleet `max_remote_runs` check sees each assignment inserted
  earlier in the same pass. A waiting decision writes
  `jobs.note = "waiting: offload <reason>: <detail>"` and leaves the job `ready`; it never
  fails the job. The note is the read model consumed by `vowp fleet status`/`queue`. A job with
  only a home target is left for the L2 loop. A job with a live assignment is skipped, so a
  heartbeat cannot create a second row for the same job, run or epoch. A fresh rejection has a
  60-second cooldown. The payload has a fresh run id, the job session, the executor, purpose,
  path-free prompt, `PathRef::Worktree`, the project HEAD as base commit, the remote name, the
  project remote URL with userinfo stripped, environment and secret **names**, epoch, optional
  deadline and the spool cap.
- **Claim and send.** `claim_for_node_with_slots` computes free slots as the node's
  `max_slots` (or the stricter `[offload.nodes.<name>].max_workers`) minus assignments in
  `sent`/`accepted`/`running`/`disconnected`, then takes the smaller of that and the fleet
  `offload.max_remote_runs` minus the same active states across every node.
  `Store::claim_assignments` selects queued rows and moves them to `sent` in one
  `BEGIN IMMEDIATE` transaction, so a row cannot be claimed twice. The dispatcher then mints a
  run token, writes only its hash through `Store::take_lease`, and returns `AssignRun`. If the
  lease cannot be issued or the frame cannot be written, the row goes back to `queued`; it is
  never dropped. At most the free slots and the remaining fleet cap are sent per heartbeat.
- **Accept / reject / state.** `RunAccepted` moves the row to `accepted` and the job to
  `running`. `RunRejected` moves it to `rejected`, finishes the open run with `outcome=error`,
  releases the lease and puts the job back to `ready` with the refusal reason and detail. A
  `RunState` report moves `accepted`/`disconnected` to `running` or `running` to `disconnected`;
  `finishing` keeps the row `running` with a redacted detail (spool bytes, optional short head
  commit). No detail contains a token, secret value or home absolute path.
- **Resume and epochs.** A duplicate `AssignRun` for the same `(run, epoch)` calls
  `resume_assignment`: the payload and epoch are unchanged, a fresh token and expiry are minted,
  and no second row is created. A higher epoch is inserted by `supersede_assignment`, which copies
  the newest payload with the new epoch and marks older active rows `lost` while keeping the run
  open. `Resume` returns `ResumeOk` with the session tail for each run and surface, then re-sends
  `AssignRun` for accepted runs. A surface is accepted when its row is attached on the connection
  node; its tail is the session stored in its metadata.
- **Fence, cancel and cleanup.** An expired lease (`expires_at <= now`), a `RunLost` decision or a
  revoked node fences the assignment to `lost`, closes the run with `outcome=error`, releases the
  lease and returns the job to `ready`. `sweep_expired` runs on heartbeats and before a resume.
  `cancel_job` moves each active row to `cancelled`, closes the run, releases the lease and returns
  `CancelRun`. `cleanup_assignment` returns `Cleanup` for a terminal row with a
  `PathRef::Worktree`; the agent removes the worktree later.
- **Secrets and leases.** The durable payload carries `env_names` and `secret_names` only. The
  plaintext run token exists only in the `AssignRun` frame; `leases.token_hash` is its SHA-256.
  Agent-supplied detail is bounded and has the home root replaced before it is stored.

Tests (`tests/offload_dispatch.rs`, temp home, fake git/clock, enrolled node row, no network,
tmux or harness): `dispatcher_places_an_any_job_on_an_eligible_node`,
`dispatcher_waits_when_a_pinned_node_is_offline`,
`offload_refuses_a_home_only_project_and_a_denied_kind`,
`offload_refuses_a_job_that_needs_a_home_only_secret`, `offload_requires_a_valid_path_map`,
`assignment_payload_is_durable_and_has_no_lease_token`,
`duplicate_assign_run_for_the_same_epoch_resumes_and_a_higher_epoch_supersedes`,
`run_rejected_keeps_the_job_ready_with_a_reason`, `resume_returns_the_run_and_surface_tails`,
`cancelled_job_sends_cancel_run_and_fences_the_assignment`,
`run_lost_after_an_expired_lease_marks_the_assignment_lost`,
`claim_is_bounded_by_the_fleet_remote_run_cap` and
`enqueue_counts_each_insert_against_the_fleet_cap`. The gateway adds
`heartbeat_dispatches_at_most_the_node_slots` and `agent_second_connection_supersedes_the_first`.
The store tests add `claim_assignments_is_atomic_and_returns_the_updated_rows`,
`advance_assignment_rejects_an_invalid_transition_but_is_idempotent` and
`assignments_for_job_filters_on_the_durable_payload`.

Tests (`tests/agent.rs`, in-memory duplex, temp home, no real keys): `gateway_enrolls_a_node_with_a_valid_code`,
`gateway_refuses_an_expired_or_used_code`, `gateway_auth_rejects_a_bad_signature_and_a_replayed_nonce`,
`gateway_heartbeat_updates_presence_and_capacity`, `gateway_revoke_marks_and_closes`,
`gateway_unknown_or_revoked_node_is_refused`, `gateway_surface_pair_issues_and_persists_hash`,
`gateway_surface_pair_refuses_a_bad_or_replayed_code`, `gateway_surface_pair_binds_the_connection_node`,
`gateway_surface_pair_refuses_a_revoked_surface`, `surface_token_never_appears_in_logs`,
`surface_rotate_sends_surface_issued_on_the_live_connection`,
`lease_acquire_grants_and_heartbeats`, `lease_conflict_without_takeover`,
`lease_takeover_pushes_lease_revoked`, `lease_release_removes_the_row`,
`surface_token_must_match_the_paired_surface`, `lease_token_is_stored_only_as_a_hash` and
`lease_stale_after_sixty_seconds`. The service tests in `src/agent/pair.rs` are
`surface_pair_issues_a_scoped_token_and_stores_only_its_hash`,
`surface_pair_refuses_expired_or_used_code`, `surface_pair_binds_node_and_session`,
`surface_token_verify_rejects_revoked_and_expired`,
`surface_token_rotation_keeps_the_old_token_for_the_overlap`,
`surface_rotate_refuses_pending_and_revoked` and
`surface_rotation_never_extends_a_shorter_expiry`.

### 9.2 Result return (`agent/result.rs`)

The node pushes `job/<id>` with its own git credentials; the home never receives or stores an
agent credential. The terminal summary path (`session::remote::IngestSession::finish_summary`)
calls the result module before it closes the run row, so a rejected result is written as a
failed run rather than a success followed by a correction.

```rust
pub mod result {
    pub const RESULT_KEY_PREFIX: &str;      // "agent-result:"

    pub struct AgentRunResult { pub transport: String, pub base_commit: String,
                                pub head_commit: Option<String>, pub spool_bytes: u64,
                                pub bundle_bytes: Option<u64>, pub bundle_sha256: Option<String>,
                                pub status: String, pub detail: String }
    pub struct Settled { pub outcome: vowp_protocol::RunOutcome, pub detail: String }

    pub fn result_key(run: &RunId) -> String;
    pub fn load(store: &Store, run: &RunId) -> Result<Option<AgentRunResult>, CoreError>;
    pub fn settle_terminal(deps: &Deps, store: &Store, run: &Run,
                           reported_head: Option<&str>, outcome: RunOutcome, detail: String)
        -> Result<Settled, CoreError>;
}
```

- **Fetch.** A `RunState` frame records the reported head and spool size durably in the store's
  existing `kv` table (`agent-result:<run>`) and sets `jobs.state=running`. The gateway also
  keeps the head in the live ingest stream. On `TurnEnd` the home reads the head from either
  source, then `git fetch <project remote>` into the home checkout. It verifies the remote name
  and both the fetch and push URLs against `projects.remote_url` with userinfo stripped; a
  fork, a different remote name or a differing push URL is rejected. The agent never supplies
  the URL or a credential.
- **Verify.** The fetched `refs/remotes/<remote>/job/<id>` head must equal the reported head,
  the branch must be the job branch, the assigned base commit must exist on the home, and the
  base must be an ancestor of the fetched head (`commits_between(head, base)` is empty). A
  mismatch fails the run and keeps the fetched ref; a remote-tracking ref is left in place even
  when the staging worktree was never created.
- **Stage.** The home points a local `job/<id>` branch at the fetched head and adds a worktree
  under `<node>/agent-results/<run>`, never in the main checkout. `git fetch`, the local branch
  update and `worktree add` leave the main checkout's working tree and HEAD untouched. The
  branch update and `worktree add` run only after verification passes, so a run rejected during
  fetch leaves no new ref and a run rejected during verification keeps the fetched
  remote-tracking ref and no local branch or worktree. A gate violation removes the staging
  worktree but keeps the local `job/<id>` branch for inspection, so a retry can move that
  branch to the next pushed head; on success the worktree becomes the job worktree.
  For the two result gates the module pins the gate context's `project.default_branch` to the
  assigned base commit, so the gate helpers' three-dot diff is exactly `base..head` even if the
  home's `main` moved while the run was on the node.
- **Gate.** The normal `GateRunner` evaluates `Gate::OwnsRespected(true)` and
  `Gate::SecretScan(true)` in the staging worktree with `run_id` set to the offloaded run. Each
  result is persisted (`gate_results`) and written to the ledger. Any violation fails the run
  with `result_gate_failed`, leaves the job `ready`, and never enqueues a merge. Gate evidence
  contains repo-relative paths and rule names only; matched secret text is never stored.
- **Hand-off.** On success the run detail ends with
  `result: transport=<push|bundle> base=<sha> head=<sha>`; the same facts live in the
  `agent-result` record because the run row has no dedicated transport/base/head columns yet. A
  bundle result also records `bundle_bytes` and `bundle_sha256`. The job gets the
  staging branch and worktree, moves `running -> gating` through the normal `ReportReady` state
  event, and the existing supervisor gate runner and merge queue continue from there. Result
  return never merges, tags, deploys, pushes the default branch or rewrites the merge queue.
- **Failure.** A failed push (the node reports `git_push_failed`), a missing head, a remote or
  head mismatch, a non-ancestor head or a gate violation settles the run as `error`, writes a
  redacted path-free detail, puts a still-running job back to `ready` (attempts and continues
  unchanged, so its retry policy applies) and clears the dispatcher's `waiting: offload ...`
  note. A node stderr is replaced by the stable `git_push_failed` sentence, so a credential URL
  or absolute local path cannot reach the run row, ledger or log.
- **Terminal cleanup.** Every terminal summary of an offloaded run releases the run lease,
  moves the assignment to `finished` (or leaves an already `lost`/`cancelled` row alone),
  appends one `note` ledger row with the transport facts, and lets the normal ingest path write
  the cost/capacity rows with the settled outcome. A summary for a run without a live
  assignment (a local chat turn) is untouched.

**Bundle return.** A bundle assignment is handled by the same terminal path with
`assignment.result_transport = "bundle"`. The gateway's `BundleStart`/`BundleChunk`/`BundleEnd`
arms write one staging file below `<home>/tmp/agent-bundles/<run>/`, 0600 in a 0700 directory,
while hashing and enforcing the 256 MiB cap and the five-minute deadline. A resume start with the
same `(run_id, epoch, sha256, bytes)` returns `BundleAccept { offset }`; a gap, overlap, oversized
chunk or digest mismatch aborts the transfer. `bundle_failed` covers the transfer and file
failures; `bundle_rejected` covers well-formed bytes that are not the assigned result.

After `BundleEnd` the home:

1. removes the file and fails `bundle_failed` on a size or sha256 mismatch;
2. runs `git bundle verify` in the project repository and requires `git bundle list-heads` to name
   exactly `refs/heads/job/<id>` at the reported head (`bundle_rejected` otherwise);
3. fetches only that ref into a private staging bare repository that already has the assigned
   base, checks the fetched head equals the reported head and descends from the base, and only
   then imports the verified objects into the project repository as `refs/vowp/jobs/<id>`
   (`bundle_rejected` on any mismatch);
4. points `job/<id>` at the verified head, creates the staging worktree and runs the same
   `owns_respected` and `secret_scan` gates as the push path.

The result record carries `transport = "bundle"`, `base_commit`, `head_commit`, `bundle_bytes`
and `bundle_sha256`; it is path-free and contains no token or secret. A failure in steps 1–3
leaves no project ref and removes the staging file after settle. A gate failure after step 4
leaves both the inspection `refs/vowp/jobs/<id>` and the local `job/<id>` branch in place: the
branch stays for inspection so a retry can move it to the next bundled head, only the staging
worktree is removed, and nothing merges. A summary that arrives before a complete bundle is a
`bundle_failed` run, not a success. The run stays `finishing` from
`BundleEnd` until the bundle result is settled.

Tests added by the bundle slice in `tests/offload_result.rs`:
`offloaded_run_returns_a_bundle_without_push_access`,
`bundle_result_is_verified_by_size_sha256_and_bundle_verify`,
`bundle_result_rejects_a_foreign_ref_or_head`,
`bundle_result_rejects_a_base_that_is_not_a_prerequisite`,
`bundle_result_resumes_after_a_disconnect`,
`bundle_result_over_the_cap_removes_the_staging_file` and
`bundle_result_manifest_and_run_row_carry_no_absolute_path_or_secret`.

The push path keeps its own tests (`tests/offload_result.rs`, temporary bare remote, project
clone, agent clone and fake summary through the real ingest state machine, no network or
harness): `offloaded_run_pushes_the_job_branch_and_the_home_verifies_the_head`,
`offload_fails_cleanly_when_the_push_fails`,
`offload_fails_when_no_pushed_head_is_reported`,
`home_rejects_a_reported_head_that_is_not_the_fetched_head`,
`home_rejects_a_head_outside_the_base_history`,
`home_runs_owns_and_secret_scan_on_the_fetched_diff`,
`home_does_not_merge_a_head_outside_the_job_owns`,
`result_return_leaves_the_main_checkout_clean`,
`terminal_result_releases_the_lease_and_clears_the_waiting_note` and
`a_failed_push_keeps_the_job_retryable`. The gateway adds
`gateway_returns_an_offloaded_result_to_the_gating_path`, which opens the offload ingest stream
from the agent-kind run lease, pushes a temporary branch and observes the job reach `gating`
through the same gateway process.

### 9.3 Offload end to end (`tests/offload_e2e.rs`)

`offload_e2e.rs` is the §14.3 `fake_gateway_full_turn_and_job_end_to_end` exit test. It uses a
temporary bare origin, a real home clone, a real node clone, an in-memory gateway transport and a
fake adapter script. The fake node is a small protocol client, not a second home: it resolves
`PathRef::Worktree`, creates a real git worktree at the assigned base commit, runs the adapter's
recorded JSONL into ordered events, commits, and pushes only `job/<id>`. The home half is the real
`gateway::serve_with_options` plus `dispatch` and `result`.

The named test asserts the whole contract: the `AssignRun` payload carries the worktree `PathRef`,
base commit, prompt, env/secret names, remote and epoch; the run-lease token appears only in the
frame and only its hash is stored; the events append in order with gapless `seq`; the home fetches
`job/<id>`, verifies the reported head against the fetched head and the assigned base, runs
`owns_respected` and `secret_scan`, records the result, and moves the job to `gating`. The origin's
default branch and the main checkout do not move, and no merge ledger row is written.

The rest of the file pins the refusals and recovery edges. A home-only project and a denied kind
stay `ready` with their documented waiting reason and receive no `AssignRun`; a pinned offline node
waits with `node_offline` and never falls back to the home; a mid-run disconnect resumes with
`ResumeOk`, replays the spool with duplicate acknowledgements, leaves a gapless log with one row per
event id, and finishes the job; a failed `job/<id>` push settles as `git_push_failed` with the job
and node worktree kept for a retry; and a head outside the job's `owns` fails the authoritative gate
without a merge. The reconnect test also covers the dispatcher rule that a `RunAccepted` for a
`disconnected` assignment returns it to `running` while a fenced or finished assignment is refused;
the real agent sends that frame after `Resume`.

The fake node still stands in for `vowp-agent`'s `WorktreeManager` and `CoreRunHandler` (their
own tests cover creation, reuse, push and bundle chunking), the gateway is the in-process stdio
server rather than the SSH forced command, and the suite stops at `gating` rather than running
the merge queue. The WSS listener, the bundle result and the launchd/systemd-user service have
their own named tests in `vowp-agent` and `vowp-core`; the owner live Mac smoke test that exercises
them together is a phase-exit activity, not part of this suite.

### 9.4 Orchestrators on a local node (`surface/orchestrator.rs`)

The v0.2 manager still owns the home-hosted orchestrator: a project-local directory, an app
config, one surface lease, an optional host and the lease keeper. The v0.4 slice adds the node
record and the home state machine; the agent-side materialization and app start are the next
slice (3g).

```rust
pub trait OrchestratorTransport: Send + Sync {
    fn send(&self, node: &NodeName, command: &OrchestratorCommand)
        -> Result<Option<OrchestratorResult>, CoreError>;
}
pub struct CommandSpool { /* root, timeout */ }
pub const COMMAND_RESULT_TIMEOUT: Duration;                        // 5 seconds

pub fn create(&self, req: &NewOrchestrator) -> Result<OrchestratorInfo, CoreError>;               // home
pub fn create_on_node(&self, req: &NewOrchestrator, node: Option<NodeName>)
    -> Result<OrchestratorInfo, CoreError>;
pub fn resume(&self, name: &str) -> Result<OrchestratorInfo, CoreError>;
pub fn stop(&self, name: &str) -> Result<(), CoreError>;
pub fn resume_all(&self) -> Result<Vec<OrchestratorInfo>, CoreError>;
pub fn mark_node_disconnected(&self, node: &NodeName) -> Result<usize, CoreError>;
pub fn node_reconnected(&self, node: &NodeName) -> Result<Vec<OrchestratorInfo>, CoreError>;
pub fn apply_result(&self, node: &NodeName, result: &OrchestratorResult)
    -> Result<Option<OrchestratorInfo>, CoreError>;
pub fn redact_text(text: &str) -> String;
```

The persisted `orchestrator.json` now carries `node: Option<NodeName>`,
`last_request_id: Option<String>`, a typed `state` and an optional redacted `detail`. The card
exposes `node` and `state`; a node orchestrator card never contains a path, port, token, pair
link or secret. The session's `cwd` for a node orchestrator is `"."` (project-relative), not a
home path.

- `create_on_node` validates the node before creating a directory or session: an unknown node
  is `NotFound`, a pending or revoked node is `Refused`. An active node that is offline gets the
  home record in `node_offline`; it is never started on the home and receives no command. An
  online node gets `starting`, a `Start` command and the correlated result (`running` or
  `failed`).
- `resume` on a remote-node orchestrator requires an active, online node. Otherwise it writes
  `node_offline` and returns a `Refused` error whose message starts with `node_offline`; the
  CLI maps that to exit 7. It never starts a home host and never starts a keeper.
- `resume_all` never starts a node orchestrator on the home. At startup, an offline node marks
  a live record `degraded` and leaves it for `node_reconnected`; a `stopped` or `failed` record
  stays as it is. `node_reconnected` sends `Resume` for a degraded record and `Start` for a
  `node_offline` record, then applies the result.
- `mark_node_disconnected` is the gateway hook for a closed authenticated connection: it marks
  only `starting`, `running` and `degraded` records `degraded`. `apply_result` is the hook for a
  late `OrchestratorResult`: it matches the node and `last_request_id` and applies the typed
  state. `gateway.rs` calls both: a current connection closing calls `mark_node_disconnected`,
  each heartbeat on an online node calls `node_reconnected`, and an
  `AgentFrame::OrchestratorResult` calls `apply_result` before publishing the result.
- `stop` on a node orchestrator sends `Stop` (the agent maps it to `SurfaceDetach`), releases
  any home lease, writes `stopped` and stops no local host and no agent process.
- `CommandSpool` is the default transport. It writes the frame under
  `agent-commands/<node>/<request_id>.json` and waits up to `COMMAND_RESULT_TIMEOUT` for
  `agent-results/<node>/<request_id>.json`; a result that arrives later is applied by
  `apply_result`. A timeout leaves the command durably queued and records `starting`. A live
  gateway connection drains the outbox at the top of every frame loop, sends
  `AgentFrame::OrchestratorCommand` on that authenticated connection, and calls
  `CommandSpool::complete` with the correlated result; a command stays on disk until its result
  is published, so a reconnect re-sends it. Hermetic tests can inject a fake transport instead.
- `redact_text` replaces absolute paths with `<path>` and obvious `key=value` secrets with
  `<redacted>`, and caps the text at 512 characters. Every result detail and error message
  passes through it before `orchestrator.json` or a log sees it.

Named tests (in `crates/vowp-cli/tests/orchestrator.rs`): `orchestrator_new_with_a_node_creates_a_card_and_sends_start`,
`orchestrator_new_refuses_an_unknown_or_revoked_node`,
`orchestrator_resume_on_an_offline_node_reports_node_offline`,
`orchestrator_stop_sends_detach_and_releases_the_lease`,
`orchestrator_disconnect_marks_it_degraded_and_reconnect_resumes`,
`orchestrator_resume_all_marks_an_offline_node_degraded`,
`orchestrator_apply_result_correlates_the_late_node_result` and
`orchestrator_card_contains_no_path_or_secret`. All use a temporary home, a fake app host or
a fake command transport, and no real tmux, app, network or home directory.

## 10. Capacity history (v0.4 Phase 4)

Spec: `docs/design/v0.4/hardening.md` §7. `capacity/history.rs` is the write and read half of the
bounded metrics history. It never samples the host itself and never invents a point.

```rust
pub const ROLLUP_BUCKET_SECS: u64;       // 300
pub const RAW_RETENTION_SECS: u64;       // 48 * 60 * 60
pub const ROLLUP_RETENTION_SECS: u64;    // 30 * 24 * 60 * 60

pub enum HistoryBucket { FiveMinutes, OneHour, OneDay }
impl HistoryBucket {
    pub fn secs(self) -> u64;            // 300, 3600, 86400
    pub fn label(self) -> &'static str;  // "5m", "1h", "1d"
    pub fn parse(text: &str) -> Option<HistoryBucket>;
}

pub fn record_rollup(store: &Store, now: Timestamp) -> Result<bool, CoreError>;
pub fn read(store: &Store, now: Timestamp, since: Option<Timestamp>, bucket: HistoryBucket)
    -> Result<CapacityHistory, CoreError>;
```

- The machine tick calls `record_rollup` after the host sample. It aggregates the **previous
  completed** 5-minute bucket from `host_samples` and writes one `capacity_rollups` row with
  `INSERT … ON CONFLICT(bucket, bucket_secs) DO UPDATE`, so a repeated tick, a restarted
  supervisor or a deferred migration cannot duplicate or lose a bucket. A bucket with no raw
  samples is skipped, not written as zero.
- `source` is `proc` when every raw sample in the bucket is `proc`, `fallback` when every sample
  is `fallback`, and `mixed` otherwise. The raw rows keep their own source.
- Raw `host_samples` are pruned after 48 hours and rollups after 30 days; the bucket containing
  `now` is never deleted. A gap is a gap, not a zero. The row budget is about 34,560 raw rows and
  8,640 rollups (roughly 7 MiB with the indexes).
- `read` clamps `since` to the retained 30-day window, reads only the stored 5-minute rows and
  aggregates `1h` and `1d` at read time, weighting means by `samples` and taking the min/max
  extrema of the constituent points. It returns `CapacityHistory` with `generated_at`, `since`,
  `until`, `bucket_secs`, `retention_secs` and ascending `points`; an empty range is an empty
  `points` array, not a fabricated row.
- The read path is read-only: `vowp capacity history` opens the home with
  `Store::open_read_only`, writes no target, advice, ledger or rollup row and is safe while the
  supervisor runs. Cost remains the existing ledger; the command's footer points at
  `vowp cost --by day` instead of duplicating it.

Named tests: `capacity_history_buckets_raw_samples_into_five_minute_points`,
`capacity_history_is_idempotent_across_a_repeated_tick`,
`capacity_history_keeps_thirty_days_and_prunes_the_rest`,
`capacity_history_clamps_since_to_the_retained_window`,
`capacity_history_read_time_buckets_weight_by_samples` and
`capacity_history_is_read_only_and_writes_no_ledger_row` in
`crates/vowp-core/tests/capacity_measure.rs`; `capacity_history_aggregates_read_time_hour_buckets`
and `capacity_history_is_read_only` in `crates/vowp-cli/tests/capacity.rs`;
`capacity_history_json_matches_the_exported_schema` in
`crates/vowp-protocol/tests/schema.rs`; the schema migration test is
`capacity_rollups_migrates_v6_to_v7_with_a_backup` in `crates/vowp-store/tests/migrate.rs`.

## 11. `offload` — pure offload eligibility (v0.4 Phase 2)

Spec: `docs/design/v0.4/local-orchestrator.md` §6.6, §6.7 and §7.1–§7.5. This module decides only:
it opens no store, starts no process, reads no clock and touches no filesystem. The home passes
the effective policy and one fact set per candidate; the dispatcher slice will act on the answer,
but the answer itself is already complete and testable.

```rust
pub mod offload {
    pub mod policy {
        pub struct HomeFacts { pub brake_active: bool, pub active_remote_runs: u32 }
        pub struct NodeCapabilities { pub harnesses: Vec<String>, pub secrets: Vec<SecretName>, pub projects: Vec<ProjectName> }
        pub struct PathFacts { pub map_present: bool, pub local_root_present: bool, pub worktree_root_present: bool, pub writable: bool }
        pub struct LimitFact { pub provider: String, pub kind: LimitKind, pub detail: String }
        pub enum ThermalState { Nominal, Fair, Serious, Critical }
        pub struct NodePower { pub on_battery: bool, pub thermal_state: ThermalState }
        pub struct NodeFacts {
            pub name: NodeName, pub status: NodeStatus, pub presence: NodePresence,
            pub labels: Vec<String>, pub capabilities: NodeCapabilities,
            pub capacity: Option<AgentCapacity>, pub power: Option<NodePower>,
            pub limits: Vec<LimitFact>, pub path: Option<PathFacts>,
            pub origin_matches: Option<bool>, pub base_fetchable: Option<bool>,
            pub clean_base: Option<bool>, pub owns_conflict: Option<bool>,
        }
        pub struct JobRequirements { pub labels: Vec<String>, pub harnesses: Vec<String>, pub secrets: Vec<SecretName>, pub provider: Option<String> }
        pub struct JobRequest { pub kind: JobKind, pub placement: Option<Placement>, pub requires: JobRequirements }
        pub enum Target { Home, Node(NodeName) }
        pub enum Decision {
            Eligible { targets: Vec<Target> },
            Waiting { reason: WaitingReason, detail: String, node: Option<NodeName> },
        }
        pub fn evaluate(fleet: &OffloadConfig, project: &ProjectName, project_policy: &OffloadProjectConfig,
                        job: &JobRequest, home: &HomeFacts, nodes: &[NodeFacts]) -> Decision;
    }
}
```

The effective placement is the per-job `job_placement` value, else the project default, else
`offload.default`. `evaluate` then applies §7.2 order: (1) enabled, active, not revoked, online;
(2) placement match; (3) project opted in and not `home_only`; (4) kind allow/deny; (5) required
labels and harness capabilities; (6) no home-only secret and every required secret declared by
the node; (7) path map present, roots present and writable, disk above `min_disk_bytes`; (8) free
node slot, fleet `max_remote_runs` and battery/thermal (`allow_battery`); (9) no blocking provider
limit for the job's executor provider; (10) no `owns` conflict; (11) origin match, base
fetchability and `require_clean_base`.

`home` is a target for `home`, `any` and the explicit `node:<name>+home` fallback. `any` returns
the home plus every eligible node, so a transient node fact is not a waiting reason there. The
job-level `project_home_only`, `kind_not_allowed` and `home_only_secret` reasons are never
overridden by a per-job placement: they remove every node target, while `any` still keeps the
home, so they become a waiting reason only when the home is unavailable too. A node at its
`max_workers` or the fleet at `max_remote_runs` reports `node_busy`, never `no_eligible_node`.
Every optional `NodeFacts` field is a refusal when `None`; the evaluator never assumes a missing
fact is good. `Decision::Waiting` leaves the job `ready`; no variant is an error.

Tests (`tests/offload.rs`, values only, no store, tmux or network):
`placement_home_node_and_any_choose_the_documented_targets`,
`home_only_project_and_denied_kind_are_refused`,
`home_only_secret_is_refused_and_a_node_secret_that_is_declared_is_allowed`,
`missing_label_capability_or_harness_reports_the_documented_reason`,
`offline_revoked_busy_battery_disk_low_and_limit_report_the_documented_reason`,
`node_at_its_worker_cap_reports_node_busy`,
`no_eligible_node_is_a_waiting_reason_not_a_failure` and
`missing_origin_or_base_facts_are_refusals`.

## 12. Out of scope (v0.2)
HTTP+SSE daemon, ACP, cross-node live sync, IANA timezone schedules, notification hooks (ntfy), automatic price-table download.
