//! Lifecycle tests: classification, dry run, guarded apply, compaction, samples, worktrees,
//! the nightly schedule and the 50 MB / 14-day run-output rules.
//!
//! Every test uses a temporary `VOWP_HOME`, a temporary project checkout and its own git
//! repository. No test reads the real home, starts tmux, touches the network or runs a harness.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use proptest::prelude::*;
use tempfile::TempDir;
use vowp_core::lifecycle::{self, GcOptions, RUN_OUTPUT_CAP_BYTES, class_name, compact_session, gc, tick};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    Actor, CAPSULE_VERSION, Capsule, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, CatalogClass,
    CatalogEntry, CatalogFilter, Cost, EventId, EventIndexRow, EventType, Executor, ExecutorRef, FakeClock,
    HarnessKind, HostSample, Job, JobId, JobKind, JobState, MemoryLog, NewJob, NodeName, Project, ProjectName, Role,
    Run, RunId, RunPurpose, RunRole, SampleSource, Session, SessionId, SessionRole, SessionState, Timestamp,
    VowpConfig,
};
use vowp_store::session_files::list_capsule_files;
use vowp_store::session_log::SessionLog;
use vowp_store::{JobFilter, NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

/// The fixed instant every test starts at.
fn ts_now() -> Timestamp {
    Timestamp::parse("2026-11-10T12:00:00Z").expect("timestamp")
}

fn days(days: u64) -> Duration {
    Duration::from_secs(days * 24 * 60 * 60)
}

fn hours(hours: u64) -> Duration {
    Duration::from_secs(hours * 60 * 60)
}

/// One temporary home, project and dependency set.
struct TestNode {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    repo: PathBuf,
    worktree_root: PathBuf,
    deps: Deps,
    clock: FakeClock,
}

impl TestNode {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temp dir");
        let project = ProjectName::parse("test").expect("project");
        let repo = tmp.path().join("project");
        let worktree_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&repo).expect("project dir");
        std::fs::create_dir_all(&worktree_root).expect("worktree root");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(repo.display().to_string());
        config.project.worktree_root = Some(worktree_root.display().to_string());

        let paths = NodePaths::new(tmp.path().join("node"));
        paths.ensure().expect("node paths");
        let clock = FakeClock::at(ts_now());
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config.clone()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };

        let store = Store::open(&paths)
            .expect("store opens")
            .with_clock(Arc::new(clock.clone()));
        let now = ts_now();
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: repo.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        Self {
            _tmp: tmp,
            paths,
            project,
            repo,
            worktree_root,
            deps,
            clock,
        }
    }

    fn store(&self) -> Store {
        Store::open_with_project(&self.paths, &self.project)
            .expect("store")
            .with_clock(Arc::new(self.clock.clone()))
    }

    fn session(&self, state: SessionState, role: SessionRole, updated_at: Timestamp) -> SessionId {
        let id = SessionId::new();
        std::fs::create_dir_all(self.paths.blobs_dir(&id)).expect("blobs dir");
        std::fs::create_dir_all(self.paths.capsules_dir(&id)).expect("capsules dir");
        let log = self.paths.log_path(&id);
        std::fs::write(&log, b"{\"seq\":1}\n").expect("log");
        let session = Session {
            id: id.clone(),
            project: Some(self.project.clone()),
            job_id: None,
            role: Some(role),
            title: format!("session {id}"),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: self.repo.display().to_string(),
            state,
            executor: Some(Executor::parse("opencode:test/model").expect("executor")),
            last_seq: 1,
            last_capsule_id: None,
            log_bytes: 11,
            created_at: updated_at,
            updated_at,
        };
        self.store().upsert_session(&session).expect("session row");
        id
    }

    fn add_event(&self, session: &SessionId, seq: u64, at: Timestamp, event_type: EventType) {
        let row = EventIndexRow {
            session_id: session.clone(),
            seq,
            id: EventId::new(),
            ts: at,
            event_type,
            actor: Actor::System,
            harness: HarnessKind::Vowp,
            model: None,
            byte_offset: seq.saturating_sub(1) * 16,
            byte_len: 16,
            summary: String::new(),
        };
        self.store().index_event(&row).expect("index event");
    }

    fn capsule(&self, session: &SessionId, trigger: CapsuleTrigger) -> PathBuf {
        let capsule = Capsule {
            version: CAPSULE_VERSION,
            capsule_id: CapsuleId::new(),
            session_id: session.clone(),
            created_at: ts_now(),
            at_seq: 1,
            trigger,
            from: ExecutorRef {
                executor: Executor::parse("opencode:test/model").expect("executor"),
                node: Some(NodeName::parse("local").expect("node")),
            },
            intent: "keep the history".to_owned(),
            constraints: Vec::new(),
            decisions: Vec::new(),
            state: CapsuleState::default(),
            files: Vec::new(),
            gotchas: Vec::new(),
            open_questions: Vec::new(),
            telemetry: CapsuleTelemetry::default(),
        };
        let path = vowp_store::session_files::write_capsule_file(&self.paths, &capsule).expect("capsule file");
        self.store()
            .insert_capsule(&capsule, &path.display().to_string())
            .expect("capsule row");
        path
    }

    fn blob(&self, session: &SessionId, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.paths.blobs_dir(session).join(name);
        std::fs::write(&path, bytes).expect("blob");
        path
    }

    fn write_run(&self, name: &str, mtime: Timestamp, bytes: &[u8]) -> PathBuf {
        let path = self.paths.runs_dir().join(name);
        std::fs::write(&path, bytes).expect("run output");
        set_mtime(&path, mtime);
        path
    }

    fn write_tmp(&self, name: &str, mtime: Timestamp) -> PathBuf {
        let path = self.paths.tmp_dir().join(name);
        std::fs::write(&path, b"temporary").expect("tmp file");
        set_mtime(&path, mtime);
        path
    }

    fn sample(&self, at: Timestamp, interval_secs: u64) {
        let sample = HostSample {
            at,
            interval_secs,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            mem_available_bytes: 8 * 1024 * 1024 * 1024,
            swap_used_bytes: 0,
            load1: 0.1,
            load5: 0.1,
            load15: 0.1,
            cpu_count: 4,
            cpu_quota_cores: None,
            disk_free_home_bytes: 100 * 1024 * 1024 * 1024,
            disk_free_worktrees_bytes: None,
            pressure_cpu_avg10: None,
            pressure_mem_avg10: None,
            pressure_io_avg10: None,
            source: SampleSource::Fallback,
        };
        self.store().lifecycle_insert_host_sample(&sample).expect("sample row");
    }

    fn job(&self, id: &str, state: JobState, state_at: Timestamp, worktree: Option<&Path>) -> Job {
        let store = self.store();
        let new = NewJob {
            id: JobId::parse(id).expect("job id"),
            title: format!("job {id}"),
            kind: JobKind::Build,
            role: Role::Worker,
            profile: Some("worker".to_owned()),
            needs: Vec::new(),
            owns: vec!["src/**".to_owned()],
            reads: Vec::new(),
            gates: None,
            priority: None,
            loop_: None,
            brief_text: None,
        };
        let job = store.insert_job(&self.project, &new, "test").expect("job row");
        if let Some(path) = worktree {
            store
                .set_job_workspace(&job.id, Some(&format!("job/{id}")), Some(&path.display().to_string()))
                .expect("job workspace");
        }
        self.clock.set(state_at);
        store
            .set_job_state(
                &job.id,
                state,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("job state");
        self.clock.set(ts_now());
        store.require_job(&job.id).expect("job")
    }

    fn init_git(&self) {
        git(&self.repo, &["init"]);
        git(&self.repo, &["config", "user.email", "test@example.com"]);
        git(&self.repo, &["config", "user.name", "test"]);
        git(&self.repo, &["checkout", "-b", "main"]);
        std::fs::write(self.repo.join("README.md"), b"main\n").expect("readme");
        git(&self.repo, &["add", "-A"]);
        git(
            &self.repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
        );
    }

    fn add_worktree(&self, id: &str) -> PathBuf {
        let path = self.worktree_root.join(id);
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("job/{id}"),
                &path.display().to_string(),
                "main",
            ],
        );
        path
    }

    fn catalog_file(&self, path: &Path, class: CatalogClass, expires_at: Option<Timestamp>) -> CatalogEntry {
        let now = ts_now();
        let entry = CatalogEntry {
            id: ulid::Ulid::generate().to_string(),
            path: path.display().to_string(),
            class,
            project: Some(self.project.clone()),
            environment: "none".to_owned(),
            job_id: None,
            session_id: None,
            bytes: std::fs::metadata(path).ok().map(|meta| meta.len()),
            expires_at,
            note: "test row".to_owned(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        self.store().upsert_catalog(&entry).expect("catalog row");
        entry
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn set_mtime(path: &Path, at: Timestamp) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    let time = if at.unix_millis() >= 0 {
        UNIX_EPOCH + Duration::from_millis(at.unix_millis() as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis((-at.unix_millis()) as u64)
    };
    file.set_modified(time).expect("set mtime");
}

fn report_class(report: &vowp_protocol::GcReport, class: CatalogClass) -> Option<&vowp_protocol::GcClassReport> {
    report.classes.iter().find(|entry| entry.class == class)
}

#[test]
fn gc_dry_run_lists_by_class_and_changes_nothing() {
    let node = TestNode::new();
    let store = node.store();
    let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(31)));
    let open = node.session(SessionState::Running, SessionRole::Worker, ts_now().minus(hours(1)));
    node.capsule(&closed, CapsuleTrigger::Manual);
    let old_run = node.write_run("old.jsonl", ts_now().minus(days(15)), b"old\n");
    let fresh_run = node.write_run("fresh.jsonl", ts_now().minus(hours(1)), b"fresh\n");
    let tmp = node.write_tmp("stale", ts_now().minus(days(2)));

    let report = gc(&node.deps, &store, &GcOptions::default()).expect("dry run");

    assert!(report.dry_run);
    assert_eq!(report.deleted_files, 0);
    assert_eq!(report.freed_bytes, 0);
    assert_eq!(report.compacted_sessions, 0);
    for class in [
        CatalogClass::Truth,
        CatalogClass::OpenWork,
        CatalogClass::ClosedSession,
        CatalogClass::RunOutput,
        CatalogClass::Cache,
    ] {
        let entry = report_class(&report, class).unwrap_or_else(|| panic!("{class:?} missing: {report:?}"));
        assert!(entry.sample_paths.len() <= 5, "{entry:?}");
    }
    let runs = report_class(&report, CatalogClass::RunOutput).expect("run output");
    assert_eq!(runs.files, 2);
    let open_work = report_class(&report, CatalogClass::OpenWork).expect("open work");
    assert!(
        open_work.sample_paths.iter().any(|path| path.ends_with("log.jsonl")),
        "open session log is not visible: {open_work:?}"
    );
    for path in [
        &old_run,
        &fresh_run,
        &tmp,
        &node.paths.log_path(&open),
        &node.paths.log_path(&closed),
    ] {
        assert!(path.exists(), "dry run changed {}", path.display());
    }
    assert!(store.require_session(&closed).is_ok());
}

#[test]
fn gc_apply_removes_only_expired_disposable_files() {
    let node = TestNode::new();
    let store = node.store();
    let old_run = node.write_run("old.jsonl", ts_now().minus(days(15)), b"old\n");
    let mid_run = node.write_run("mid.jsonl", ts_now().minus(days(2)), b"middle\n");
    let fresh_run = node.write_run("fresh.jsonl", ts_now().minus(hours(1)), b"fresh\n");
    let old_tmp = node.write_tmp("old", ts_now().minus(days(3)));
    let fresh_tmp = node.write_tmp("fresh", ts_now().minus(hours(1)));

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");

    assert!(!report.dry_run);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(!old_run.exists(), "15-day run output must be deleted");
    assert!(!mid_run.exists(), "2-day run output must be replaced by .gz");
    assert!(mid_run.with_file_name("mid.jsonl.gz").is_file());
    assert!(fresh_run.exists(), "fresh run output must stay");
    assert!(!old_tmp.exists(), "stale tmp file must be deleted");
    assert!(fresh_tmp.exists(), "fresh tmp file must stay");
    let gz = mid_run.with_file_name("mid.jsonl.gz");
    let entry = store
        .catalog_entry(&gz.display().to_string())
        .expect("catalog query")
        .expect("compressed run catalog row");
    assert_eq!(entry.class, CatalogClass::RunOutput);
    assert!(report.deleted_files >= 2, "{report:?}");
}

#[test]
fn gc_removes_run_output_after_fourteen_days() {
    let node = TestNode::new();
    let store = node.store();
    let old = node.write_run("old.jsonl", ts_now().minus(days(14)), b"fourteen\n");
    let young = node.write_run("young.stderr", ts_now().minus(days(13)), b"thirteen\n");
    let young_err = node.write_run("young.jsonl.err", ts_now().minus(days(13)), b"stderr\n");

    let dry = gc(&node.deps, &store, &GcOptions::default()).expect("dry");
    assert!(old.exists());
    assert_eq!(report_class(&dry, CatalogClass::RunOutput).expect("runs").files, 3);

    let applied = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(!old.exists(), "14-day-old run output must be deleted");
    assert!(
        young.with_file_name("young.stderr.gz").is_file(),
        "13-day-old run output is still inside retention and is compressed"
    );
    assert!(
        young_err.with_file_name("young.jsonl.err.gz").is_file(),
        "the adapter's .jsonl.err tee is compressed too"
    );
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
}

#[test]
fn gc_caps_run_output_at_fifty_megabytes() {
    let node = TestNode::new();
    let store = node.store();
    let path = node.paths.runs_dir().join("huge.jsonl");
    let file = std::fs::File::create(&path).expect("huge file");
    file.set_len(RUN_OUTPUT_CAP_BYTES + 4096).expect("sparse length");
    set_mtime(&path, ts_now().minus(hours(2)));

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let length = std::fs::metadata(&path).expect("metadata").len();
    assert!(
        length <= RUN_OUTPUT_CAP_BYTES,
        "cap did not hold: {length} > {RUN_OUTPUT_CAP_BYTES}"
    );
}

#[test]
fn tick_caps_run_output_and_creates_nightly_schedule() {
    let node = TestNode::new();
    let store = node.store();
    let path = node.paths.runs_dir().join("huge.jsonl");
    let file = std::fs::File::create(&path).expect("huge file");
    file.set_len(RUN_OUTPUT_CAP_BYTES + 8192).expect("sparse length");
    set_mtime(&path, ts_now().minus(hours(3)));

    tick(&node.deps, &store, ts_now()).expect("tick");
    assert!(std::fs::metadata(&path).expect("metadata").len() <= RUN_OUTPUT_CAP_BYTES);

    let schedule = store
        .schedule(lifecycle::GC_NIGHTLY_SCHEDULE)
        .expect("schedule query")
        .expect("gc-nightly schedule");
    assert_eq!(schedule.spec, "every 24h");
    assert_eq!(schedule.project, Some(node.project.clone()));
    assert!(matches!(
        schedule.action,
        vowp_protocol::ScheduleAction::Command { ref argv, .. }
            if argv == &["vowp".to_owned(), "gc".to_owned(), "--apply".to_owned()]
    ));

    tick(&node.deps, &store, ts_now()).expect("second tick");
    assert_eq!(store.list_schedules().expect("schedules").len(), 1);
}

#[test]
fn tick_creates_the_nightly_schedule_from_an_unbound_store() {
    let node = TestNode::new();
    let global = Store::open_global(&node.paths).expect("global store");
    tick(&node.deps, &global, ts_now()).expect("tick");
    assert!(
        node.store()
            .schedule(lifecycle::GC_NIGHTLY_SCHEDULE)
            .expect("schedule query")
            .is_some()
    );
}

#[test]
fn tick_caps_an_open_run_output_file() {
    let node = TestNode::new();
    let store = node.store();
    let run = Run {
        id: RunId::new(),
        job_id: None,
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: None,
        provider: None,
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at: ts_now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("open run");
    let path = node.paths.run_raw_path(&run.id);
    let file = std::fs::File::create(&path).expect("run output");
    file.set_len(RUN_OUTPUT_CAP_BYTES + 2048).expect("sparse length");

    tick(&node.deps, &store, ts_now()).expect("tick");
    assert!(
        std::fs::metadata(&path).expect("metadata").len() <= RUN_OUTPUT_CAP_BYTES,
        "an open run's tee is capped too"
    );
}

#[test]
fn gc_compacts_closed_session_to_capsule_and_summary() {
    let node = TestNode::new();
    let store = node.store();
    let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(31)));
    node.add_event(&closed, 1, ts_now().minus(days(32)), EventType::TurnStarted);
    node.add_event(&closed, 2, ts_now().minus(days(32)), EventType::AssistantMessage);
    node.add_event(&closed, 3, ts_now().minus(days(31)), EventType::TurnEnded);
    let blob_a = node.blob(&closed, &"a".repeat(64), b"blob a");
    let blob_b = node.blob(&closed, &"b".repeat(64), b"blob b");
    let log = node.paths.log_path(&closed);
    let lock = node.paths.lock_path(&closed);
    std::fs::write(&lock, b"1234").expect("lock");

    let open = node.session(SessionState::Running, SessionRole::Worker, ts_now().minus(hours(1)));

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.compacted_sessions, 1, "{report:?}");
    assert!(!log.exists(), "log must be deleted");
    assert!(!blob_a.exists() && !blob_b.exists(), "blobs must be deleted");
    assert!(!lock.exists(), "session lock must be deleted");
    assert!(node.paths.log_path(&open).exists(), "open session is untouched");

    let summary = std::fs::read_to_string(node.paths.session_dir(&closed).join("summary.md")).expect("summary");
    assert!(summary.contains("Events: 3"), "{summary}");
    assert!(summary.contains("Capsule:"), "{summary}");
    assert!(summary.contains("session "), "{summary}");

    let capsule = store.latest_capsule(&closed).expect("capsule query").expect("capsule");
    assert_eq!(capsule.trigger, CapsuleTrigger::JobEnd);
    assert_eq!(capsule.session_id, closed);
    assert_eq!(
        store.lifecycle_compacted_at(&closed).expect("compacted_at"),
        Some(ts_now())
    );
    assert_eq!(list_capsule_files(&node.paths, &closed).expect("capsules").len(), 1);
    assert!(store.require_session(&closed).is_ok(), "the index row must stay");

    let again = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("second apply");
    assert_eq!(again.compacted_sessions, 0);
    assert!(node.paths.session_dir(&closed).join("summary.md").is_file());
}

