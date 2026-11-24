//! Offload persistence: global v6 assignments and node limits, project v2 job placement.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §7.1, §11.1 and §11.2.

use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{
    AssignmentPayload, Executor, HarnessKind, JobId, JobKind, LimitKind, NewJob, Node, NodeFallback, NodeName, PathRef,
    Placement, Project, ProjectName, Role, RunId, RunPurpose, SecretName, Session, SessionId, SessionRole,
    SessionState, Timestamp,
};
use vowp_store::migrate_v05;
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState, AssignmentFilter, NodeLimit};
use vowp_store::{NodePaths, Store, StoreError};

/// A temporary home with one bound project and one pending job.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    store: Store,
    project: ProjectName,
    job: JobId,
}

fn now() -> Timestamp {
    Timestamp::parse("2026-11-14T00:00:00.000Z").expect("timestamp")
}

fn later() -> Timestamp {
    Timestamp::parse("2026-11-14T01:00:00.000Z").expect("timestamp")
}

fn node(name: &str, is_local: bool) -> Node {
    Node {
        name: NodeName::parse(name).expect("node name"),
        ssh_target: if is_local { None } else { Some("user@host".to_owned()) },
        is_local,
        capabilities: serde_json::json!({}),
        last_seen_at: None,
        created_at: now(),
    }
}

fn project_row(name: &ProjectName, root: &Path) -> Project {
    Project {
        name: name.clone(),
        path: root.display().to_string(),
        remote_url: None,
        default_branch: "main".to_owned(),
        config: serde_json::json!({}),
        created_at: now(),
        updated_at: now(),
    }
}

