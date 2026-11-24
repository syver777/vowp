//! `vowp gc --older-than` tests: the duration parser, a dry run, apply, refusals and the
//! reindex consistency that follows a compaction.
//!
//! Every test uses a temporary node home, a temporary project checkout and a fixed fake clock.
//! No test reads the real home, starts tmux, touches the network or runs a harness.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tempfile::TempDir;
use vowp_core::lifecycle::{self, GcOptions, gc_with_window, parse_older_than};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    Actor, CatalogClass, EventBody, EventId, EventIndexRow, EventSource, EventType, Executor, FakeClock, HarnessKind,
    Job, JobId, JobKind, JobState, MemoryLog, NewJob, NodeName, Project, ProjectName, Role, Session, SessionCreated,
    SessionId, SessionRole, SessionState, Timestamp, VowpConfig,
};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

/// The fixed instant every test starts at.
fn ts_now() -> Timestamp {
    Timestamp::parse("2026-11-10T12:00:00Z").expect("timestamp")
}

/// A whole number of days as a duration.
fn days(days: u64) -> Duration {
    Duration::from_secs(days * 24 * 60 * 60)
}

/// A whole number of hours as a duration.
fn hours(hours: u64) -> Duration {
    Duration::from_secs(hours * 60 * 60)
}

/// One temporary home, project and dependency set with a fixed clock.
struct TestNode {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    repo: PathBuf,
    deps: Deps,
    clock: FakeClock,
}