#[test]
fn gc_compaction_keeps_an_existing_capsule() {
    let node = TestNode::new();
    let store = node.store();
    let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(45)));
    let capsule_path = node.capsule(&closed, CapsuleTrigger::TurnEnd);
    let capsule_id = store
        .latest_capsule(&closed)
        .expect("query")
        .expect("capsule")
        .capsule_id;

    let report = compact_session(&node.deps, &store, &closed, ts_now()).expect("compact");
    assert!(!report.skipped);
    assert_eq!(report.capsule_id, Some(capsule_id));
    assert!(capsule_path.is_file());
    assert_eq!(list_capsule_files(&node.paths, &closed).expect("capsules").len(), 1);
}

#[test]
fn compacted_session_tail_and_export_are_explicit() {
    let node = TestNode::new();
    let store = node.store();
    let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(31)));
    compact_session(&node.deps, &store, &closed, ts_now()).expect("compact");

    // The authoritative log is gone, but the index and capsule survive so `session show
    // --capsule` and `session export` can still explain the history. The user-facing "session
    // compacted to capsule <id>" wording lives in `cmd/session.rs`.
    assert!(!node.paths.log_path(&closed).exists());
    assert!(SessionLog::open_read(&node.paths, &closed).is_err());
    assert!(store.require_session(&closed).is_ok());
    assert!(store.latest_capsule(&closed).expect("capsule query").is_some());
}

