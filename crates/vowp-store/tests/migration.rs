//! v0.1 → v0.2 migration tests.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.9 and §15.

use std::path::Path;

use rusqlite::{Connection, params};
use tempfile::TempDir;
use vowp_protocol::{JobId, ProjectName, RunId, SessionId, Timestamp};
use vowp_store::{NodePaths, Store, StoreError, migrate_rollback, migrate_v01, migrate_v01_dry_run, resume_migration};

const V01_SCHEMA: &str = include_str!("../../../docs/design/schema.sql");

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";

fn project(name: &str) -> ProjectName {
    ProjectName::parse(name).expect("project")
}

fn insert_project(conn: &Connection, name: &str) {
    conn.execute(
        "INSERT INTO projects (name, path, remote_url, default_branch, config_json, created_at, updated_at)
         VALUES (?1, ?2, NULL, 'main', '{}', ?3, ?3)",
        params![name, format!("/tmp/{name}"), T0],
    )
    .expect("insert project");
}

fn insert_job(conn: &Connection, id: &str, project: &str) {
    conn.execute(
        "INSERT INTO jobs (
            id, project, title, kind, role, profile, brief_path, report_path,
            state_changed_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, 'build', 'worker', 'worker', 'brief', 'report', ?4, ?4, ?4)",
        params![id, project, format!("job {id}"), T0],
    )
    .expect("insert job");
}

fn insert_session(conn: &Connection, id: &SessionId, project: Option<&str>, job: Option<&str>) {
    conn.execute(
        "INSERT INTO sessions (id, project, job_id, role, title, node, cwd, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'user', 'session', 'local', '/tmp', ?4, ?4)",
        params![id.as_str(), project, job, T0],
    )
    .expect("insert session");
}

