//! Migration, opening and path-layout tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{Cost, JobId, Node, NodeName, Project, ProjectName, Run, RunId, RunPurpose, RunRole, Timestamp};
use vowp_store::{NodePaths, Store, StoreError};

const SCHEMA: &str = include_str!("../src/schema/001_init.sql");
const GLOBAL_SCHEMA: &str = include_str!("../src/schema/global.sql");
const PROJECT_SCHEMA: &str = include_str!("../src/schema/project.sql");

/// Opens a file-backed store plus its temporary root.
fn file_store() -> (TempDir, NodePaths, Store) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("open store");
    (tmp, paths, store)
}

/// Returns every table name declared in the schema file.
fn schema_table_names(schema: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = schema;
    while let Some(index) = rest.find("CREATE TABLE ") {
        let after = &rest[index + "CREATE TABLE ".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            names.push(name);
        }
        rest = after;
    }
    names
}

#[test]
fn schema_file_is_identical_to_design() {
    assert_eq!(
        SCHEMA,
        include_str!("../../../docs/design/schema.sql"),
        "src/schema/001_init.sql must stay byte-identical to docs/design/schema.sql"
    );
}

#[test]
fn open_creates_schema_version_1() {
    let (_tmp, paths, store) = file_store();
    assert_eq!(store.schema_version().expect("compat version"), 1);
    assert_eq!(store.global_schema_version().expect("global version"), 2);
    assert!(store.bound_project().is_none(), "a fresh home binds no project");
    assert!(matches!(store.project_schema_version(), Err(StoreError::NoProject)));

    let tables = schema_table_names(GLOBAL_SCHEMA);
    let connection = Connection::open(paths.db_path()).expect("reopen for inspection");
    for table in tables {
        let found: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [&table],
                |row| row.get(0),
            )
            .expect("table lookup");
        assert_eq!(found, 1, "missing global table {table}");
    }
    let project_tables = schema_table_names(PROJECT_SCHEMA);
    assert_eq!(project_tables.len(), 10, "the project schema declares ten tables");

    let project = ProjectName::parse("proj").expect("project");
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: "/tmp/proj".to_owned(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: Timestamp::parse("2026-01-01T00:00:00.000Z").expect("time"),
            updated_at: Timestamp::parse("2026-01-01T00:00:00.000Z").expect("time"),
        })
        .expect("create project");
    assert_eq!(store.bound_project(), Some(&project));
    assert_eq!(store.project_schema_version().expect("project version"), 1);
    let project_conn = Connection::open(paths.project_db_path(&project)).expect("project db");
    let version: i64 = project_conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("project user_version");
    assert_eq!(version, 1);

    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal");

    let unbound = Store::open_global(&paths).expect("global handle");
    let run = sample_run(JobId::parse("no-such-job").expect("job id"));
    let error = unbound.insert_run(&run).expect_err("unbound handle");
    assert!(matches!(error, StoreError::NoProject), "got {error:?}");
}

#[test]
fn open_refuses_newer_schema() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vowp.db");
    {
        let connection = Connection::open(&path).expect("create db");
        connection
            .pragma_update(None, "user_version", 99)
            .expect("set user_version");
    }
    let before = std::fs::read(&path).expect("read before");

    let error = match Store::open_path(&path) {
        Ok(_) => panic!("a newer schema must be refused"),
        Err(error) => error,
    };
    match error {
        StoreError::SchemaTooNew { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, 2);
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }

    let after = std::fs::read(&path).expect("read after");
    assert_eq!(before, after, "the database file must not be touched");
}

#[test]
fn open_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let first = Store::open(&paths).expect("first open");
    let node_id_first = stored_node_id(&paths);
    let second = Store::open(&paths).expect("second open");
    let node_id_second = stored_node_id(&paths);

    assert_eq!(first.schema_version().expect("version"), 1);
    assert_eq!(second.schema_version().expect("version"), 1);
    assert_eq!(node_id_first, node_id_second);
    assert!(!node_id_first.is_empty());
}