#[test]
fn gc_removes_merged_worktree_and_keeps_failed_seven_days() {
    let node = TestNode::new();
    node.init_git();
    let merged = node.add_worktree("merged");
    let failed_old = node.add_worktree("failed-old");
    let failed_young = node.add_worktree("failed-young");
    node.job("merged", JobState::Done, ts_now().minus(days(1)), Some(&merged));
    node.job(
        "failed-old",
        JobState::Failed,
        ts_now().minus(days(8)),
        Some(&failed_old),
    );
    node.job(
        "failed-young",
        JobState::Failed,
        ts_now().minus(days(3)),
        Some(&failed_young),
    );

    let store = node.store();
    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(!merged.exists(), "merged worktree must be removed");
    assert!(!failed_old.exists(), "8-day-old failed worktree must be removed");
    assert!(failed_young.exists(), "3-day-old failed worktree must stay");
    let branches = Command::new("git")
        .args(["branch", "--list", "job/failed-old"])
        .current_dir(&node.repo)
        .output()
        .expect("git branch");
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("job/failed-old"),
        "gc must keep the branch"
    );
}

#[test]
fn gc_prunes_host_samples_24h_full_30d_minute() {
    let node = TestNode::new();
    let store = node.store();
    node.sample(ts_now().minus(hours(25)), 5);
    node.sample(ts_now().minus(hours(23)), 5);
    node.sample(ts_now().minus(days(31)), 60);
    node.sample(ts_now().minus(days(29)), 60);

    let before = store
        .lifecycle_sample_rows(ts_now().plus(days(1)), ts_now().plus(days(1)))
        .expect("rows");
    assert_eq!(before.len(), 4);

    let dry = gc(&node.deps, &store, &GcOptions::default()).expect("dry");
    assert_eq!(report_class(&dry, CatalogClass::Sample).expect("samples").files, 2);
    let still = store
        .lifecycle_sample_rows(ts_now().plus(days(1)), ts_now().plus(days(1)))
        .expect("rows");
    assert_eq!(still.len(), 4, "dry run must not delete samples");

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let after = store
        .lifecycle_sample_rows(ts_now().plus(days(1)), ts_now().plus(days(1)))
        .expect("rows");
    assert_eq!(after.len(), 2, "the 25h full-rate and 31d minute row must go");
    assert_eq!(report.deleted_files, 2);
}