/// Builds a realistic v0.1 single-file home.
fn v01_home() -> (TempDir, NodePaths, SessionId, RunId, RunId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    paths.ensure().expect("dirs");
    let conn = Connection::open(paths.db_path()).expect("v0.1 db");
    conn.pragma_update(None, "journal_mode", "WAL").expect("wal");
    conn.execute_batch(V01_SCHEMA).expect("v0.1 schema");

    insert_project(&conn, "alpha");
    insert_project(&conn, "beta");
    insert_job(&conn, "alpha-a", "alpha");
    insert_job(&conn, "alpha-b", "alpha");
    insert_job(&conn, "beta-job", "beta");
    conn.execute(
        "INSERT INTO edges (src, dst, kind, created_at) VALUES ('alpha-a', 'alpha-b', 'needs', ?1)",
        [T0],
    )
    .expect("edge");

    let alpha_session = SessionId::new();
    let node_session = SessionId::new();
    insert_session(&conn, &alpha_session, Some("alpha"), Some("alpha-a"));
    insert_session(&conn, &node_session, None, None);

    let alpha_run = RunId::new();
    let planner_run = RunId::new();
    let beta_run = RunId::new();
    for (id, job, session, started) in [
        (&alpha_run, Some("alpha-a"), Some(alpha_session.as_str()), T0),
        (&planner_run, None, Some(node_session.as_str()), T1),
        (&beta_run, Some("beta-job"), None, T1),
    ] {
        conn.execute(
            "INSERT INTO runs (id, job_id, session_id, role, harness, node, started_at)
             VALUES (?1, ?2, ?3, 'worker', 'vowp', 'local', ?4)",
            params![id.as_str(), job, session, started],
        )
        .expect("run");
    }

    for (slot, kind) in [(1, "worker"), (2, "worker"), (3, "worker"), (4, "worker")] {
        let (job, run) = match slot {
            1 => (Some("alpha-a"), Some(alpha_run.as_str())),
            2 => (Some("beta-job"), Some(beta_run.as_str())),
            3 => (None, Some(planner_run.as_str())),
            _ => (None, None),
        };
        let state = if run.is_some() { "busy" } else { "idle" };
        conn.execute(
            "INSERT INTO workers (node, slot, kind, state, run_id, job_id, updated_at)
             VALUES ('local', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![slot, kind, state, run, job, T0],
        )
        .expect("worker");
    }

    conn.execute(
        "INSERT INTO gate_results (job_id, run_id, attempt, gate_index, gate_kind, gate_json,
                                   passed, evidence_json, ts)
         VALUES ('alpha-a', ?1, 0, 0, 'report', '{}', 1, '{}', ?2)",
        params![alpha_run.as_str(), T0],
    )
    .expect("gate result");

    conn.execute(
        "INSERT INTO loops (name, kind, project, interval_secs, updated_at)
         VALUES ('l4-node', 'L4', NULL, 60, ?1), ('l4-alpha', 'L4', 'alpha', 60, ?1)",
        [T0],
    )
    .expect("loops");
    conn.execute(
        "INSERT INTO schedules (id, project, name, spec_kind, spec, action_kind, action_json, created_at)
         VALUES (?1, NULL, 'nightly', 'interval', 'every 1h', 'loop', '{\"kind\":\"loop\",\"loop_name\":\"l4-node\"}', ?2)",
        params![vowp_protocol::ScheduleId::new().as_str(), T0],
    )
    .expect("schedule");

    for (project, job, title) in [
        (None, Some("alpha-a"), "node job notice"),
        (None, None, "node notice"),
        (Some("alpha"), Some("alpha-a"), "alpha question"),
        (Some("beta"), None, "beta question"),
    ] {
        conn.execute(
            "INSERT INTO inbox (id, project, job_id, kind, title, created_at)
             VALUES (?1, ?2, ?3, 'question', ?4, ?5)",
            params![vowp_protocol::InboxId::new().as_str(), project, job, title, T0],
        )
        .expect("inbox");
    }
    for (project, summary, run) in [
        (None, "node cost", Some(planner_run.as_str())),
        (Some("alpha"), "alpha cost", Some(alpha_run.as_str())),
        (Some("beta"), "beta cost", Some(beta_run.as_str())),
    ] {
        conn.execute(
            "INSERT INTO ledger (ts, kind, project, run_id, summary, usd, cost_source)
             VALUES (?1, 'cost', ?2, ?3, ?4, 1.0, 'receipt')",
            params![T0, project, run, summary],
        )
        .expect("ledger");
    }
    conn.execute(
        "INSERT INTO releases (id, project, env, change_ref, created_at)
         VALUES (?1, 'alpha', 'production', 'v1', ?2)",
        params![vowp_protocol::ReleaseId::new().as_str(), T0],
    )
    .expect("release");

    (tmp, paths, alpha_session, alpha_run, beta_run)
}

#[test]
fn v01_database_migrates_with_matching_row_counts() {
    let (_tmp, paths, alpha_session, alpha_run, beta_run) = v01_home();
    let report = migrate_v01(&paths).expect("migrate");
    assert!(report.verified);
    assert!(!report.dry_run);
    assert_eq!(report.projects, vec![project("alpha"), project("beta")]);
    assert!(Path::new(&report.backup).is_file(), "pre-migration backup exists");

    for (key, count) in &report.tables {
        if key == "global:projects" {
            assert_eq!(count.after, count.before + 1, "{key}");
        } else if key == "workers_dropped" {
            assert_eq!(count.before, count.after, "{key}");
        } else {
            assert_eq!(count.before, count.after, "row count drift for {key}");
        }
    }
    assert_eq!(report.tables["project:alpha:jobs"].after, 2);
    assert_eq!(report.tables["project:beta:jobs"].after, 1);
    assert_eq!(report.tables["global:node_inbox"].after, 2);
    assert_eq!(report.tables["global:node_ledger"].after, 1);

    let global = Store::open_global(&paths).expect("global");
    assert_eq!(global.global_schema_version().expect("version"), 2);
    assert_eq!(
        global.list_projects().expect("projects").len(),
        3,
        "_node is registered"
    );
    drop(global);

    let alpha = Store::open_with_project(&paths, &project("alpha")).expect("alpha");
    assert_eq!(alpha.project_schema_version().expect("version"), 1);
    assert!(alpha.require_job(&JobId::parse("alpha-a").expect("job")).is_ok());
    assert!(alpha.require_run(&alpha_run).is_ok());
    assert!(alpha.require_session(&alpha_session).is_ok());

    let beta = Store::open_with_project(&paths, &project("beta")).expect("beta");
    assert!(beta.require_job(&JobId::parse("beta-job").expect("job")).is_ok());
    assert!(beta.require_run(&beta_run).is_ok());
    assert!(paths.project_db_path(&project("_node")).is_file());
}