impl TestNode {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temp dir");
        let project = ProjectName::parse("test").expect("project");
        let repo = tmp.path().join("project");
        std::fs::create_dir_all(&repo).expect("project dir");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(repo.display().to_string());

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
        self.session_for_job(state, role, updated_at, None)
    }

    fn session_for_job(
        &self,
        state: SessionState,
        role: SessionRole,
        updated_at: Timestamp,
        job: Option<&JobId>,
    ) -> SessionId {
        let id = SessionId::new();
        std::fs::create_dir_all(self.paths.blobs_dir(&id)).expect("blobs dir");
        std::fs::create_dir_all(self.paths.capsules_dir(&id)).expect("capsules dir");
        let log = self.paths.log_path(&id);
        std::fs::write(&log, b"{\"seq\":1}\n").expect("log");
        let session = Session {
            id: id.clone(),
            project: Some(self.project.clone()),
            job_id: job.cloned(),
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

    fn hold_lease(&self, session: &SessionId, heartbeat_at: Timestamp) {
        let lease = vowp_protocol::Lease {
            session_id: session.clone(),
            surface_id: None,
            surface_kind: vowp_protocol::SurfaceKind::Api,
            node: NodeName::parse("local").expect("node"),
            holder: "test-surface".to_owned(),
            token: vowp_protocol::surface::new_lease_token(),
            acquired_at: heartbeat_at,
            heartbeat_at,
        };
        self.store().take_lease(&lease).expect("lease row");
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

    fn job(&self, id: &str, state: JobState, state_at: Timestamp) -> Job {
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

#[test]
fn older_than_duration_parser_accepts_units_and_rejects_bad_input() {
    assert_eq!(parse_older_than("30s").expect("seconds"), Duration::from_secs(30));
    assert_eq!(parse_older_than("90m").expect("minutes"), Duration::from_secs(90 * 60));
    assert_eq!(parse_older_than("36h").expect("hours"), hours(36));
    assert_eq!(parse_older_than("7d").expect("days"), days(7));
    assert_eq!(parse_older_than("2w").expect("weeks"), days(14));
    assert_eq!(parse_older_than(" 7D ").expect("trimmed and case-insensitive"), days(7));
    assert_eq!(parse_older_than("3650d").expect("maximum"), days(3650));

    for bad in [
        "",
        "   ",
        "0d",
        "0s",
        "-7d",
        "+7d",
        "7",
        "7x",
        "1.5d",
        "7 d",
        "999999999999999999999d",
        "3651d",
    ] {
        let error = parse_older_than(bad).expect_err(bad);
        assert!(
            matches!(error.kind(), vowp_protocol::ErrorKind::InvalidInput),
            "{bad:?}: {error:?}"
        );
    }
}

#[test]
fn gc_older_than_dry_run_lists_eligible_and_changes_nothing() {
    let node = TestNode::new();
    let store = node.store();
    let old_session = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    let old_blob = node.blob(&old_session, &"a".repeat(64), b"old blob");
    let young_session = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(3)));
    let old_run = node.write_run("old.jsonl", ts_now().minus(days(10)), b"old run\n");
    let young_run = node.write_run("young.jsonl", ts_now().minus(days(2)), b"young run\n");

    let outcome = gc_with_window(&node.deps, &store, &GcOptions::default(), Some(days(7))).expect("dry run");

    assert!(outcome.dry_run);
    assert_eq!(outcome.deleted_files, 0);
    assert_eq!(outcome.freed_bytes, 0);
    assert_eq!(outcome.compacted_sessions, 0);
    let closed = outcome
        .classes
        .iter()
        .find(|class| class.class == CatalogClass::ClosedSession)
        .expect("closed sessions");
    assert_eq!(closed.files, 3, "{closed:?}");
    assert_eq!(closed.eligible_files, 2, "{closed:?}");
    assert!(closed.eligible_bytes >= 11, "{closed:?}");
    let runs = outcome
        .classes
        .iter()
        .find(|class| class.class == CatalogClass::RunOutput)
        .expect("run output");
    assert_eq!(runs.files, 2, "{runs:?}");
    assert_eq!(runs.eligible_files, 1, "{runs:?}");
    assert!(runs.eligible_bytes >= 8, "{runs:?}");

    for path in [
        &old_blob,
        &node.paths.log_path(&old_session),
        &node.paths.log_path(&young_session),
        &old_run,
        &young_run,
    ] {
        assert!(path.exists(), "dry run changed {}", path.display());
    }
}

#[test]
fn gc_older_than_apply_compacts_old_sessions_and_reindex_stays_consistent() {
    let node = TestNode::new();
    let store = node.store();
    let old = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    let older = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(20)));
    let young = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(3)));
    let stale_tmp = node.paths.tmp_dir().join("stale");
    std::fs::write(&stale_tmp, b"temporary").expect("tmp file");
    set_mtime(&stale_tmp, ts_now().minus(days(3)));
    node.add_event(&old, 1, ts_now().minus(days(11)), EventType::TurnStarted);
    node.add_event(&old, 2, ts_now().minus(days(10)), EventType::TurnEnded);
    let old_blob = node.blob(&old, &"a".repeat(64), b"blob for the old session");
    let older_blob = node.blob(&older, &"b".repeat(64), b"blob for the older session");
    let young_log = node.paths.log_path(&young);

    // Replace the minimal fixture line with a valid session.created event, so the post-gc
    // `reindex` proves a remaining session is still readable rather than merely tolerated.
    std::fs::remove_file(&young_log).expect("remove minimal log");
    let log = SessionLog::create(&node.paths, &young, Arc::new(node.clock.clone())).expect("young log");
    log.append(NewEvent {
        actor: Actor::System,
        source: EventSource {
            harness: HarnessKind::Vowp,
            model: None,
            node: None,
            native_session_id: None,
        },
        body: EventBody::SessionCreated(SessionCreated {
            title: "young session".to_owned(),
            cwd: node.repo.display().to_string(),
            project: Some(node.project.clone()),
            job_id: None,
            role: Some(SessionRole::Worker),
            parent_session_id: None,
        }),
        raw: None,
        ts: Some(ts_now().minus(days(3))),
    })
    .expect("session.created event");
    log.sync().expect("sync young log");
    drop(log);

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("apply");

    assert!(!outcome.dry_run);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
    assert_eq!(outcome.compacted_sessions, 2, "{outcome:?}");
    assert!(outcome.freed_bytes > 0, "{outcome:?}");
    assert!(!node.paths.log_path(&old).exists());
    assert!(!node.paths.log_path(&older).exists());
    assert!(!old_blob.exists());
    assert!(!older_blob.exists());
    assert!(!stale_tmp.exists(), "other classes keep their default retention");
    assert!(young_log.exists(), "a session younger than the window stays");
    assert!(node.paths.session_dir(&old).join("summary.md").is_file());
    assert!(node.paths.session_dir(&older).join("summary.md").is_file());
    for session in [&old, &older, &young] {
        assert!(
            store.require_session(session).is_ok(),
            "session row {session} must stay"
        );
        assert!(
            store.latest_capsule(session).expect("capsule query").is_some() || session == &young,
            "each compacted session keeps a capsule"
        );
    }

    // Compaction removes log.jsonl; reindex still has to succeed, keep every session row and
    // leave the remaining session readable.
    let reindexed = vowp_store::reindex::reindex(&store, &node.paths, None).expect("reindex");
    assert!(reindexed.sessions >= 1, "{reindexed:?}");
    for session in [&old, &older, &young] {
        assert!(store.require_session(session).is_ok(), "reindex dropped {session}");
    }
    assert!(young_log.exists());
    assert!(vowp_store::session_log::SessionLog::open_read(&node.paths, &young).is_ok());
}

#[test]
fn gc_older_than_class_filter_leaves_the_other_overridable_class_alone() {
    let node = TestNode::new();
    let store = node.store();
    let closed = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    let run = node.write_run("old.jsonl", ts_now().minus(days(10)), b"old run\n");

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            class: Some(CatalogClass::ClosedSession),
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("closed-session apply");

    assert_eq!(outcome.compacted_sessions, 1, "{outcome:?}");
    assert!(!node.paths.log_path(&closed).exists());
    assert!(run.exists(), "a closed_session filter must not touch run output");

    let second = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            class: Some(CatalogClass::RunOutput),
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("run-output apply");

    assert_eq!(outcome.compacted_sessions, 0, "{outcome:?}");
    assert!(!run.exists(), "a run_output filter collects old run output");
    assert!(
        node.paths.log_path(&second).exists(),
        "a run_output filter must not compact a closed session"
    );
}