fn new_job(id: &JobId) -> NewJob {
    NewJob {
        id: id.clone(),
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

fn session(project: &ProjectName) -> Session {
    Session {
        id: SessionId::new(),
        project: Some(project.clone()),
        job_id: None,
        role: Some(SessionRole::Worker),
        title: "offload test".to_owned(),
        parent_session_id: None,
        node: NodeName::parse("local").expect("node"),
        cwd: "/tmp/project".to_owned(),
        state: SessionState::Idle,
        executor: None,
        last_seq: 0,
        last_capsule_id: None,
        log_bytes: 0,
        created_at: now(),
        updated_at: now(),
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("open store");
    let project = ProjectName::parse("alpha").expect("project");
    store
        .upsert_project(&project_row(&project, tmp.path()))
        .expect("project row");
    let job = JobId::parse("job-a").expect("job id");
    store.insert_job(&project, &new_job(&job), "human").expect("job");
    Fixture {
        _tmp: tmp,
        paths,
        store,
        project,
        job,
    }
}

fn payload(
    assignment_id: &str,
    project: &ProjectName,
    job: &JobId,
    run: &RunId,
    session: &SessionId,
    epoch: u64,
) -> AssignmentPayload {
    AssignmentPayload {
        assignment_id: assignment_id.to_owned(),
        run_id: run.clone(),
        session_id: session.clone(),
        project: project.clone(),
        job: job.clone(),
        executor: Executor {
            harness: HarnessKind::Opencode,
            model: "provider/model".to_owned(),
            effort: None,
            variant: Some("max".to_owned()),
        },
        purpose: RunPurpose::Start,
        prompt: "Implement the brief in the job worktree".to_owned(),
        cwd_ref: PathRef::Worktree {
            project: project.clone(),
            job: job.clone(),
        },
        base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        remote: "origin".to_owned(),
        remote_url: "https://example.invalid/owner/repo.git".to_owned(),
        result_transport: vowp_protocol::ResultTransport::Push,
        env_names: vec!["CARGO_HOME".to_owned()],
        secret_names: vec![SecretName::parse("CARGO_REGISTRY_TOKEN").expect("secret name")],
        epoch,
        deadline: Some(later()),
        spool_cap_bytes: 64 * 1024 * 1024,
    }
}

fn assignment(fixture: &Fixture, id: &str, epoch: u64) -> AgentAssignment {
    let run = RunId::new();
    let session = SessionId::new();
    AgentAssignment {
        id: id.to_owned(),
        run_id: run.clone(),
        session_id: session.clone(),
        project: fixture.project.clone(),
        node: NodeName::parse("mac-studio").expect("node"),
        epoch,
        state: AgentAssignmentState::Queued,
        payload: payload(id, &fixture.project, &fixture.job, &run, &session, epoch),
        created_at: now(),
        claimed_at: None,
        expires_at: Some(later()),
        detail: String::new(),
    }
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .expect("table lookup")
        > 0
}

/// Downgrades a fresh v6 global file to the v5 shape used by the migration test.
fn downgrade_global_to_v5(paths: &NodePaths) {
    let conn = Connection::open(paths.db_path()).expect("global db");
    conn.execute_batch(
        "PRAGMA wal_checkpoint(TRUNCATE);
         DROP TABLE capacity_rollups;
         DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         PRAGMA user_version = 5;",
    )
    .expect("downgrade global");
}

#[test]
fn migration_v5_to_v6_adds_agent_assignments_and_node_limits_preserving_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let project = ProjectName::parse("alpha").expect("project");
    let session_id;
    {
        let store = Store::open(&paths).expect("open");
        store.upsert_node(&node("local", true)).expect("local node");
        store
            .upsert_project(&project_row(&project, tmp.path()))
            .expect("project");
        let item = session(&project);
        session_id = item.id.clone();
        store.upsert_session(&item).expect("session");
        store.kv_set("seed", "kept").expect("kv");
    }
    downgrade_global_to_v5(&paths);
    {
        let conn = Connection::open(paths.db_path()).expect("v5 db");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 5);
        assert!(!table_exists(&conn, "agent_assignments"));
        assert!(!table_exists(&conn, "node_limits"));
    }

    let report = migrate_v05(&paths).expect("migrate v5 to v6");
    assert_eq!(report.from_version, 5);
    assert_eq!(report.to_version, 6);
    assert!(report.verified);
    assert!(!report.dry_run);
    assert!(Path::new(&report.backup).is_file(), "backup exists");
    assert_eq!(report.tables["global:agent_assignments"].before, 0);
    assert_eq!(report.tables["global:agent_assignments"].after, 0);
    assert_eq!(report.tables["global:node_limits"].before, 0);
    assert_eq!(report.tables["global:node_limits"].after, 0);
    assert_eq!(report.tables["global:nodes"].before, 1);
    assert_eq!(report.tables["global:nodes"].after, 1);
    assert_eq!(report.tables["global:projects"].before, 1);
    assert_eq!(report.tables["global:sessions"].before, 1);
    for table in ["kv", "nodes", "projects", "sessions", "surface_tokens", "secret_index"] {
        let key = format!("global:{table}");
        assert_eq!(
            report.tables[&key].before, report.tables[&key].after,
            "row count drift for {table}"
        );
    }

    let backup = Connection::open(&report.backup).expect("backup db");
    let backup_version: i64 = backup
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 5, "the backup is the untouched v5 file");
    assert!(!table_exists(&backup, "agent_assignments"));
    let backup_nodes: i64 = backup
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .expect("backup nodes");
    assert_eq!(backup_nodes, 1);
    drop(backup);

    let store = Store::open(&paths).expect("open migrated home");
    assert_eq!(store.global_schema_version().expect("version"), 8);
    assert_eq!(store.kv_get("seed").expect("kv"), Some("kept".to_owned()));
    assert_eq!(
        store.require_session(&session_id).expect("session").title,
        "offload test"
    );
    let conn = Connection::open(paths.db_path()).expect("migrated db");
    assert!(table_exists(&conn, "agent_assignments"));
    assert!(table_exists(&conn, "node_limits"));
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'agent_assignments_node'",
            [],
            |row| row.get(0),
        )
        .expect("index");
    assert_eq!(index, 1, "the (node, state) index exists");
    let mut statement = conn.prepare("PRAGMA foreign_key_check").expect("fk check");
    assert!(statement.query([]).expect("fk rows").next().expect("fk row").is_none());
}

#[test]
fn migration_v5_to_v6_failure_leaves_the_v5_file_intact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let project = ProjectName::parse("alpha").expect("project");
    {
        let store = Store::open(&paths).expect("open");
        store
            .upsert_project(&project_row(&project, tmp.path()))
            .expect("project");
    }
    downgrade_global_to_v5(&paths);
    {
        // A conflicting table makes CREATE TABLE node_limits fail after the first table was
        // created. The whole transaction must roll back.
        let conn = Connection::open(paths.db_path()).expect("v5 db");
        conn.execute_batch("CREATE TABLE node_limits (marker TEXT NOT NULL) STRICT;")
            .expect("conflicting table");
    }
    let error = migrate_v05(&paths).expect_err("the conflicting table fails the migration");
    assert!(matches!(error, StoreError::Sqlite(_)), "got {error:?}");
    let conn = Connection::open(paths.db_path()).expect("v5 db after failure");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 5, "the failed migration did not bump the version");
    assert!(
        !table_exists(&conn, "agent_assignments"),
        "the failed transaction left no partial new table"
    );
    let projects: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .expect("projects");
    assert_eq!(projects, 1, "the v5 file is still readable");
}

