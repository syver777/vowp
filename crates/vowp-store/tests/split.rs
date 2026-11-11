//! Global + per-project store split tests.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.1–§3.8 and §15.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{
    Actor, Clock, Cost, CostSource, EventId, EventIndexRow, EventType, FakeClock, HarnessKind, InboxId, InboxItem,
    InboxKind, InboxState, JobId, JobKind, LedgerEntry, LedgerKind, NewJob, Node, NodeName, Project, ProjectName, Role,
    Run, RunId, RunPurpose, RunRole, Session, SessionId, SessionRole, SessionState, Timestamp, Tokens, WorkerKind,
};
use vowp_store::{CostGroup, CostQuery, LedgerFilter, NodePaths, Store, StoreError};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

fn now() -> Timestamp {
    Timestamp::parse(EPOCH).expect("epoch")
}

fn project(name: &str) -> ProjectName {
    ProjectName::parse(name).expect("project")
}

fn project_row(name: &str) -> Project {
    Project {
        name: project(name),
        path: format!("/tmp/{name}"),
        remote_url: None,
        default_branch: "main".to_owned(),
        config: serde_json::json!({}),
        created_at: now(),
        updated_at: now(),
    }
}

struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    store: Store,
    clock: FakeClock,
    project: ProjectName,
}

impl Fixture {
    fn new() -> Self {
        Self::with_project("proj")
    }

    fn with_project(name: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(now());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = project(name);
        store.upsert_project(&project_row(name)).expect("insert project");
        Self {
            _tmp: tmp,
            paths,
            store,
            clock,
            project,
        }
    }

    fn add_project(&self, name: &str) -> ProjectName {
        self.store.upsert_project(&project_row(name)).expect("insert project");
        project(name)
    }
}

fn new_job(id: &str) -> NewJob {
    NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("job {id}"),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    }
}

fn session(id: &SessionId, project: Option<ProjectName>, state: SessionState) -> Session {
    Session {
        id: id.clone(),
        project,
        job_id: None,
        role: Some(SessionRole::Worker),
        title: "test session".to_owned(),
        parent_session_id: None,
        node: NodeName::parse("local").expect("node"),
        cwd: "/tmp/proj".to_owned(),
        state,
        executor: None,
        last_seq: 0,
        last_capsule_id: None,
        log_bytes: 0,
        created_at: now(),
        updated_at: now(),
    }
}

fn run(id: &RunId, session: Option<&SessionId>, job: Option<&JobId>, started_at: Timestamp) -> Run {
    Run {
        id: id.clone(),
        job_id: job.cloned(),
        session_id: session.cloned(),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: None,
        provider: None,
        node: NodeName::parse("local").expect("node"),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at,
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    }
}

fn inbox(id: &InboxId, project: Option<ProjectName>, job: Option<JobId>, created_at: Timestamp) -> InboxItem {
    InboxItem {
        id: id.clone(),
        project,
        job_id: job,
        kind: InboxKind::Question,
        title: "question".to_owned(),
        body: String::new(),
        options: vec!["yes".to_owned(), "no".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at,
        answered_at: None,
    }
}

fn ledger(project: Option<ProjectName>, summary: &str, run: Option<RunId>) -> LedgerEntry {
    LedgerEntry {
        id: None,
        ts: now(),
        kind: LedgerKind::Cost,
        project,
        job_id: None,
        run_id: run,
        session_id: None,
        role: Some(RunRole::Worker),
        provider: Some("claude".to_owned()),
        harness: Some(HarnessKind::ClaudeCode),
        model: Some("m".to_owned()),
        cost: Some(Cost {
            usd: Some(1.0),
            tokens: Tokens::default(),
            source: CostSource::Receipt,
        }),
        summary: summary.to_owned(),
        detail: serde_json::json!({}),
    }
}

fn event_row(session: &SessionId, seq: u64, event_type: EventType, ts: Timestamp) -> EventIndexRow {
    EventIndexRow {
        session_id: session.clone(),
        seq,
        id: EventId::new(),
        ts,
        event_type,
        actor: Actor::System,
        harness: HarnessKind::Vowp,
        model: None,
        byte_offset: 0,
        byte_len: 0,
        summary: String::new(),
    }
}

fn schema_table_names(schema: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = schema;
    while let Some(index) = rest.find("CREATE TABLE ") {
        let after = &rest[index + "CREATE TABLE ".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        names.push(name);
        rest = after;
    }
    names
}

#[test]
fn global_schema_loads_and_matches_enums() {
    let fixture = Fixture::new();
    let conn = Connection::open(fixture.paths.db_path()).expect("global db");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 2);
    for table in schema_table_names(include_str!("../src/schema/global.sql")) {
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [&table],
                |row| row.get(0),
            )
            .expect("table lookup");
        assert_eq!(found, 1, "missing global table {table}");
    }
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'jobs'",
            [],
            |row| row.get(0),
        )
        .expect("jobs table");
    assert_eq!(tables, 0, "jobs live only in a project database");
}