#[test]
fn gc_class_filter_limits_report_and_apply() {
    let node = TestNode::new();
    let store = node.store();
    let run = node.write_run("old.jsonl", ts_now().minus(days(15)), b"old\n");
    let tmp = node.write_tmp("old", ts_now().minus(days(3)));

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            class: Some(CatalogClass::RunOutput),
            now: Some(ts_now()),
        },
    )
    .expect("filtered apply");
    assert_eq!(report.classes.len(), 1);
    assert_eq!(report.classes[0].class, CatalogClass::RunOutput);
    assert!(!run.exists());
    assert!(tmp.exists(), "a run_output filter must not touch cache");
}

#[test]
fn gc_dry_run_does_not_cap_large_run_output() {
    let node = TestNode::new();
    let store = node.store();
    let path = node.paths.runs_dir().join("huge.jsonl");
    let file = std::fs::File::create(&path).expect("huge file");
    file.set_len(RUN_OUTPUT_CAP_BYTES + 1024).expect("sparse length");
    set_mtime(&path, ts_now().minus(hours(2)));

    gc(&node.deps, &store, &GcOptions::default()).expect("dry");
    assert!(
        std::fs::metadata(&path).expect("metadata").len() > RUN_OUTPUT_CAP_BYTES,
        "a dry run must change nothing"
    );
}