#[test]
fn migration_v1_to_v2_adds_job_placement_and_preserves_jobs() {
    let fixture = fixture();
    let other = JobId::parse("job-b").expect("job id");
    fixture
        .store
        .insert_job(&fixture.project, &new_job(&other), "human")
        .expect("second job");
    let project = fixture.project.clone();
    let paths = fixture.paths.clone();
    drop(fixture.store);

    {
        let conn = Connection::open(paths.project_db_path(&project)).expect("project db");
        conn.execute_batch(
            "DROP TABLE job_placement;
             PRAGMA user_version = 1;",
        )
        .expect("downgrade project");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 1);
        assert!(!table_exists(&conn, "job_placement"));
        let jobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .expect("jobs");
        assert_eq!(jobs, 2);
    }

    let store = Store::open_with_project(&paths, &project).expect("open project");
    assert_eq!(store.project_schema_version().expect("version"), 2);
    assert!(store.require_job(&JobId::parse("job-a").expect("job")).is_ok());
    assert!(store.require_job(&other).is_ok());
    assert!(store.list_job_placements().expect("placements").is_empty());

    let conn = Connection::open(paths.project_db_path(&project)).expect("migrated project");
    assert!(table_exists(&conn, "job_placement"));
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .expect("jobs");
    assert_eq!(jobs, 2, "every job row survived");
    drop(conn);

    let backups = std::fs::read_dir(paths.backups_dir())
        .expect("backups dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("backups");
    let backup = backups
        .iter()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("project-alpha-pre-0.6-"))
        })
        .expect("project backup exists");
    let backup_conn = Connection::open(&backup).expect("backup project");
    let backup_version: i64 = backup_conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 1);
    let backup_jobs: i64 = backup_conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .expect("backup jobs");
    assert_eq!(backup_jobs, 2);
}

#[test]
fn job_placement_round_trips_home_any_and_node() {
    let fixture = fixture();
    let node = NodeName::parse("mac-studio").expect("node");
    let cases = [
        (Placement::Home, now()),
        (Placement::Any, later()),
        (
            Placement::Node {
                node: node.clone(),
                fallback: None,
            },
            now(),
        ),
        (
            Placement::Node {
                node: node.clone(),
                fallback: Some(NodeFallback::Home),
            },
            later(),
        ),
    ];
    for (placement, at) in cases {
        fixture
            .store
            .set_job_placement(&fixture.job, placement.clone(), at)
            .expect("set placement");
        assert_eq!(
            fixture.store.job_placement(&fixture.job).expect("read placement"),
            Some(placement.clone())
        );
    }
    let placements = fixture.store.list_job_placements().expect("list placements");
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].job, fixture.job);
    assert_eq!(placements[0].updated_at, later());
    assert_eq!(
        placements[0].placement,
        Placement::Node {
            node,
            fallback: Some(NodeFallback::Home),
        }
    );

    assert!(fixture.store.clear_job_placement(&fixture.job).expect("clear"));
    assert!(!fixture.store.clear_job_placement(&fixture.job).expect("clear twice"));
    assert_eq!(fixture.store.job_placement(&fixture.job).expect("gone"), None);
    assert!(fixture.store.list_job_placements().expect("empty").is_empty());
}

#[test]
fn job_placement_cascades_with_its_job() {
    let fixture = fixture();
    fixture
        .store
        .set_job_placement(&fixture.job, Placement::Any, now())
        .expect("set placement");
    assert!(fixture.store.job_placement(&fixture.job).expect("placement").is_some());
    fixture.store.delete_job(&fixture.job).expect("delete job");
    assert_eq!(
        fixture.store.job_placement(&fixture.job).expect("cascaded"),
        None,
        "job_placement follows its job"
    );
    assert!(fixture.store.list_job_placements().expect("list").is_empty());
}