#[test]
fn project_schema_loads_and_matches_enums() {
    let fixture = Fixture::new();
    let conn = Connection::open(fixture.paths.project_db_path(&fixture.project)).expect("project db");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 1);
    for table in schema_table_names(include_str!("../src/schema/project.sql")) {
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [&table],
                |row| row.get(0),
            )
            .expect("table lookup");
        assert_eq!(found, 1, "missing project table {table}");
    }
}

#[test]
fn global_and_project_schemas_share_no_application_table_names() {
    fn tables(schema: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = schema;
        while let Some(index) = rest.find("CREATE TABLE ") {
            let after = &rest[index + "CREATE TABLE ".len()..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            names.push(name);
            rest = after;
        }
        names
    }
    let global = tables(include_str!("../src/schema/global.sql"));
    let project = tables(include_str!("../src/schema/project.sql"));
    for table in &project {
        assert!(
            !global.contains(table),
            "table {table} exists in both the global and project schemas"
        );
    }
    assert!(global.contains(&"node_inbox".to_owned()));
    assert!(global.contains(&"node_ledger".to_owned()));
    assert!(project.contains(&"inbox".to_owned()));
    assert!(project.contains(&"ledger".to_owned()));
}

#[test]
fn store_binds_global_plus_exactly_one_project() {
    let fixture = Fixture::new();
    assert_eq!(fixture.store.bound_project(), Some(&fixture.project));
    assert_eq!(fixture.store.global_schema_version().expect("global"), 2);
    assert_eq!(fixture.store.project_schema_version().expect("project"), 1);

    let other = fixture.add_project("other");
    assert_eq!(
        fixture.store.bound_project(),
        Some(&fixture.project),
        "a real bound handle never re-attaches"
    );
    let error = fixture
        .store
        .insert_job(&other, &new_job("wrong-job"), "human")
        .expect_err("wrong project");
    assert!(matches!(error, StoreError::WrongProject { .. }), "got {error:?}");

    let global = Store::open_global(&fixture.paths).expect("global handle");
    assert!(global.bound_project().is_none());
    assert!(matches!(
        global.list_jobs(&Default::default()),
        Err(StoreError::NoProject)
    ));
    let bound = Store::open_with_project(&fixture.paths, &other).expect("bound other");
    assert_eq!(bound.bound_project(), Some(&other));
}

#[test]
fn two_project_handles_never_shadow_each_other() {
    let fixture = Fixture::new();
    let other = fixture.add_project("other");
    let alpha_job = fixture
        .store
        .insert_job(&fixture.project, &new_job("alpha-job"), "human")
        .expect("alpha job");
    let beta = Store::open_with_project(&fixture.paths, &other).expect("beta handle");
    let beta_job = beta
        .insert_job(&other, &new_job("beta-job"), "human")
        .expect("beta job");

    assert_eq!(
        fixture.store.require_job(&alpha_job.id).expect("alpha").project,
        fixture.project
    );
    assert_eq!(beta.require_job(&beta_job.id).expect("beta").project, other);
    assert!(
        fixture.store.job(&beta_job.id).expect("lookup").is_none(),
        "the alpha handle cannot see beta rows"
    );
    assert!(beta.job(&alpha_job.id).expect("lookup").is_none());
    assert_eq!(
        fixture.store.list_jobs(&Default::default()).expect("alpha jobs").len(),
        1
    );
    assert_eq!(beta.list_jobs(&Default::default()).expect("beta jobs").len(), 1);
}

#[test]
fn node_inbox_and_node_ledger_resolve_explicitly() {
    let fixture = Fixture::new();
    let node_item = inbox(&InboxId::new(), None, None, now());
    fixture.store.insert_inbox(&node_item).expect("node inbox");
    let project_item = inbox(
        &InboxId::new(),
        Some(fixture.project.clone()),
        None,
        fixture.clock.now(),
    );
    fixture.store.insert_inbox(&project_item).expect("project inbox");
    let named_node = inbox(
        &InboxId::new(),
        Some(fixture.project.clone()),
        None,
        fixture.clock.now(),
    );
    fixture.store.insert_node_inbox(&named_node).expect("named node inbox");

    let connection = Connection::open(fixture.paths.db_path()).expect("global db");
    let node_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM node_inbox", [], |row| row.get(0))
        .expect("node count");
    assert_eq!(node_count, 2, "two node rows, including the named one");
    let project_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'inbox'",
            [],
            |row| row.get(0),
        )
        .expect("inbox table");
    assert_eq!(project_count, 0, "the global file has no project inbox");

    let node_pen = ledger(None, "node cost", None);
    fixture.store.append_ledger(&node_pen).expect("node ledger");
    let project_pen = ledger(Some(fixture.project.clone()), "project cost", None);
    fixture.store.append_ledger(&project_pen).expect("project ledger");
    let project_conn = Connection::open(fixture.paths.project_db_path(&fixture.project)).expect("project db");
    let project_ledger: i64 = project_conn
        .query_row("SELECT COUNT(*) FROM ledger", [], |row| row.get(0))
        .expect("project ledger count");
    assert_eq!(project_ledger, 1);
    let node_ledger: i64 = connection
        .query_row("SELECT COUNT(*) FROM node_ledger", [], |row| row.get(0))
        .expect("node ledger count");
    assert_eq!(node_ledger, 1);
}