#[test]
fn catalog_rows_filter_mark_and_delete() {
    let node = TestNode::new();
    let store = node.store();
    let path = node.paths.runs_dir().join("catalog-run.jsonl");
    std::fs::write(&path, b"run\n").expect("run file");
    let entry = node.catalog_file(&path, CatalogClass::RunOutput, Some(ts_now().minus(days(1))));

    let listed = store
        .list_catalog(&CatalogFilter {
            class: Some(CatalogClass::RunOutput),
            ..CatalogFilter::default()
        })
        .expect("list catalog");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, entry.path);

    store.mark_catalog_deleted(&entry.id, ts_now()).expect("mark deleted");
    let marked = store.catalog_entry(&entry.path).expect("catalog query").expect("row");
    assert_eq!(marked.deleted_at, Some(ts_now()));
    assert!(store.delete_catalog(&entry.id).expect("delete"));
    assert!(!store.delete_catalog(&entry.id).expect("second delete"));
    assert!(store.catalog_entry(&entry.path).expect("catalog query").is_none());
}

#[test]
fn gc_never_compacts_an_orchestrator_session() {
    let node = TestNode::new();
    let store = node.store();
    let orchestrator = node.session(
        SessionState::Closed,
        SessionRole::Orchestrator,
        ts_now().minus(days(90)),
    );
    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert_eq!(report.compacted_sessions, 0);
    assert!(node.paths.log_path(&orchestrator).exists());
}