#[test]
fn migration_node_rows_go_to_primary() {
    let (_tmp, paths, _session, _alpha_run, _beta_run) = v01_home();
    let report = migrate_v01(&paths).expect("migrate");
    assert!(report.warnings.iter().any(|warning| warning.contains("worker")));
    let alpha = Store::open_with_project(&paths, &project("alpha")).expect("alpha");
    let runs = alpha.open_runs().expect("open runs");
    assert_eq!(runs.len(), 2);
    let all_runs: i64 = {
        let conn = Connection::open(paths.project_db_path(&project("alpha"))).expect("alpha db");
        conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .expect("runs")
    };
    assert_eq!(all_runs, 2, "alpha run plus the job-less planner run");
    let loops = alpha.list_loops().expect("loops");
    assert!(loops.iter().any(|loop_| loop_.name == "l4-node"));
    assert!(loops.iter().any(|loop_| loop_.name == "l4-alpha"));
    let schedules = alpha.list_schedules().expect("schedules");
    assert_eq!(schedules.len(), 1);
    let _beta = Store::open_with_project(&paths, &project("beta")).expect("beta");
    let beta_runs: i64 = {
        let conn = Connection::open(paths.project_db_path(&project("beta"))).expect("beta db");
        conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .expect("runs")
    };
    assert_eq!(beta_runs, 1);
}

#[test]
fn migration_workers_follow_their_job_project() {
    let (_tmp, paths, _session, _alpha_run, _beta_run) = v01_home();
    let report = migrate_v01(&paths).expect("migrate");
    assert_eq!(report.dropped_workers, 1);
    let alpha = Store::open_with_project(&paths, &project("alpha")).expect("alpha");
    let beta = Store::open_with_project(&paths, &project("beta")).expect("beta");
    let node = vowp_protocol::NodeName::parse("local").expect("node");
    let alpha_workers = alpha.list_workers(&node).expect("alpha workers");
    let beta_workers = beta.list_workers(&node).expect("beta workers");
    assert_eq!(alpha_workers.len(), 2, "alpha worker plus the idle primary slot");
    assert_eq!(beta_workers.len(), 1);
    assert!(
        beta_workers
            .iter()
            .any(|worker| worker.job_id.as_ref().map(JobId::as_str) == Some("beta-job"))
    );
    assert_eq!(
        report.tables["workers_dropped"].before,
        u64::from(report.dropped_workers)
    );
}

#[test]
fn migration_unassignable_workers_dropped_and_reported() {
    let (_tmp, paths, _session, _alpha_run, _beta_run) = v01_home();
    let report = migrate_v01(&paths).expect("migrate");
    assert_eq!(report.dropped_workers, 1);
    assert!(
        report.warnings.iter().any(|warning| warning.contains("unassignable")),
        "the report names the dropped worker"
    );
}

#[test]
fn migration_multi_project_split() {
    let (_tmp, paths, ..) = v01_home();
    migrate_v01(&paths).expect("migrate");
    assert!(paths.project_db_path(&project("alpha")).is_file());
    assert!(paths.project_db_path(&project("beta")).is_file());
    let global = Store::open_global(&paths).expect("global");
    let projects = global.list_projects().expect("projects");
    assert!(projects.iter().any(|row| row.name == project("alpha")));
    assert!(projects.iter().any(|row| row.name == project("beta")));
    assert!(projects.iter().any(|row| row.name == project("_node")));
    let beta_rows: i64 = {
        let conn = Connection::open(paths.project_db_path(&project("beta"))).expect("beta db");
        conn.query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .expect("jobs")
    };
    assert_eq!(beta_rows, 1);
}