#[test]
fn inbox_list_merges_node_and_project_rows() {
    let fixture = Fixture::new();
    let other = fixture.add_project("other");
    let first = inbox(&InboxId::new(), None, None, now());
    fixture.store.insert_inbox(&first).expect("node");
    let job = fixture
        .store
        .insert_job(&fixture.project, &new_job("inbox-job"), "human")
        .expect("job");
    let second = inbox(
        &InboxId::new(),
        Some(fixture.project.clone()),
        Some(job.id.clone()),
        fixture.clock.now(),
    );
    fixture.store.insert_inbox(&second).expect("project");
    let named = inbox(
        &InboxId::new(),
        Some(fixture.project.clone()),
        None,
        fixture.clock.now(),
    );
    fixture.store.insert_node_inbox(&named).expect("named node");
    let elsewhere = inbox(&InboxId::new(), Some(other.clone()), None, fixture.clock.now());
    fixture.store.insert_node_inbox(&elsewhere).expect("other node");

    let items = fixture.store.list_inbox(None).expect("merged inbox");
    let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&first.id.as_str()));
    assert!(ids.contains(&second.id.as_str()));
    assert!(ids.contains(&named.id.as_str()));
    assert!(!ids.contains(&elsewhere.id.as_str()));

    let other_store = Store::open_with_project(&fixture.paths, &other).expect("other store");
    let other_items = other_store.list_inbox(None).expect("other inbox");
    assert_eq!(other_items.len(), 2, "the fleet-wide node row is visible everywhere");
    assert!(other_items.iter().any(|item| item.id == elsewhere.id));
    assert!(other_items.iter().any(|item| item.id == first.id));
}

#[test]
fn inbox_list_on_an_unbound_handle_returns_node_rows_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open_global(&paths).expect("global");
    let item = inbox(&InboxId::new(), None, None, now());
    store.insert_node_inbox(&item).expect("node item");
    store.upsert_project(&project_row("proj")).expect("project registry");
    let project_item = inbox(&InboxId::new(), Some(project("proj")), None, now());
    let bound = Store::open_with_project(&paths, &project("proj")).expect("bound");
    bound.insert_inbox(&project_item).expect("project item");
    assert_eq!(
        store.list_inbox(None).expect("fleet inbox"),
        vec![item],
        "the unbound handle does not read a project inbox"
    );
}