#[test]
fn gc_older_than_refuses_a_live_lease() {
    let node = TestNode::new();
    let store = node.store();
    let session = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    node.hold_lease(&session, ts_now());
    let log = node.paths.log_path(&session);

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("apply");

    assert_eq!(outcome.compacted_sessions, 0, "{outcome:?}");
    assert!(log.exists(), "a live lease must keep the log");
    let refusal = outcome
        .refused
        .iter()
        .find(|entry| entry.session_id.as_ref() == Some(&session))
        .unwrap_or_else(|| panic!("live lease not listed: {:?}", outcome.refused));
    assert_eq!(refusal.class, CatalogClass::ClosedSession);
    assert!(refusal.reason.contains("live lease"), "{refusal:?}");
}

#[test]
fn gc_older_than_refuses_a_running_or_reviewing_job() {
    let node = TestNode::new();
    let store = node.store();
    let running = node.job("running-old", JobState::Running, ts_now().minus(hours(2)));
    let reviewing = node.job("reviewing-old", JobState::Reviewing, ts_now().minus(hours(1)));
    let running_session = node.session_for_job(
        SessionState::Closed,
        SessionRole::Worker,
        ts_now().minus(days(10)),
        Some(&running.id),
    );
    let reviewing_session = node.session_for_job(
        SessionState::Closed,
        SessionRole::Worker,
        ts_now().minus(days(10)),
        Some(&reviewing.id),
    );

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("apply");

    assert_eq!(outcome.compacted_sessions, 0, "{outcome:?}");
    assert!(node.paths.log_path(&running_session).exists());
    assert!(node.paths.log_path(&reviewing_session).exists());
    let running_refusal = outcome
        .refused
        .iter()
        .find(|entry| entry.session_id.as_ref() == Some(&running_session))
        .unwrap_or_else(|| panic!("running job not listed: {:?}", outcome.refused));
    assert!(running_refusal.reason.contains("running"), "{running_refusal:?}");
    let reviewing_refusal = outcome
        .refused
        .iter()
        .find(|entry| entry.session_id.as_ref() == Some(&reviewing_session))
        .unwrap_or_else(|| panic!("reviewing job not listed: {:?}", outcome.refused));
    assert!(reviewing_refusal.reason.contains("reviewing"), "{reviewing_refusal:?}");
}

#[test]
fn gc_older_than_refuses_a_running_orchestrator() {
    let node = TestNode::new();
    let store = node.store();
    let orchestrator = node.session(
        SessionState::Running,
        SessionRole::Orchestrator,
        ts_now().minus(days(10)),
    );

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect("apply");

    assert!(node.paths.log_path(&orchestrator).exists());
    let refusal = outcome
        .refused
        .iter()
        .find(|entry| entry.session_id.as_ref() == Some(&orchestrator))
        .unwrap_or_else(|| panic!("running orchestrator not listed: {:?}", outcome.refused));
    assert_eq!(refusal.class, CatalogClass::OpenWork);
    assert!(refusal.reason.contains("running orchestrator"), "{refusal:?}");
}

#[test]
fn gc_older_than_rejects_zero_and_unsupported_classes() {
    let node = TestNode::new();
    let store = node.store();
    let error =
        gc_with_window(&node.deps, &store, &GcOptions::default(), Some(Duration::ZERO)).expect_err("zero window");
    assert!(matches!(error, vowp_core::CoreError::Invalid(_)), "{error:?}");

    let error = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            class: Some(CatalogClass::Cache),
            ..GcOptions::default()
        },
        Some(days(7)),
    )
    .expect_err("unsupported class");
    assert!(matches!(error, vowp_core::CoreError::Invalid(_)), "{error:?}");

    let error = gc_with_window(&node.deps, &store, &GcOptions::default(), Some(days(3651))).expect_err("absurd window");
    assert!(matches!(error, vowp_core::CoreError::Invalid(_)), "{error:?}");
}

#[test]
fn default_gc_keeps_the_published_retention_windows() {
    let node = TestNode::new();
    let store = node.store();
    let session = node.session(SessionState::Closed, SessionRole::Worker, ts_now().minus(days(10)));
    let run = node.write_run("ten-days.jsonl", ts_now().minus(days(10)), b"ten days\n");

    let outcome = gc_with_window(&node.deps, &store, &GcOptions::default(), None).expect("dry run");
    let closed = outcome
        .classes
        .iter()
        .find(|class| class.class == CatalogClass::ClosedSession)
        .expect("closed sessions");
    assert_eq!(closed.eligible_files, 0, "10 days is inside the 30-day default");
    let runs = outcome
        .classes
        .iter()
        .find(|class| class.class == CatalogClass::RunOutput)
        .expect("run output");
    assert_eq!(runs.eligible_files, 0, "10 days is inside the 14-day default");
    assert!(node.paths.log_path(&session).exists());
    assert!(run.exists());

    // The nightly behaviour stays the protocol report with the same class totals.
    let report = lifecycle::gc(&node.deps, &store, &GcOptions::default()).expect("default gc");
    assert!(report.dry_run);
    assert!(
        report
            .classes
            .iter()
            .any(|class| class.class == CatalogClass::ClosedSession)
    );
}