#[test]
fn migration_refuses_while_supervisor_runs() {
    let (_tmp, paths, ..) = v01_home();
    std::fs::write(
        paths.supervisor_lock_path(),
        format!("{}\n{}\n", std::process::id(), T0),
    )
    .expect("lock");
    let error = migrate_v01(&paths).expect_err("supervisor lock");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
}

#[test]
fn migration_is_restartable_at_each_step() {
    let (_tmp, paths, ..) = v01_home();
    let stage = paths.tmp_dir().join("migrate-test-stage");
    std::fs::create_dir_all(&stage).expect("stage");
    let manifest = serde_json::json!({
        "from": 1,
        "to": 2,
        "backup": paths.backups_dir().join("pre-0.2-test.db").display().to_string(),
        "stage": stage.display().to_string(),
        "state": "copying",
    });
    std::fs::write(
        paths.migration_manifest_path(),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");
    resume_migration(&paths).expect("resume copying");
    assert!(!paths.migration_manifest_path().exists());
    assert!(!stage.exists());
    // A second resume is a no-op, and the original home is still v1 and migratable.
    resume_migration(&paths).expect("no manifest");
    let report = migrate_v01(&paths).expect("migrate after resume");
    assert!(report.verified);
}

#[test]
fn migration_rollback_restores_v01_home() {
    let (_tmp, paths, ..) = v01_home();
    let report = migrate_v01(&paths).expect("migrate");
    let backup = std::path::PathBuf::from(&report.backup);
    migrate_rollback(&paths, &backup).expect("rollback");
    let conn = Connection::open(paths.db_path()).expect("rolled back db");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 1);
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .expect("jobs");
    assert_eq!(jobs, 3);
    assert!(!paths.projects_dir().exists());
    assert!(!paths.migration_manifest_path().exists());
}

#[test]
fn migration_dry_run_writes_nothing() {
    let (_tmp, paths, ..) = v01_home();
    let before = std::fs::read(paths.db_path()).expect("before");
    let report = migrate_v01_dry_run(&paths).expect("dry run");
    assert!(report.dry_run);
    assert_eq!(report.from_version, 1);
    assert_eq!(report.to_version, 2);
    assert_eq!(report.projects.len(), 2);
    assert_eq!(report.tables["project:alpha:jobs"].before, 2);
    let after = std::fs::read(paths.db_path()).expect("after");
    assert_eq!(before, after);
    assert!(!paths.projects_dir().exists());
    assert!(!paths.migration_manifest_path().exists());
}

#[test]
fn migration_resume_finishes_a_swap() {
    let (_tmp, paths, ..) = v01_home();
    let stage = paths.tmp_dir().join("migrate-swap-stage");
    let projects = stage.join("projects");
    std::fs::create_dir_all(&projects).expect("stage projects");
    let global = Connection::open(stage.join("global.db")).expect("stage global");
    global
        .execute_batch(include_str!("../src/schema/global.sql"))
        .expect("global schema");
    drop(global);
    for name in ["alpha", "beta", "_node"] {
        let dir = projects.join(name);
        std::fs::create_dir_all(&dir).expect("project dir");
        let conn = Connection::open(dir.join("project.db")).expect("stage project");
        conn.execute_batch(include_str!("../src/schema/project.sql"))
            .expect("project schema");
    }
    let backup = paths.backups_dir().join("pre-0.2-test.db");
    let manifest = serde_json::json!({
        "from": 1,
        "to": 2,
        "backup": backup.display().to_string(),
        "stage": stage.display().to_string(),
        "state": "swapping",
    });
    std::fs::write(
        paths.migration_manifest_path(),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");

    resume_migration(&paths).expect("resume swapping");
    assert!(!paths.migration_manifest_path().exists());
    assert!(backup.is_file(), "the v0.1 file is kept as the backup");
    let conn = Connection::open(paths.db_path()).expect("swapped global");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 2);
    assert!(paths.project_db_path(&project("alpha")).is_file());
    assert!(paths.project_db_path(&project("_node")).is_file());
}