#[test]
fn ledger_since_and_cost_totals_merge_both_files() {
    let fixture = Fixture::new();
    fixture
        .store
        .append_ledger(&ledger(Some(fixture.project.clone()), "project", None))
        .expect("project ledger");
    fixture
        .store
        .append_ledger(&ledger(None, "node", None))
        .expect("node ledger");
    let entries = fixture
        .store
        .ledger_since(&LedgerFilter::default())
        .expect("both ledgers");
    assert_eq!(entries.len(), 2);

    // A node row may carry a project id; cost totals include it with the project ledger.
    let conn = Connection::open(fixture.paths.db_path()).expect("global db");
    conn.execute(
        "INSERT INTO node_ledger (ts, kind, project, provider, summary, usd, cost_source)
         VALUES (?1, 'cost', 'proj', 'claude', 'node project cost', 1.0, 'receipt')",
        [now().as_rfc3339()],
    )
    .expect("node project row");
    let totals = fixture
        .store
        .cost_totals(&CostQuery::default(), CostGroup::Provider)
        .expect("cost totals");
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].entries, 2);
    assert_eq!(totals[0].usd, 2.0);

    let unbound = Store::open_global(&fixture.paths).expect("global handle");
    let node_totals = unbound
        .cost_totals(&CostQuery::default(), CostGroup::Job)
        .expect("node totals");
    assert_eq!(node_totals[0].entries, 2, "both node rows, none of the project row");
}

#[test]
fn project_column_is_validated_not_foreign_keyed() {
    let fixture = Fixture::new();
    let job = fixture
        .store
        .insert_job(&fixture.project, &new_job("valid-job"), "human")
        .expect("job");
    let connection = Connection::open(fixture.paths.project_db_path(&fixture.project)).expect("project db");
    let mut statement = connection
        .prepare("PRAGMA foreign_key_list('jobs')")
        .expect("foreign keys");
    let rows = statement.query_map([], |row| row.get::<_, String>(2)).expect("fk rows");
    for row in rows {
        assert_ne!(row.expect("fk row"), "projects");
    }
    connection
        .execute(
            "INSERT INTO jobs (
                id, project, title, kind, role, profile, brief_path, report_path,
                state_changed_at, created_at, updated_at
             ) VALUES ('bad-job', 'other', 'bad', 'build', 'worker', 'worker', 'b', 'r', ?1, ?1, ?1)",
            [now().as_rfc3339()],
        )
        .expect("no cross-file foreign key rejects the row");
    let error = fixture
        .store
        .list_jobs(&Default::default())
        .expect_err("a stored jobs.project that disagrees with the handle is corrupt");
    assert!(matches!(error, StoreError::Corrupt { .. }), "got {error:?}");
    assert_eq!(
        fixture.store.require_job(&job.id).expect("valid row").project,
        fixture.project
    );
}

#[test]
fn every_sql_statement_names_its_database() {
    let tables = [
        "jobs",
        "edges",
        "runs",
        "gate_results",
        "workers",
        "loops",
        "schedules",
        "inbox",
        "ledger",
        "releases",
        "node_inbox",
        "node_ledger",
        "sessions",
        "session_events",
        "session_natives",
        "capsules",
        "switches",
    ];
    let sources = [
        include_str!("../src/repo_jobs.rs"),
        include_str!("../src/repo_runs.rs"),
        include_str!("../src/repo_loops.rs"),
        include_str!("../src/repo_misc.rs"),
        include_str!("../src/repo_sessions.rs"),
        include_str!("../src/repo_ledger.rs"),
        include_str!("../src/reindex.rs"),
    ];
    for source in sources {
        for table in tables {
            for keyword in ["FROM", "INTO", "UPDATE", "JOIN", "DELETE FROM"] {
                let unqualified = format!("{keyword} {table}");
                assert!(
                    !source.contains(&unqualified),
                    "unqualified `{unqualified}` in a vowp-store statement"
                );
            }
        }
    }
}

#[test]
fn schema_version_compat_shim_reports_project_one() {
    let fixture = Fixture::new();
    assert_eq!(fixture.store.schema_version().expect("compat"), 1);
    assert_eq!(fixture.store.project_schema_version().expect("project"), 1);
    let global = Store::open_global(&fixture.paths).expect("global");
    assert_eq!(global.schema_version().expect("compat"), 1);
}

#[test]
fn global_schema_version_reports_two() {
    let fixture = Fixture::new();
    assert_eq!(fixture.store.global_schema_version().expect("global"), 2);
    assert_eq!(
        Store::open_global(&fixture.paths)
            .expect("global")
            .global_schema_version()
            .expect("v"),
        2
    );
}

#[test]
fn upsert_project_creates_the_project_db() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let project = project("made");
    store.upsert_project(&project_row("made")).expect("upsert");
    let path = paths.project_db_path(&project);
    assert!(path.is_file(), "project database was created");
    let conn = Connection::open(path).expect("project db");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 1);
    drop(conn);
    let reopened = Store::open(&paths).expect("reopen");
    assert_eq!(reopened.bound_project(), Some(&project));
}