#[test]
fn nodes_and_projects_roundtrip() {
    let (_tmp, _paths, store) = file_store();
    let t0 = Timestamp::parse("2026-01-01T00:00:00.000Z").expect("timestamp");
    let t1 = Timestamp::parse("2026-01-02T00:00:00.000Z").expect("timestamp");

    let local = NodeName::parse("local").expect("node");
    let remote = NodeName::parse("vps").expect("node");
    store
        .upsert_node(&Node {
            name: local.clone(),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({"tmux": true}),
            last_seen_at: None,
            created_at: t0,
        })
        .expect("insert local node");
    store
        .upsert_node(&Node {
            name: remote.clone(),
            ssh_target: Some("user@host".to_owned()),
            is_local: false,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: t0,
        })
        .expect("insert remote node");

    assert_eq!(
        store.local_node().expect("local node").map(|node| node.name),
        Some(local.clone())
    );
    assert_eq!(
        store
            .list_nodes()
            .expect("nodes")
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>(),
        vec![local.clone(), remote.clone()]
    );
    store.touch_node_seen(&remote, t1).expect("touch node");
    assert_eq!(store.require_node(&remote).expect("remote node").last_seen_at, Some(t1));
    let error = store.remove_node(&local).expect_err("local node is protected");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
    store.remove_node(&remote).expect("remove remote node");
    assert!(store.node(&remote).expect("lookup").is_none());
    let error = store.touch_node_seen(&remote, t1).expect_err("missing node");
    assert!(matches!(error, StoreError::NotFound { .. }), "got {error:?}");

    let one = ProjectName::parse("one").expect("project");
    let two = ProjectName::parse("two").expect("project");
    for (name, path) in [(&one, "/tmp/one"), (&two, "/tmp/two")] {
        store
            .upsert_project(&Project {
                name: name.clone(),
                path: path.to_owned(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({"project": {"name": name}}),
                created_at: t0,
                updated_at: t0,
            })
            .expect("insert project");
    }
    assert_eq!(
        store
            .list_projects()
            .expect("projects")
            .iter()
            .map(|project| project.name.clone())
            .collect::<Vec<_>>(),
        vec![one.clone(), two.clone()]
    );
    store
        .upsert_project(&Project {
            name: one.clone(),
            path: "/tmp/one-moved".to_owned(),
            remote_url: Some("git@example/one".to_owned()),
            default_branch: "trunk".to_owned(),
            config: serde_json::json!({}),
            created_at: t0,
            updated_at: t1,
        })
        .expect("update project");
    let updated = store.require_project(&one).expect("updated project");
    assert_eq!(updated.path, "/tmp/one-moved");
    assert_eq!(updated.default_branch, "trunk");
    assert_eq!(updated.created_at, t0, "creation time is preserved");
    assert_eq!(updated.updated_at, t1);
    let error = store
        .require_project(&ProjectName::parse("missing").expect("project"))
        .expect_err("missing project");
    assert!(matches!(error, StoreError::NotFound { .. }), "got {error:?}");
}

#[test]
fn backup_copies_rows_to_an_openable_database() {
    let (tmp, paths, store) = file_store();
    let destination = tmp.path().join("copy.db");
    store.backup(&destination).expect("backup");
    assert!(destination.is_file());

    let copy = Store::open_path(&destination).expect("open backup");
    assert_eq!(copy.schema_version().expect("version"), 1);
    assert_eq!(stored_node_id(&paths), {
        let connection = Connection::open(&destination).expect("read backup kv");
        connection
            .query_row("SELECT value FROM kv WHERE key = 'node_id'", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("backup node_id")
    });
    copy.integrity_check().expect("backup integrity");
}

/// Reads the `node_id` kv row through a separate connection.
fn stored_node_id(paths: &NodePaths) -> String {
    let connection = Connection::open(paths.db_path()).expect("open kv reader");
    connection
        .query_row("SELECT value FROM kv WHERE key = 'node_id'", [], |row| row.get(0))
        .expect("node_id row")
}

#[test]
fn paths_layout() {
    if let Ok(case) = std::env::var("VOWP_TEST_PATHS_CASE") {
        match case.as_str() {
            "vowp_home" => {
                let paths = NodePaths::from_env().expect("VOWP_HOME resolves");
                assert_eq!(paths.root(), Path::new("/x"));
            }
            "home" => {
                let paths = NodePaths::from_env().expect("HOME resolves");
                assert_eq!(paths.root(), Path::new("/h/.vowp"));
            }
            "none" => {
                let error = NodePaths::from_env().expect_err("neither variable is set");
                assert!(matches!(error, StoreError::NoHome), "got {error:?}");
            }
            other => panic!("unknown child case {other}"),
        }
        return;
    }

    let root = Path::new("/x");
    let paths = NodePaths::new(root);
    let session = vowp_protocol::SessionId::new();
    let run = RunId::new();

    assert_eq!(paths.root(), root);
    assert_eq!(paths.db_path(), Path::new("/x/vowp.db"));
    assert_eq!(paths.sessions_dir(), Path::new("/x/sessions"));
    let session_dir = Path::new("/x/sessions").join(session.as_str());
    assert_eq!(paths.session_dir(&session), session_dir);
    assert_eq!(paths.log_path(&session), session_dir.join("log.jsonl"));
    assert_eq!(paths.lock_path(&session), session_dir.join(".lock"));
    assert_eq!(paths.blobs_dir(&session), session_dir.join("blobs"));
    assert_eq!(paths.capsules_dir(&session), session_dir.join("capsules"));
    assert_eq!(paths.adapters_json_path(&session), session_dir.join("adapters.json"));
    assert_eq!(paths.logs_dir(), Path::new("/x/logs"));
    assert_eq!(paths.runs_dir(), Path::new("/x/runs"));
    assert_eq!(
        paths.run_raw_path(&run),
        Path::new("/x/runs").join(format!("{run}.jsonl"))
    );
    assert_eq!(paths.backups_dir(), Path::new("/x/backups"));
    assert_eq!(paths.tmp_dir(), Path::new("/x/tmp"));

    let tmp = tempfile::tempdir().expect("tempdir");
    let real = NodePaths::new(tmp.path());
    real.ensure().expect("first ensure");
    real.ensure().expect("second ensure is idempotent");
    for dir in [
        real.root().to_path_buf(),
        real.sessions_dir(),
        real.logs_dir(),
        real.runs_dir(),
        real.backups_dir(),
        real.tmp_dir(),
    ] {
        assert!(dir.is_dir(), "{} was not created", dir.display());
    }

    // from_env reads process environment variables, so probe it in child processes to keep
    // the test hermetic and avoid unsafe environment mutation in this thread.
    for (case, set, remove) in [
        ("vowp_home", vec![("VOWP_HOME", "/x"), ("HOME", "/ignored")], vec![]),
        ("home", vec![("VOWP_HOME", ""), ("HOME", "/h")], vec![]),
        ("none", vec![], vec!["VOWP_HOME", "HOME"]),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .arg("--exact")
            .arg("paths_layout")
            .arg("--nocapture")
            .env("VOWP_TEST_PATHS_CASE", case);
        for (key, value) in set {
            command.env(key, value);
        }
        for key in remove {
            command.env_remove(key);
        }
        let output = command.output().expect("spawn child test");
        assert!(
            output.status.success(),
            "child case {case} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Builds a minimal run, used to prove foreign-key enforcement.
fn sample_run(job: JobId) -> Run {
    Run {
        id: RunId::new(),
        job_id: Some(job),
        session_id: None,
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
        started_at: Timestamp::parse("2026-01-01T00:00:00.000Z").expect("timestamp"),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    }
}
