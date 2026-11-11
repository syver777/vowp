//! Inbox, limits, releases, kv/request, cascade and backup repository tests for
//! `vowp-store`. Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{
    Clock, FakeClock, GuardResult, HarnessKind, InboxId, InboxItem, InboxKind, InboxState, Job, JobId, JobKind, Limit,
    LimitRowKind, NewJob, NodeName, Project, ProjectName, Release, ReleaseId, ReleaseState, Role, Session, SessionId,
    SessionRole, SessionState, Timestamp,
};
use vowp_store::{LedgerFilter, NodePaths, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A file-backed store with a controllable clock and one project.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    store: Store,
    clock: FakeClock,
    project: ProjectName,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project");
        let now = clock.now();
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: "/tmp/proj".to_owned(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("insert project");
        Self {
            _tmp: tmp,
            paths,
            store,
            clock,
            project,
        }
    }

    fn job(&self, project: &ProjectName, id: &str, needs: Vec<JobId>) -> Job {
        self.store
            .insert_job(
                project,
                &NewJob {
                    id: JobId::parse(id).expect("job id"),
                    title: format!("job {id}"),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: None,
                    needs,
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: None,
                    priority: None,
                    loop_: None,
                    brief_text: None,
                },
                "human",
            )
            .expect("insert job")
    }

    fn session(&self, project: Option<ProjectName>, job: Option<JobId>) -> Session {
        let now = self.clock.now();
        Session {
            id: SessionId::new(),
            project,
            job_id: job,
            role: Some(SessionRole::User),
            title: "test session".to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/tmp/proj".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Builds an open inbox item.
fn inbox_item(created_at: Timestamp) -> InboxItem {
    InboxItem {
        id: InboxId::new(),
        project: None,
        job_id: None,
        kind: InboxKind::Question,
        title: "Continue?".to_owned(),
        body: "The job needs a decision.".to_owned(),
        options: vec!["yes".to_owned(), "no".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at,
        answered_at: None,
    }
}

/// Builds a limit row.
fn limit(provider: &str, kind: LimitRowKind, since: Timestamp, resets_at: Option<Timestamp>) -> Limit {
    Limit {
        provider: provider.to_owned(),
        harness: Some(HarnessKind::ClaudeCode),
        kind,
        resets_at,
        detail: format!("{provider} limit"),
        since,
        last_probe_at: None,
        probe_fail_streak: 0,
        concurrency_cap: None,
        cap_until: None,
    }
}

/// Builds a release row.
fn release(project: &ProjectName, env: &str, created_at: Timestamp) -> Release {
    Release {
        id: ReleaseId::new(),
        project: project.clone(),
        env: env.to_owned(),
        change_ref: "abc1234".to_owned(),
        from_commit: Some("def5678".to_owned()),
        to_commit: None,
        state: ReleaseState::Planned,
        guards: Vec::new(),
        snapshot_path: None,
        rollback_cmd: None,
        probe: serde_json::json!({}),
        approved_by: None,
        rolled_back_by: None,
        note: String::new(),
        created_at,
        finished_at: None,
    }
}

#[test]
fn inbox_answer_flow() {
    let fixture = Fixture::new();
    let item = inbox_item(fixture.clock.now());
    fixture.store.insert_inbox(&item).expect("insert inbox item");
    assert_eq!(
        fixture.store.inbox_item(&item.id).expect("inbox item"),
        Some(item.clone())
    );
    assert_eq!(
        fixture.store.list_inbox(Some(InboxState::Open)).expect("open items"),
        vec![item.clone()]
    );
    assert!(
        fixture
            .store
            .list_inbox(Some(InboxState::Answered))
            .expect("answered items")
            .is_empty()
    );

    fixture.clock.advance(Duration::from_secs(5));
    let answered = fixture
        .store
        .answer_inbox(&item.id, "yes", "owner")
        .expect("answer item");
    assert_eq!(answered.state, InboxState::Answered);
    assert_eq!(answered.answer.as_deref(), Some("yes"));
    assert_eq!(answered.answered_by.as_deref(), Some("owner"));
    assert_eq!(answered.answered_at, Some(fixture.clock.now()));

    let error = fixture
        .store
        .answer_inbox(&item.id, "again", "owner")
        .expect_err("second answer conflicts");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");
    let error = fixture
        .store
        .dismiss_inbox(&item.id, "owner")
        .expect_err("dismissing an answered item conflicts");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");

    let mut second = inbox_item(fixture.clock.now());
    second.title = "Dismiss me".to_owned();
    fixture.store.insert_inbox(&second).expect("insert second item");
    fixture.store.dismiss_inbox(&second.id, "keeper").expect("dismiss item");
    let dismissed = fixture.store.inbox_item(&second.id).expect("item").expect("row");
    assert_eq!(dismissed.state, InboxState::Dismissed);
    assert_eq!(dismissed.answer, None);
    assert_eq!(dismissed.answered_by.as_deref(), Some("keeper"));
    assert_eq!(dismissed.answered_at, Some(fixture.clock.now()));

    // The schema keeps state and answered_at in lockstep even for raw SQL.
    let open = inbox_item(fixture.clock.now());
    fixture.store.insert_inbox(&open).expect("insert open item");
    let connection = Connection::open(fixture.paths.db_path()).expect("raw connection");
    let error = connection
        .execute(
            "UPDATE node_inbox SET state = 'answered' WHERE id = ?1",
            [open.id.as_str()],
        )
        .expect_err("answered without answered_at must fail the CHECK");
    assert!(error.to_string().contains("CHECK"), "got {error}");
    let error = connection
        .execute(
            "UPDATE node_inbox SET answered_at = ?1 WHERE id = ?2",
            [fixture.clock.now().as_rfc3339(), open.id.as_str().to_owned()],
        )
        .expect_err("open with answered_at must fail the CHECK");
    assert!(error.to_string().contains("CHECK"), "got {error}");

    let all = fixture.store.list_inbox(None).expect("all items");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, item.id);
    assert_eq!(all[1].id, second.id);
    assert_eq!(all[2].id, open.id);
}

#[test]
fn limits_active_and_clear() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let past = limit(
        "past",
        LimitRowKind::Quota,
        now.minus(Duration::from_secs(120)),
        Some(now.minus(Duration::from_secs(60))),
    );
    let future = limit(
        "future",
        LimitRowKind::Rate,
        now.minus(Duration::from_secs(120)),
        Some(now.plus(Duration::from_secs(60))),
    );
    let never = limit(
        "never",
        LimitRowKind::Credits,
        now.minus(Duration::from_secs(120)),
        None,
    );
    for row in [&past, &future, &never] {
        fixture.store.upsert_limit(row).expect("upsert limit");
    }

    assert_eq!(
        fixture.store.limit_for("past").expect("limit"),
        Some(past.clone()),
        "limit_for returns expired rows too"
    );
    assert_eq!(fixture.store.limit_for("missing").expect("limit"), None);
    let active = fixture.store.active_limits(now).expect("active limits");
    assert_eq!(
        active.iter().map(|row| row.provider.as_str()).collect::<Vec<_>>(),
        vec!["future", "never"]
    );

    assert!(fixture.store.clear_limit("past").expect("clear past"));
    assert!(!fixture.store.clear_limit("past").expect("already cleared"));
    assert_eq!(fixture.store.limit_for("past").expect("limit"), None);

    // The same kind keeps the original `since`; a different kind starts a new window.
    let mut updated = future.clone();
    updated.since = now.plus(Duration::from_secs(100));
    updated.resets_at = Some(now.plus(Duration::from_secs(120)));
    updated.detail = "still limited".to_owned();
    fixture.store.upsert_limit(&updated).expect("update same kind");
    let reloaded = fixture.store.limit_for("future").expect("limit").expect("row");
    assert_eq!(reloaded.since, future.since, "same-kind upsert preserves since");
    assert_eq!(reloaded.resets_at, updated.resets_at);
    assert_eq!(reloaded.detail, "still limited");

    let mut different = reloaded.clone();
    different.kind = LimitRowKind::Credits;
    different.since = now.plus(Duration::from_secs(200));
    fixture.store.upsert_limit(&different).expect("update different kind");
    let reloaded = fixture.store.limit_for("future").expect("limit").expect("row");
    assert_eq!(reloaded.kind, LimitRowKind::Credits);
    assert_eq!(reloaded.since, different.since, "a new kind starts a new window");
}

#[test]
fn record_probe_streak() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    fixture
        .store
        .upsert_limit(&limit(
            "claude",
            LimitRowKind::Quota,
            now,
            Some(now.plus(Duration::from_secs(3600))),
        ))
        .expect("upsert limit");

    let first = now.plus(Duration::from_secs(60));
    fixture.store.record_probe("claude", first, false).expect("probe 1");
    let row = fixture.store.limit_for("claude").expect("limit").expect("row");
    assert_eq!(row.probe_fail_streak, 1);
    assert_eq!(row.last_probe_at, Some(first));

    let second = now.plus(Duration::from_secs(120));
    fixture.store.record_probe("claude", second, false).expect("probe 2");
    let row = fixture.store.limit_for("claude").expect("limit").expect("row");
    assert_eq!(row.probe_fail_streak, 2);
    assert_eq!(row.last_probe_at, Some(second));

    let third = now.plus(Duration::from_secs(180));
    fixture.store.record_probe("claude", third, true).expect("probe 3");
    let row = fixture.store.limit_for("claude").expect("limit").expect("row");
    assert_eq!(row.probe_fail_streak, 0, "success resets the streak");
    assert_eq!(row.last_probe_at, Some(third));

    let error = fixture
        .store
        .record_probe("missing", third, false)
        .expect_err("probe of a missing limit");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn releases_roundtrip() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let mut first = release(&fixture.project, "prod", now);
    first.guards = vec![GuardResult {
        name: "clean_tree".to_owned(),
        passed: true,
        detail: String::new(),
    }];
    fixture.store.insert_release(&first).expect("insert release");
    assert_eq!(fixture.store.release(&first.id).expect("release"), Some(first.clone()));

    first.state = ReleaseState::Guarded;
    first.approved_by = Some("owner".to_owned());
    first.finished_at = Some(now.plus(Duration::from_secs(5)));
    first.note = "guards passed".to_owned();
    fixture.store.update_release(&first).expect("update release");
    assert_eq!(fixture.store.release(&first.id).expect("release"), Some(first.clone()));

    first.state = ReleaseState::Live;
    first.to_commit = Some("abc1234".to_owned());
    fixture.store.update_release(&first).expect("finish release");
    let live = fixture.store.release(&first.id).expect("release").expect("row");
    assert_eq!(live.state, ReleaseState::Live);
    assert_eq!(live.to_commit.as_deref(), Some("abc1234"));
    assert_eq!(live.created_at, now, "update keeps created_at");

    fixture.clock.advance(Duration::from_secs(30));
    let mut rollback = release(&fixture.project, "prod", fixture.clock.now());
    rollback.state = ReleaseState::RolledBack;
    rollback.rolled_back_by = Some(first.id.clone());
    fixture
        .store
        .insert_release(&rollback)
        .expect("insert rollback release");
    assert_eq!(
        fixture.store.release(&rollback.id).expect("release"),
        Some(rollback.clone())
    );

    let listed = fixture.store.list_releases(&fixture.project, None).expect("releases");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, rollback.id, "newest first");
    assert_eq!(listed[1].id, first.id);
    assert_eq!(
        fixture
            .store
            .list_releases(&fixture.project, Some("prod"))
            .expect("prod releases")
            .len(),
        2
    );
    assert!(
        fixture
            .store
            .list_releases(&fixture.project, Some("staging"))
            .expect("staging releases")
            .is_empty()
    );

    let mut invalid = release(&fixture.project, "prod", fixture.clock.now());
    invalid.rolled_back_by = Some(ReleaseId::new());
    let error = fixture
        .store
        .insert_release(&invalid)
        .expect_err("rolled_back_by must reference an existing release");
    assert!(
        matches!(error, vowp_store::StoreError::InvalidInput(_)),
        "got {error:?}"
    );

    let mut missing = release(&fixture.project, "prod", now);
    missing.id = ReleaseId::new();
    let error = fixture.store.update_release(&missing).expect_err("missing release");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn kv_and_requests_queue() {
    let fixture = Fixture::new();
    assert!(fixture.store.kv_get("alpha").expect("get").is_none());
    assert!(fixture.store.kv_prefix("req:").expect("requests").is_empty());

    fixture.store.kv_set("alpha", "one").expect("set alpha");
    assert_eq!(fixture.store.kv_get("alpha").expect("get"), Some("one".to_owned()));
    fixture.clock.advance(Duration::from_secs(1));
    fixture.store.kv_set("alpha", "two").expect("overwrite alpha");
    assert_eq!(fixture.store.kv_get("alpha").expect("get"), Some("two".to_owned()));
    fixture.store.kv_set("app:a", "1").expect("set app a");
    fixture.store.kv_set("app:b", "2").expect("set app b");
    assert_eq!(
        fixture.store.kv_prefix("app:").expect("app prefix"),
        vec![
            ("app:a".to_owned(), "1".to_owned()),
            ("app:b".to_owned(), "2".to_owned()),
        ]
    );
    assert!(fixture.store.kv_delete("app:a").expect("delete"));
    assert!(!fixture.store.kv_delete("app:a").expect("already gone"));
    assert_eq!(fixture.store.kv_get("app:a").expect("get"), None);

    let created_at = fixture.clock.now();
    let first = fixture
        .store
        .push_request("retry_job", &serde_json::json!({"job": "job-a"}))
        .expect("push retry");
    let second = fixture
        .store
        .push_request("pause_loop", &serde_json::json!({"name": "l4-planner"}))
        .expect("push pause");
    let third = fixture
        .store
        .push_request("wake", &serde_json::json!({}))
        .expect("push wake");
    assert_ne!(first, second);
    assert_ne!(second, third);
    assert_eq!(first.0.len(), 26, "request ids are ULIDs");
    assert_eq!(fixture.store.kv_prefix("req:").expect("request rows").len(), 3);

    let drained = fixture.store.take_requests().expect("take requests");
    assert_eq!(drained.len(), 3, "all three requests are drained");
    assert_eq!(
        drained.iter().map(|request| request.kind.as_str()).collect::<Vec<_>>(),
        vec!["retry_job", "pause_loop", "wake"],
        "requests come back in push order"
    );
    assert_eq!(drained[0].id, first);
    assert_eq!(drained[1].id, second);
    assert_eq!(drained[2].id, third);
    assert_eq!(drained[0].payload, serde_json::json!({"job": "job-a"}));
    assert_eq!(drained[1].payload, serde_json::json!({"name": "l4-planner"}));
    assert_eq!(drained[2].payload, serde_json::json!({}));
    assert!(drained.iter().all(|request| request.created_at == created_at));
    assert!(
        fixture.store.take_requests().expect("second take").is_empty(),
        "a second drain is empty and deletes nothing else"
    );
    assert_eq!(fixture.store.kv_get("alpha").expect("get"), Some("two".to_owned()));
}

#[test]
fn project_delete_keeps_other_projects_rows() {
    let fixture = Fixture::new();
    let job = fixture.job(&fixture.project, "kept-job", Vec::new());
    let job_id = job.id.clone();
    let session = fixture.session(Some(fixture.project.clone()), Some(job_id.clone()));
    fixture.store.upsert_session(&session).expect("insert session");

    let project_store = Store::open_with_project(&fixture.paths, &fixture.project).expect("project store");
    let connection = Connection::open(fixture.paths.db_path()).expect("raw connection");
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .expect("enable foreign keys");
    connection
        .execute("DELETE FROM projects WHERE name = ?1", [fixture.project.as_str()])
        .expect("delete project registry row");

    // No cross-file foreign key removes the project rows.
    assert_eq!(project_store.job(&job_id).expect("job"), Some(job));
    assert_eq!(
        project_store.list_edges(Some(&fixture.project)).expect("edges"),
        Vec::new()
    );
    let surviving = fixture.store.session(&session.id).expect("session").expect("row");
    assert_eq!(surviving.project, Some(fixture.project.clone()));
    assert_eq!(surviving.job_id, Some(job_id));
}

#[test]
fn backup_produces_openable_copy() {
    let fixture = Fixture::new();
    let job = fixture.job(&fixture.project, "backup-job", Vec::new());
    let session = fixture.session(Some(fixture.project.clone()), Some(job.id.clone()));
    fixture.store.upsert_session(&session).expect("insert session");
    let item = inbox_item(fixture.clock.now());
    fixture.store.insert_inbox(&item).expect("insert inbox");
    let ledger = vowp_protocol::LedgerEntry {
        id: None,
        ts: fixture.clock.now(),
        kind: vowp_protocol::LedgerKind::Note,
        project: Some(fixture.project.clone()),
        job_id: Some(job.id.clone()),
        run_id: None,
        session_id: Some(session.id.clone()),
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: "backup test".to_owned(),
        detail: serde_json::json!({"ok": true}),
    };
    fixture.store.append_ledger(&ledger).expect("append ledger");

    let destination = fixture.paths.root().join("backup.db");
    fixture.store.backup(&destination).expect("backup");
    let copy = Store::open_path(&destination).expect("open backup");
    copy.integrity_check().expect("backup integrity");
    assert_eq!(copy.global_schema_version().expect("global version"), 2);
    assert!(copy.bound_project().is_none(), "the legacy file is a global-only copy");
    assert_eq!(
        copy.list_projects().expect("projects").len(),
        1,
        "the global copy includes the registry"
    );
    assert_eq!(copy.session(&session.id).expect("session"), Some(session.clone()));
    assert!(matches!(copy.job(&job.id), Err(vowp_store::StoreError::NoProject)));

    // A split backup unit is the global copy plus each project file.
    let restored_root = tempfile::tempdir().expect("restored root");
    let restored = NodePaths::new(restored_root.path());
    restored.ensure().expect("restored dirs");
    std::fs::copy(&destination, restored.db_path()).expect("copy global");
    std::fs::create_dir_all(restored.project_dir(&fixture.project)).expect("project dir");
    let project_conn = Connection::open(fixture.paths.project_db_path(&fixture.project)).expect("project checkpoint");
    project_conn
        .execute_batch("PRAGMA wal_checkpoint(FULL)")
        .expect("checkpoint project");
    drop(project_conn);
    std::fs::copy(
        fixture.paths.project_db_path(&fixture.project),
        restored.project_db_path(&fixture.project),
    )
    .expect("copy project");
    let project_copy = Store::open_with_project(&restored, &fixture.project).expect("open copied project");
    project_copy.integrity_check().expect("project integrity");
    assert_eq!(project_copy.job(&job.id).expect("job"), Some(job));
    assert_eq!(project_copy.inbox_item(&item.id).expect("inbox"), Some(item));
    let copied_ledger = project_copy.ledger_since(&LedgerFilter::default()).expect("ledger");
    assert_eq!(copied_ledger.len(), 1);
    assert_eq!(copied_ledger[0].summary, "backup test");
}