#[test]
fn node_bound_handle_rebinds_once_to_first_real_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store.upsert_project(&project_row("_node")).expect("node project");
    assert_eq!(store.bound_project().map(ProjectName::as_str), Some("_node"));
    store.upsert_project(&project_row("real")).expect("real project");
    assert_eq!(store.bound_project().map(ProjectName::as_str), Some("real"));
    store.upsert_project(&project_row("second")).expect("second project");
    assert_eq!(
        store.bound_project().map(ProjectName::as_str),
        Some("real"),
        "a real handle never re-binds"
    );
}

#[test]
fn real_bound_handle_never_rebinds() {
    let fixture = Fixture::new();
    let first = fixture.project.clone();
    let other = fixture.add_project("other");
    assert_ne!(first, other);
    assert_eq!(fixture.store.bound_project(), Some(&first));
}

#[test]
fn fresh_home_deps_open_store_binds_node_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let global = Store::open_global(&paths).expect("global");
    global.upsert_project(&project_row("_node")).expect("node project");
    drop(global);
    let store = Store::open_with_project(&paths, &project("_node")).expect("bound node");
    assert_eq!(store.bound_project().map(ProjectName::as_str), Some("_node"));
    assert!(paths.project_db_path(&project("_node")).is_file());
}

#[test]
fn cost_ledger_replay_does_not_duplicate() {
    let fixture = Fixture::new();
    let run = RunId::new();
    let entry = ledger(Some(fixture.project.clone()), "cost", Some(run.clone()));
    fixture.store.append_ledger(&entry).expect("first cost");
    let error = fixture.store.append_ledger(&entry).expect_err("replay");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
    let entries = fixture.store.ledger_since(&LedgerFilter::default()).expect("ledger");
    assert_eq!(entries.len(), 1);
}

#[test]
fn request_queue_is_project_scoped() {
    let fixture = Fixture::new();
    let other = fixture.add_project("other");
    let other_store = Store::open_with_project(&fixture.paths, &other).expect("other store");
    let alpha_request = fixture
        .store
        .push_request("retry_job", &serde_json::json!({"job": "alpha"}))
        .expect("alpha request");
    let beta_request = other_store
        .push_request("retry_job", &serde_json::json!({"job": "beta"}))
        .expect("beta request");
    let alpha = fixture.store.take_requests().expect("alpha drain");
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].id, alpha_request);
    let beta = other_store.take_requests().expect("beta drain");
    assert_eq!(beta.len(), 1);
    assert_eq!(beta[0].id, beta_request);
    assert!(fixture.store.take_requests().expect("empty").is_empty());
}

#[test]
fn crash_between_global_and_project_write_is_repaired() {
    let fixture = Fixture::new();
    let session_id = SessionId::new();
    fixture
        .store
        .upsert_session(&session(
            &session_id,
            Some(fixture.project.clone()),
            SessionState::Running,
        ))
        .expect("session");
    fixture.clock.advance(Duration::from_secs(1));
    let run = run(&RunId::new(), Some(&session_id), None, now());
    fixture.store.insert_run(&run).expect("open run");
    fixture.clock.advance(Duration::from_secs(1));
    fixture
        .store
        .index_event(&event_row(&session_id, 1, EventType::TurnEnded, fixture.clock.now()))
        .expect("turn ended");

    let report = fixture.store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.runs_closed, 1);
    let stored = fixture.store.require_run(&run.id).expect("run");
    assert!(stored.ended_at.is_some());
    assert!(matches!(stored.outcome, Some(vowp_protocol::RunOutcome::Stalled)));
    let second = fixture.store.reconcile_cross_db().expect("idempotent reconcile");
    assert_eq!(second, Default::default());
}