#[test]
fn migration_resume_finishes_swap_with_stale_projects() {
    let (_tmp, paths, ..) = v01_home();
    let backup = paths.backups_dir().join("pre-0.2-stale.db");
    std::fs::copy(paths.db_path(), &backup).expect("back up v0.1 file");

    let stage = paths.tmp_dir().join("migrate-stale-stage");
    let staged_projects = stage.join("projects");
    std::fs::create_dir_all(&staged_projects).expect("stage projects");
    let global = Connection::open(stage.join("global.db")).expect("stage global");
    global
        .execute_batch(include_str!("../src/schema/global.sql"))
        .expect("global schema");
    drop(global);

    let write_project = |path: &Path, job: &str| {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("project dir");
        let conn = Connection::open(path).expect("project db");
        conn.execute_batch(include_str!("../src/schema/project.sql"))
            .expect("project schema");
        conn.execute(
            "INSERT INTO jobs (
                id, project, title, kind, role, profile, brief_path, report_path,
                state_changed_at, created_at, updated_at
             ) VALUES (?1, 'alpha', ?1, 'build', 'worker', 'worker', 'b', 'r', ?2, ?2, ?2)",
            params![job, T0],
        )
        .expect("job");
    };
    write_project(&staged_projects.join("alpha/project.db"), "staged-job");
    write_project(&staged_projects.join("beta/project.db"), "beta-job");
    write_project(&staged_projects.join("_node/project.db"), "node-job");

    // A stale projects/ directory exists from an earlier attempt.
    let stale = paths.projects_dir();
    write_project(&stale.join("alpha/project.db"), "stale-job");

    // Simulate a crash after step 8.2: the global file is already v2 but projects/ is old.
    std::fs::copy(stage.join("global.db"), paths.db_path()).expect("install v2 global");
    let manifest = serde_json::json!({
        "from": 1,
        "to": 2,
        "backup": backup.display().to_string(),
        "stage": stage.display().to_string(),
        "state": "swapping",
    });
    std::fs::write(
        paths.migration_manifest_path(),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");

    resume_migration(&paths).expect("resume with stale projects");
    assert!(!paths.migration_manifest_path().exists());
    assert!(
        !stage.exists(),
        "the stage is removed only after projects are installed"
    );
    let installed = Connection::open(paths.project_db_path(&project("alpha"))).expect("installed");
    let staged: i64 = installed
        .query_row("SELECT COUNT(*) FROM jobs WHERE id = 'staged-job'", [], |row| {
            row.get(0)
        })
        .expect("staged job");
    let stale: i64 = installed
        .query_row("SELECT COUNT(*) FROM jobs WHERE id = 'stale-job'", [], |row| row.get(0))
        .expect("stale job");
    assert_eq!(staged, 1, "the staged project database was installed");
    assert_eq!(stale, 0, "the stale directory did not win");

    let aside = std::fs::read_dir(paths.backups_dir())
        .expect("backups")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("projects-before-0.2-"))
        })
        .expect("stale projects moved aside");
    let old = Connection::open(aside.join("alpha/project.db")).expect("old project");
    let old_stale: i64 = old
        .query_row("SELECT COUNT(*) FROM jobs WHERE id = 'stale-job'", [], |row| row.get(0))
        .expect("old job");
    assert_eq!(old_stale, 1, "the old directory is preserved in backups");
}

#[test]
fn v01_database_refuses_to_open_without_migration() {
    let (_tmp, paths, ..) = v01_home();
    let error = match Store::open(&paths) {
        Ok(_) => panic!("v0.1 must be migrated first"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::MigrationRequired), "got {error:?}");
    assert_eq!(
        Timestamp::parse(T0).expect("time").as_rfc3339(),
        "2026-01-01T00:00:00.000Z"
    );
}