#[test]
fn assignment_round_trips_payload_and_state() {
    let fixture = fixture();
    fixture
        .store
        .upsert_node(&node("mac-studio", false))
        .expect("remote node");
    let item = assignment(&fixture, "assign-1", 1);
    fixture.store.insert_assignment(&item).expect("insert");
    assert_eq!(fixture.store.assignment("assign-1").expect("read"), Some(item.clone()));

    let claimed = later();
    let updated = fixture
        .store
        .set_assignment_state(
            "assign-1",
            AgentAssignmentState::Running,
            Some(claimed),
            "harness started",
        )
        .expect("set state");
    assert_eq!(updated.state, AgentAssignmentState::Running);
    assert_eq!(updated.claimed_at, Some(claimed));
    assert_eq!(updated.detail, "harness started");
    let read_back = fixture.store.assignment("assign-1").expect("read").expect("row");
    assert_eq!(read_back, updated);

    let node = NodeName::parse("mac-studio").expect("node");
    assert_eq!(fixture.store.assignments_for_node(&node).expect("by node").len(), 1);
    assert_eq!(
        fixture.store.assignments_for_run(&item.run_id).expect("by run").len(),
        1
    );
    let filter = AssignmentFilter {
        node: Some(node),
        project: Some(fixture.project.clone()),
        states: vec![AgentAssignmentState::Running],
        ..Default::default()
    };
    assert_eq!(fixture.store.list_assignments(&filter).expect("filtered").len(), 1);
    let empty = AssignmentFilter {
        states: vec![AgentAssignmentState::Queued],
        ..Default::default()
    };
    assert!(fixture.store.list_assignments(&empty).expect("empty").is_empty());
}

#[test]
fn assignment_remote_url_round_trips_through_the_store() {
    let fixture = fixture();
    let item = assignment(&fixture, "assign-remote", 1);
    fixture.store.insert_assignment(&item).expect("insert");

    let stored = fixture.store.assignment("assign-remote").expect("read").expect("row");
    assert_eq!(stored.payload.remote, "origin");
    assert_eq!(stored.payload.remote_url, item.payload.remote_url);
    assert_eq!(
        stored.payload.remote_url, "https://example.invalid/owner/repo.git",
        "the temp-home round-trip keeps the expected remote URL"
    );
}

#[test]
fn assignment_rejects_a_mismatched_payload_and_duplicate_run_epoch() {
    let fixture = fixture();

    type Mutation = fn(&mut AgentAssignment);
    let mismatches: [(&str, Mutation); 4] = [
        ("run_id", |item| item.payload.run_id = RunId::new()),
        ("session_id", |item| item.payload.session_id = SessionId::new()),
        ("project", |item| {
            item.payload.project = ProjectName::parse("other").expect("project")
        }),
        ("epoch", |item| item.payload.epoch += 1),
    ];
    for (field, mutate) in mismatches {
        let mut item = assignment(&fixture, &format!("mismatch-{field}"), 1);
        mutate(&mut item);
        let error = fixture.store.insert_assignment(&item).expect_err("mismatch must fail");
        match error {
            StoreError::InvalidInput(message) => assert!(message.contains(field), "{message}"),
            other => panic!("expected InvalidInput for {field}, got {other:?}"),
        }
        assert!(
            fixture
                .store
                .assignment(&format!("mismatch-{field}"))
                .expect("lookup")
                .is_none(),
            "a rejected assignment was not written"
        );
    }

    let item = assignment(&fixture, "assign-1", 1);
    fixture.store.insert_assignment(&item).expect("first insert");

    let mut duplicate = assignment(&fixture, "assign-2", 1);
    duplicate.run_id = item.run_id.clone();
    duplicate.payload.run_id = item.run_id.clone();
    duplicate.session_id = item.session_id.clone();
    duplicate.payload.session_id = item.session_id.clone();
    let error = fixture
        .store
        .insert_assignment(&duplicate)
        .expect_err("duplicate epoch");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");

    let mut lower = assignment(&fixture, "assign-lower", 0);
    lower.run_id = item.run_id.clone();
    lower.payload.run_id = item.run_id.clone();
    lower.session_id = item.session_id.clone();
    lower.payload.session_id = item.session_id.clone();
    let error = fixture.store.insert_assignment(&lower).expect_err("lower epoch");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");

    let mut higher = assignment(&fixture, "assign-3", 2);
    higher.run_id = item.run_id.clone();
    higher.payload.run_id = item.run_id.clone();
    higher.session_id = item.session_id.clone();
    higher.payload.session_id = item.session_id.clone();
    fixture
        .store
        .insert_assignment(&higher)
        .expect("higher epoch supersedes");
    assert_eq!(
        fixture.store.assignments_for_run(&item.run_id).expect("by run").len(),
        2,
        "both epochs stay as separate rows"
    );
}