#[test]
fn cross_db_reconcile_repairs_sessions_and_terminal_events() {
    let fixture = Fixture::new();
    let session_id = SessionId::new();
    fixture
        .store
        .upsert_session(&session(
            &session_id,
            Some(fixture.project.clone()),
            SessionState::Running,
        ))
        .expect("session");
    let report = fixture.store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.sessions_repaired, 1);
    assert_eq!(
        fixture.store.require_session(&session_id).expect("session").state,
        SessionState::Idle
    );

    let session_id = SessionId::new();
    fixture
        .store
        .upsert_session(&session(&session_id, Some(fixture.project.clone()), SessionState::Idle))
        .expect("session");
    let run = run(&RunId::new(), Some(&session_id), None, now());
    fixture.store.insert_run(&run).expect("run");
    fixture
        .store
        .finish_run(
            &run.id,
            &vowp_store::RunFinish {
                outcome: vowp_protocol::RunOutcome::Ok,
                exit_code: Some(0),
                limit_kind: None,
                cost: Cost::unknown(),
                native_session_id: None,
                detail: String::new(),
            },
        )
        .expect("finish");
    let report = fixture.store.reconcile_cross_db().expect("reconcile terminal");
    assert_eq!(report.terminal_events_repaired, 1);
    assert_eq!(report.ledgers_repaired, 1);
    let second = fixture.store.reconcile_cross_db().expect("idempotent");
    assert_eq!(second, Default::default());
}

#[test]
fn reconcile_repairs_project_less_session_on_node_handle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store.upsert_project(&project_row("_node")).expect("node project");
    let session_id = SessionId::new();
    store
        .upsert_session(&session(&session_id, None, SessionState::Running))
        .expect("project-less session");

    let report = store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.sessions_repaired, 1);
    assert_eq!(
        store.require_session(&session_id).expect("session").state,
        SessionState::Idle
    );
    let second = store.reconcile_cross_db().expect("idempotent");
    assert_eq!(second, Default::default());
}

#[test]
fn cross_db_reconcile_is_idempotent() {
    let fixture = Fixture::new();
    let session_id = SessionId::new();
    fixture
        .store
        .upsert_session(&session(
            &session_id,
            Some(fixture.project.clone()),
            SessionState::Running,
        ))
        .expect("session");
    let first = fixture.store.reconcile_cross_db().expect("first");
    assert_eq!(first.sessions_repaired, 1);
    let second = fixture.store.reconcile_cross_db().expect("second");
    assert_eq!(second, Default::default());
    let third = fixture.store.reconcile_cross_db().expect("third");
    assert_eq!(third, Default::default());
}

#[test]
fn open_memory_attaches_one_lazy_project_database() {
    let store = Store::open_memory().expect("memory store");
    assert!(store.bound_project().is_none());
    store.upsert_project(&project_row("mem")).expect("project");
    let project = project("mem");
    assert_eq!(store.bound_project(), Some(&project));
    let job = store
        .insert_job(&project, &new_job("memory-job"), "human")
        .expect("insert job");
    assert_eq!(store.require_job(&job.id).expect("job"), job);
    store
        .append_ledger(&ledger(Some(project), "memory ledger", None))
        .expect("ledger");
    assert_eq!(store.ledger_since(&LedgerFilter::default()).expect("ledger").len(), 1);
}

#[test]
fn read_only_handles_see_both_files_and_never_write() {
    let fixture = Fixture::new();
    let job = fixture
        .store
        .insert_job(&fixture.project, &new_job("read-only-job"), "human")
        .expect("job");
    let read_only = Store::open_read_only(&fixture.paths, Some(&fixture.project)).expect("read-only handle");
    assert_eq!(read_only.bound_project(), Some(&fixture.project));
    assert_eq!(read_only.global_schema_version().expect("global"), 2);
    assert_eq!(read_only.require_job(&job.id).expect("job"), job);
    let error = read_only
        .insert_job(&fixture.project, &new_job("write-attempt"), "human")
        .expect_err("query_only refuses writes");
    assert!(matches!(error, StoreError::Sqlite(_)), "got {error:?}");
}

#[test]
fn node_and_project_rows_use_reserved_node_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store.upsert_project(&project_row("_node")).expect("node project");
    let node_project = project("_node");
    let session_id = SessionId::new();
    store
        .upsert_session(&session(&session_id, None, SessionState::Idle))
        .expect("project-less session");
    let run = run(&RunId::new(), Some(&session_id), None, now());
    store.insert_run(&run).expect("node run");
    assert_eq!(
        Store::open_with_project(&paths, &node_project)
            .expect("node store")
            .require_run(&run.id)
            .expect("run")
            .id,
        run.id
    );
    store
        .upsert_node(&Node {
            name: NodeName::parse("local").expect("node"),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: now(),
        })
        .expect("node row");
    store
        .ensure_worker_slots(&NodeName::parse("local").expect("node"), WorkerKind::Worker, 1)
        .expect("worker slot");
}