#[test]
fn catalog_protected_rows_survive_apply_even_when_expired() {
    let node = TestNode::new();
    let store = node.store();
    let bank = node.paths.root().join("release-banks/bank-1");
    std::fs::create_dir_all(bank.parent().expect("parent")).expect("bank dir");
    std::fs::write(&bank, b"bank").expect("bank file");
    let entry = node.catalog_file(&bank, CatalogClass::Production, Some(ts_now().minus(days(365))));

    let report = gc(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
    )
    .expect("apply");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(bank.is_file(), "production files are never deleted");
    let row = store
        .catalog_entry(&entry.path)
        .expect("catalog query")
        .expect("catalog row");
    assert_eq!(row.deleted_at, None);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn gc_apply_never_deletes_protected_classes(
        bank_suffix in "[a-z][a-z0-9]{1,8}",
        secret_suffix in "[A-Z][A-Z0-9_]{1,8}",
    ) {
        let node = TestNode::new();
        let store = node.store();

        let bank = node.paths.root().join("release-banks").join(&bank_suffix);
        std::fs::create_dir_all(bank.parent().expect("parent")).expect("bank dir");
        std::fs::write(&bank, b"bank").expect("bank");
        node.catalog_file(&bank, CatalogClass::Production, Some(ts_now().minus(days(365))));

        let secret = node.paths.root().join("secrets").join(&secret_suffix);
        std::fs::create_dir_all(secret.parent().expect("parent")).expect("secret dir");
        std::fs::write(&secret, b"secret").expect("secret");

        let gate_blob = node
            .paths
            .root()
            .join("gates/blobs")
            .join("c".repeat(64));
        std::fs::create_dir_all(gate_blob.parent().expect("parent")).expect("gate blob dir");
        std::fs::write(&gate_blob, b"passing gate evidence").expect("gate blob");

        let open = node.session(SessionState::Running, SessionRole::Worker, ts_now().minus(hours(1)));
        let open_log = node.paths.log_path(&open);

        let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(hours(1)));
        let capsule = node.capsule(&closed, CapsuleTrigger::Manual);

        let project_db = node.paths.project_db_path(&node.project);
        let global_db = node.paths.db_path();
        let protected = vec![
            bank.clone(),
            secret.clone(),
            open_log.clone(),
            capsule.clone(),
            project_db.clone(),
            global_db.clone(),
            gate_blob.clone(),
        ];
        for path in &protected {
            prop_assert!(path.exists(), "test setup missing {}", path.display());
        }

        let report = gc(
            &node.deps,
            &store,
            &GcOptions {
                apply: true,
                ..GcOptions::default()
            },
        ).expect("apply");
        prop_assert!(report.errors.is_empty(), "{:?}", report.errors);
        for path in &protected {
            prop_assert!(path.exists(), "gc deleted protected {}", path.display());
        }
        let production = store.catalog_entry(&bank.display().to_string()).expect("catalog query");
        prop_assert_eq!(production.and_then(|row| row.deleted_at), None);
    }
}