#[test]
fn assignment_payload_json_contains_no_lease_token_or_absolute_path() {
    let fixture = fixture();
    let item = assignment(&fixture, "assign-1", 1);
    fixture.store.insert_assignment(&item).expect("insert");

    let conn = Connection::open(fixture.paths.db_path()).expect("global db");
    let raw: String = conn
        .query_row(
            "SELECT payload_json FROM agent_assignments WHERE id = 'assign-1'",
            [],
            |row| row.get(0),
        )
        .expect("payload json");
    assert!(!raw.contains("lease_token"), "{raw}");
    assert!(!raw.contains("lease_expires_at"), "{raw}");
    assert!(raw.contains("CARGO_REGISTRY_TOKEN"), "names are stored: {raw}");

    let value: serde_json::Value = serde_json::from_str(&raw).expect("payload is JSON");
    fn assert_no_secret_or_absolute_path(value: &serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                assert!(!text.starts_with('/'), "absolute path in payload: {text}");
                assert!(!text.starts_with("~/"), "home-relative path in payload: {text}");
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_secret_or_absolute_path(item);
                }
            }
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        key != "lease_token" && key != "token" && key != "secret_value",
                        "secret field {key} in payload"
                    );
                    assert_no_secret_or_absolute_path(value);
                }
            }
            _ => {}
        }
    }
    assert_no_secret_or_absolute_path(&value);
}

#[test]
fn node_limit_upserts_per_provider_and_node() {
    let fixture = fixture();
    let mac = NodeName::parse("mac-studio").expect("node");
    let other = NodeName::parse("mac-mini").expect("node");
    fixture.store.upsert_node(&node("mac-studio", false)).expect("node");
    fixture.store.upsert_node(&node("mac-mini", false)).expect("node");

    let mut limit = NodeLimit {
        provider: "claude".to_owned(),
        node: mac.clone(),
        kind: LimitKind::Quota,
        resets_at: Some(later()),
        detail: "weekly quota".to_owned(),
        since: now(),
        updated_at: now(),
    };
    fixture.store.upsert_node_limit(&limit).expect("insert");
    assert_eq!(fixture.store.node_limits(&mac).expect("limits"), vec![limit.clone()]);

    limit.kind = LimitKind::Auth;
    limit.detail = "login expired".to_owned();
    limit.since = later();
    limit.updated_at = later();
    fixture.store.upsert_node_limit(&limit).expect("update");
    let auth = fixture.store.node_limits(&mac).expect("limits");
    assert_eq!(auth.len(), 1);
    assert_eq!(auth[0].kind, LimitKind::Auth);
    assert_eq!(auth[0].since, later(), "a new kind restarts since");

    let keep_since = auth[0].since;
    limit.detail = "still expired".to_owned();
    limit.updated_at = now();
    fixture.store.upsert_node_limit(&limit).expect("same kind update");
    let same = fixture.store.node_limits(&mac).expect("limits");
    assert_eq!(same[0].detail, "still expired");
    assert_eq!(same[0].since, keep_since, "same kind preserves since");

    let codex = NodeLimit {
        provider: "codex".to_owned(),
        node: mac.clone(),
        kind: LimitKind::Rate,
        resets_at: None,
        detail: "rate".to_owned(),
        since: now(),
        updated_at: now(),
    };
    fixture.store.upsert_node_limit(&codex).expect("second provider");
    let both = fixture.store.node_limits(&mac).expect("limits");
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].provider, "claude");
    assert_eq!(both[1].provider, "codex");
    assert!(fixture.store.node_limits(&other).expect("other node").is_empty());

    assert!(fixture.store.clear_node_limit("codex", &mac).expect("clear"));
    assert!(!fixture.store.clear_node_limit("codex", &mac).expect("clear twice"));
    assert_eq!(fixture.store.node_limits(&mac).expect("one left").len(), 1);
}

#[test]
fn node_limit_clears_and_cascades_with_the_node() {
    let fixture = fixture();
    let mac = NodeName::parse("mac-studio").expect("node");
    fixture.store.upsert_node(&node("mac-studio", false)).expect("node");
    for (provider, kind) in [("claude", LimitKind::Quota), ("codex", LimitKind::Rate)] {
        fixture
            .store
            .upsert_node_limit(&NodeLimit {
                provider: provider.to_owned(),
                node: mac.clone(),
                kind,
                resets_at: None,
                detail: String::new(),
                since: now(),
                updated_at: now(),
            })
            .expect("limit");
    }
    assert_eq!(fixture.store.node_limits(&mac).expect("limits").len(), 2);
    assert!(fixture.store.clear_node_limit("claude", &mac).expect("clear"));
    assert_eq!(fixture.store.node_limits(&mac).expect("limits").len(), 1);
    fixture.store.remove_node(&mac).expect("remove node");
    assert!(
        fixture.store.node_limits(&mac).expect("cascaded").is_empty(),
        "node_limits cascades with its node"
    );
}

#[test]
fn claim_assignments_is_atomic_and_returns_the_updated_rows() {
    let fixture = fixture();
    for id in ["assign-a", "assign-b", "assign-c"] {
        fixture
            .store
            .insert_assignment(&assignment(&fixture, id, 1))
            .expect("insert");
    }
    let mac = NodeName::parse("mac-studio").expect("node");
    let claimed = fixture.store.claim_assignments(&mac, 2, now(), later()).expect("claim");
    assert_eq!(claimed.len(), 2, "the limit is honoured");
    for item in &claimed {
        assert_eq!(item.state, AgentAssignmentState::Sent);
        assert_eq!(item.claimed_at, Some(now()));
        assert_eq!(item.expires_at, Some(later()));
    }
    let again = fixture
        .store
        .claim_assignments(&mac, 2, now(), later())
        .expect("second claim");
    assert_eq!(again.len(), 1, "claimed rows are not claimed twice");
    assert!(again[0].id != claimed[0].id && again[0].id != claimed[1].id);
    assert!(
        fixture
            .store
            .claim_assignments(&mac, 0, now(), later())
            .expect("zero limit")
            .is_empty()
    );
}

#[test]
fn advance_assignment_rejects_an_invalid_transition_but_is_idempotent() {
    let fixture = fixture();
    let item = assignment(&fixture, "assign-a", 1);
    fixture.store.insert_assignment(&item).expect("insert");

    let sent = fixture
        .store
        .advance_assignment(&item.id, AgentAssignmentState::Sent, Some(now()), "sent")
        .expect("queued -> sent");
    assert_eq!(sent.state, AgentAssignmentState::Sent);

    let lost = fixture
        .store
        .advance_assignment(&item.id, AgentAssignmentState::Lost, None, "lost")
        .expect("sent -> lost");
    assert_eq!(lost.state, AgentAssignmentState::Lost);
    let error = fixture
        .store
        .advance_assignment(&item.id, AgentAssignmentState::Accepted, None, "late accept")
        .expect_err("a lost assignment cannot be accepted");
    assert!(matches!(error, StoreError::Conflict(_)), "{error}");
    assert_eq!(
        fixture.store.assignment(&item.id).expect("read").expect("exists").state,
        AgentAssignmentState::Lost,
        "a refused transition leaves the row unchanged"
    );

    // The same state is an idempotent replay for the agent protocol.
    let replay = fixture
        .store
        .advance_assignment(&item.id, AgentAssignmentState::Lost, None, "lost again")
        .expect("same-state replay");
    assert_eq!(replay.state, AgentAssignmentState::Lost);
}

#[test]
fn assignments_for_job_filters_on_the_durable_payload() {
    let fixture = fixture();
    fixture
        .store
        .insert_assignment(&assignment(&fixture, "assign-a", 1))
        .expect("insert");
    let other_job = JobId::parse("job-b").expect("job id");
    let mut other = assignment(&fixture, "assign-b", 1);
    other.payload.job = other_job.clone();
    fixture.store.insert_assignment(&other).expect("insert");

    let rows = fixture.store.assignments_for_job(&fixture.job).expect("by job");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "assign-a");
    assert!(fixture.store.assignments_for_job(&other_job).expect("other job")[0].id == "assign-b");
}