#[test]
fn class_names_round_trip_through_the_parser() {
    for class in [
        CatalogClass::Production,
        CatalogClass::Truth,
        CatalogClass::OpenWork,
        CatalogClass::Secrets,
        CatalogClass::ClosedSession,
        CatalogClass::RunOutput,
        CatalogClass::Worktree,
        CatalogClass::Sample,
        CatalogClass::Cache,
    ] {
        assert_eq!(lifecycle::parse_class(class_name(class)).expect("parse"), class);
    }
    assert!(lifecycle::parse_class("open-work").is_ok());
    assert!(lifecycle::parse_class("nonsense").is_err());
}

#[test]
fn classify_returns_the_node_database_as_truth() {
    let node = TestNode::new();
    let store = node.store();
    let files = lifecycle::classify(&node.deps, &store, ts_now()).expect("classify");
    let db = node.paths.db_path().display().to_string();
    let entry = files
        .iter()
        .find(|entry| entry.path.display().to_string() == db)
        .expect("global DB classified");
    assert_eq!(entry.class, CatalogClass::Truth);
}

#[test]
fn compact_session_skips_an_open_session() {
    let node = TestNode::new();
    let store = node.store();
    let open = node.session(SessionState::Running, SessionRole::Worker, ts_now().minus(days(90)));
    let report = compact_session(&node.deps, &store, &open, ts_now()).expect("compact");
    assert!(report.skipped);
    assert!(node.paths.log_path(&open).exists());
}

#[test]
fn job_filter_includes_the_worktree_rows_used_by_classification() {
    let node = TestNode::new();
    node.init_git();
    let worktree = node.add_worktree("pending-job");
    node.job("pending-job", JobState::Running, ts_now(), Some(&worktree));
    let store = node.store();
    let jobs = store.list_jobs(&JobFilter::default()).expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].worktree.as_deref(),
        Some(worktree.display().to_string().as_str())
    );
    let files = lifecycle::classify(&node.deps, &store, ts_now()).expect("classify");
    let entry = files
        .iter()
        .find(|entry| entry.path == worktree)
        .expect("worktree classified");
    assert_eq!(entry.class, CatalogClass::OpenWork);
}
