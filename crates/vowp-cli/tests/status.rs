//! `vowp status` integration tests.

mod common;

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use vowp_protocol::{
    InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, JobState, NewJob, NodeName, Project, ProjectName, Role,
    Session, SessionId, SessionRole, SessionState, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, StateChange, Store};

/// Writes a heartbeat `age_secs` seconds in the past.
fn write_heartbeat(env: &common::TestEnv, age_secs: i64) {
    let now = Timestamp::now().unix_secs();
    std::fs::write(env.home_path().join("heartbeat"), (now - age_secs).to_string()).expect("heartbeat");
}

#[test]
fn status_check_exit_1_on_stale_heartbeat() {
    let env = common::TestEnv::new();
    write_heartbeat(&env, 600);

    let (code, stdout, stderr) = env.run(&["status", "--check"]);
    assert_eq!(code, 1, "a stale heartbeat must exit 1; stderr: {stderr}");
    assert!(stdout.contains("supervisor: not running"), "{stdout}");
    assert!(stdout.contains("slots: 0/6"), "{stdout}");

    let (code, value, stderr) = env.run_json(&["status", "--check"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(value["supervisor"]["running"], false);
    assert_eq!(value["supervisor"]["pid"], serde_json::Value::Null);
    assert_eq!(value["supervisor"]["paused"], false);
    let age = value["supervisor"]["heartbeat_age_secs"]
        .as_i64()
        .expect("heartbeat age");
    assert!(age >= 600, "age {age} must include the written 600s");
    assert!(value["supervisor"]["last_tick"].as_str().is_some());
    assert_eq!(value["jobs"], serde_json::json!({}));
    assert_eq!(value["slots"]["busy"], 0);
    assert_eq!(value["slots"]["total"], 6);
    assert_eq!(value["limits"], serde_json::json!([]));
    assert_eq!(value["inbox_open"], 0);
    assert!(value["cost_today_usd"].is_null(), "unknown cost is null, not zero");

    write_heartbeat(&env, 0);
    let (code, value, stderr) = env.run_json(&["status", "--check"]);
    assert_eq!(code, 0, "a fresh heartbeat must pass; stderr: {stderr}");
    let age = value["supervisor"]["heartbeat_age_secs"]
        .as_i64()
        .expect("heartbeat age");
    assert!((0..=5).contains(&age), "fresh age {age}");
}

/// A lock naming a dead pid must not make `status` claim a running supervisor.
#[test]
fn status_reports_stopped_for_stale_lock() {
    let env = common::TestEnv::new();
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().expect("spawn child");
    let dead_pid = child.id();
    assert!(child.wait().expect("reap child").success(), "child exits 0");
    std::fs::write(
        env.home_path().join("supervisor.lock"),
        format!("{dead_pid}\n{}\n", Timestamp::now().as_rfc3339()),
    )
    .expect("lock");

    let (code, stdout, stderr) = env.run(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("supervisor: not running"),
        "a stale lock must not report a running supervisor:\n{stdout}"
    );

    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["supervisor"]["running"], false);
    assert_eq!(value["supervisor"]["pid"], serde_json::Value::Null);

    let (code, _, stderr) = env.run(&["status", "--check"]);
    assert_eq!(code, 1, "a stale lock has no live heartbeat; stderr: {stderr}");
}

#[test]
fn status_without_check_never_exits_1_and_reports_running_supervisor() {
    let env = common::TestEnv::new();
    let stale = Timestamp::now().unix_secs() - 900;
    std::fs::write(env.home_path().join("heartbeat"), stale.to_string()).expect("heartbeat");

    let (code, _, stderr) = env.run(&["status"]);
    assert_eq!(code, 0, "status without --check always exits 0; stderr: {stderr}");

    let pid = std::process::id();
    std::fs::write(
        env.home_path().join("supervisor.lock"),
        format!("{pid}\n{}\n", Timestamp::now().as_rfc3339()),
    )
    .expect("lock");
    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["supervisor"]["running"], true);
    assert_eq!(value["supervisor"]["pid"], pid);
    assert_eq!(value["supervisor"]["paused"], false);
}

#[test]
fn status_human_line_reports_a_paused_running_supervisor() {
    let env = common::TestEnv::new();
    env.store().kv_set("paused", "1").expect("pause marker");
    let pid = std::process::id();
    std::fs::write(
        env.home_path().join("supervisor.lock"),
        format!("{pid}\n{}\n", Timestamp::now().as_rfc3339()),
    )
    .expect("lock");

    let (code, stdout, stderr) = env.run(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let line = stdout
        .lines()
        .find(|line| line.starts_with("supervisor:"))
        .expect("supervisor line");
    assert!(line.contains("running"), "{line}");
    assert!(line.contains("paused"), "a paused running supervisor says so: {line}");

    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["supervisor"]["running"], true);
    assert_eq!(value["supervisor"]["paused"], true);
    assert_eq!(value["supervisor"]["pid"], pid);
}

/// Polls `predicate` for up to five seconds.
fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// One spawned `vowp up --foreground`, killed if the test panics.
struct Foreground {
    child: Child,
    stderr: std::path::PathBuf,
}

impl Foreground {
    /// Spawns the real binary against one test environment.
    fn spawn(env: &common::TestEnv) -> Self {
        let stderr = env.home_path().join("spawned-supervisor.stderr");
        let file = std::fs::File::create(&stderr).expect("stderr file");
        let child = env
            .command(env!("CARGO_BIN_EXE_vowp"))
            .arg("--home")
            .arg(env.home_path())
            .arg("--project")
            .arg(env.project_path())
            .arg("up")
            .arg("--foreground")
            .arg("--no-tmux")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .expect("spawn vowp up --foreground");
        Self { child, stderr }
    }

    /// Returns the supervisor pid.
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Polls for exit up to `timeout`.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("poll supervisor") {
                Some(status) => return Some(status),
                None if Instant::now() >= deadline => return None,
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Returns the captured stderr for failure messages.
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn status_reports_stopped_after_sigterm() {
    let env = common::TestEnv::new();
    let mut supervisor = Foreground::spawn(&env);
    assert!(
        eventually(|| env.home_path().join("heartbeat").is_file()),
        "the supervisor did not complete a first tick; stderr:\n{}",
        supervisor.stderr()
    );
    let snapshot =
        vowp_core::fleet::read_snapshot(&NodePaths::new(env.home_path())).expect("the tick wrote snapshot.json");
    assert_eq!(snapshot.projects.len(), 1, "the project card is current");

    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(supervisor.pid().to_string())
        .status()
        .expect("run kill");
    assert!(killed.success(), "SIGTERM to the supervisor failed");
    let status = supervisor
        .wait_for_exit(Duration::from_secs(15))
        .unwrap_or_else(|| panic!("SIGTERM did not stop the supervisor; stderr:\n{}", supervisor.stderr()));
    assert_eq!(
        status.code(),
        Some(0),
        "a cancelled supervisor exits cleanly; stderr:\n{}",
        supervisor.stderr()
    );

    assert!(
        !env.home_path().join("heartbeat").exists(),
        "the live heartbeat is gone after a clean stop"
    );
    let stopped = std::fs::read_to_string(env.home_path().join("heartbeat.stopped")).expect("stopped marker");
    assert_eq!(stopped.lines().count(), 1, "the marker is one line: {stopped:?}");
    assert!(
        stopped.trim().parse::<i64>().is_ok(),
        "the marker holds Unix seconds: {stopped:?}"
    );

    let (code, stdout, stderr) = env.run(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("supervisor: stopped"), "{stdout}");

    let (code, value, stderr) = env.run_json(&["status", "--check"]);
    assert_eq!(
        code, 1,
        "a stopped supervisor is stale for the watchdog; stderr: {stderr}"
    );
    assert_eq!(value["supervisor"]["running"], false);
    assert_eq!(value["supervisor"]["pid"], serde_json::Value::Null);
    assert_eq!(value["supervisor"]["heartbeat_age_secs"], serde_json::Value::Null);
    assert_eq!(value["supervisor"]["last_tick"], serde_json::Value::Null);
}

#[test]
fn status_counts_jobs_and_open_inbox_items() {
    let env = common::TestEnv::new();
    env.insert_job("n01-a", "A", vowp_protocol::JobKind::Build, &[]);
    env.insert_job("n02-b", "B", vowp_protocol::JobKind::Build, &[]);
    let store = env.store();
    let now = store.clock().now();
    store
        .insert_inbox(&vowp_protocol::InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(env.config.project.name.clone()),
            job_id: None,
            kind: vowp_protocol::InboxKind::Question,
            title: "need a decision".to_owned(),
            body: "choose".to_owned(),
            options: Vec::new(),
            state: vowp_protocol::InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now,
            answered_at: None,
        })
        .expect("inbox item");

    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["jobs"]["pending"], 2);
    assert_eq!(value["inbox_open"], 1);
}

/// Registers a second project in the same temporary home.
fn add_project(env: &common::TestEnv, name: &str) -> (TempDir, VowpConfig) {
    let dir = tempfile::tempdir().expect("second project dir");
    let mut config = env.config.clone();
    config.project.name = ProjectName::parse(name).expect("project name");
    config.project.path = Some(dir.path().display().to_string());
    std::fs::write(
        dir.path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("write vowp.toml");
    let paths = NodePaths::new(env.home_path());
    let store = Store::open(&paths).expect("store");
    let now = store.clock().now();
    store
        .upsert_project(&Project {
            name: config.project.name.clone(),
            path: dir.path().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("project row");
    (dir, config)
}

/// Inserts one pending job into a named project.
fn insert_job(env: &common::TestEnv, project: &ProjectName, id: &str) {
    let paths = NodePaths::new(env.home_path());
    let store = Store::open_with_project(&paths, project).expect("project store");
    store
        .insert_job(
            project,
            &NewJob {
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
            },
            "test",
        )
        .expect("insert job");
}

/// Moves a job in a named project to `running`.
fn set_running(env: &common::TestEnv, project: &ProjectName, id: &str) {
    let paths = NodePaths::new(env.home_path());
    let store = Store::open_with_project(&paths, project).expect("project store");
    store
        .set_job_state(
            &JobId::parse(id).expect("job id"),
            JobState::Running,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("running job");
}

/// Inserts one session row attributed to a named project.
fn insert_session(env: &common::TestEnv, project: &ProjectName, title: &str) {
    let paths = NodePaths::new(env.home_path());
    let store = Store::open_global(&paths).expect("global store");
    let now = store.clock().now();
    store
        .upsert_session(&Session {
            id: SessionId::new(),
            project: Some(project.clone()),
            job_id: None,
            role: Some(SessionRole::Worker),
            title: title.to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/srv/vowp".to_owned(),
            state: SessionState::Running,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now.minus(Duration::from_secs(30)),
            updated_at: now.minus(Duration::from_secs(5)),
        })
        .expect("session row");
}

/// Runs one command with `--home` but without `--project`, like a bare fleet invocation.
fn run_bare(env: &common::TestEnv, args: &[&str]) -> (i32, String, String) {
    let mut full = vec![
        "vowp".to_owned(),
        "--home".to_owned(),
        env.home_path().display().to_string(),
    ];
    full.extend(args.iter().map(|value| (*value).to_owned()));
    let mut io = vowp_cli::BufferIo::new("");
    let code = vowp_cli::run(full, &mut io);
    (code, io.stdout(), io.stderr())
}

/// Runs one bare command with `--json` and parses its stdout document.
fn run_bare_json(env: &common::TestEnv, args: &[&str]) -> (i32, serde_json::Value, String) {
    let mut full = vec!["--json"];
    full.extend(args.iter().copied());
    let (code, stdout, stderr) = run_bare(env, &full);
    let value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
    (code, value, stderr)
}

/// Returns the snapshot path for a test home.
fn snapshot_path(env: &common::TestEnv) -> std::path::PathBuf {
    env.home_path().join("snapshot.json")
}

#[test]
fn fleet_status_json_keeps_v01_status_keys() {
    let env = common::TestEnv::new();
    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for key in [
        "supervisor",
        "jobs",
        "slots",
        "limits",
        "cost_today_usd",
        "inbox_open",
        "home",
        "projects",
    ] {
        assert!(value.get(key).is_some(), "status JSON is missing {key}: {value}");
    }
    assert_eq!(value["home"], "local");
    assert_eq!(value["projects"][0]["name"], "test");
    assert_eq!(value["projects"][0]["state"], "active");
    assert_eq!(value["projects"][0]["running"], 0);
    assert_eq!(value["projects"][0]["ready"], 0);
    assert_eq!(value["projects"][0]["blocked"], 0);
}

#[test]
fn status_shows_every_project() {
    let env = common::TestEnv::new();
    let (dir, config) = add_project(&env, "beta");
    insert_job(&env, &config.project.name, "b01-pending");

    let (code, stdout, stderr) = env.run(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("projects:"), "{stdout}");
    assert!(stdout.contains("test"), "{stdout}");
    assert!(stdout.contains("beta"), "{stdout}");

    let (code, value, stderr) = env.run_json(&["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let projects = value["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 2, "{value}");
    assert!(projects.iter().any(|project| project["name"] == "beta"));
    drop(dir);
}

/// The convergence test both parent verifiers asked for: one home, two projects, and bare
/// `status`, `board` and `fleet ...` invocations without `--project`.
#[test]
fn two_project_home_routes_status_board_and_fleet() {
    let env = common::TestEnv::new();
    let (beta_dir, beta_config) = add_project(&env, "beta");
    let alpha = env.config.project.name.clone();
    let beta = beta_config.project.name.clone();

    insert_job(&env, &alpha, "a01-alpha-run");
    insert_job(&env, &alpha, "a02-alpha-pending");
    insert_job(&env, &beta, "b01-beta-run");
    insert_job(&env, &beta, "b02-beta-pending");
    set_running(&env, &alpha, "a01-alpha-run");
    set_running(&env, &beta, "b01-beta-run");
    insert_session(&env, &alpha, "alpha session");
    insert_session(&env, &beta, "beta session");

    let (code, value, stderr) = run_bare_json(&env, &["status"]);
    assert_eq!(code, 0, "bare status must work on a two-project home; stderr: {stderr}");
    assert_eq!(value["home"], "local");
    let projects = value["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 2, "{value}");
    assert!(projects.iter().any(|project| project["name"] == "test"));
    assert!(projects.iter().any(|project| project["name"] == "beta"));
    assert_eq!(
        value["jobs"]["running"], 2,
        "the v0.1 keys aggregate both projects: {value}"
    );
    assert_eq!(value["jobs"]["pending"], 2, "{value}");
    assert_eq!(value["slots"]["busy"], 2, "{value}");
    assert_eq!(value["slots"]["total"], 6, "{value}");
    assert_eq!(value["inbox_open"], 0, "{value}");

    let (code, stdout, stderr) = run_bare(&env, &["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for needle in ["home: local", "test", "beta", "test/a01-alpha-run", "beta/b01-beta-run"] {
        assert!(stdout.contains(needle), "status is missing {needle:?}:\n{stdout}");
    }

    let (code, stdout, stderr) = run_bare(&env, &["board", "--once"]);
    assert_eq!(code, 0, "bare board must work on a two-project home; stderr: {stderr}");
    assert!(stdout.contains("projects: 2"), "{stdout}");
    assert!(
        stdout.contains("a01-alpha-run"),
        "alpha's running job is shown:\n{stdout}"
    );
    assert!(
        stdout.contains("b01-beta-run"),
        "beta's running job is shown:\n{stdout}"
    );

    let (code, value, stderr) = run_bare_json(&env, &["fleet", "status", "--refresh"]);
    assert_eq!(code, 0, "bare fleet status --refresh must work; stderr: {stderr}");
    assert_eq!(value["source"], "local");
    assert_eq!(value["stale"], false);
    let snapshot = &value["snapshot"];
    assert_eq!(snapshot["home"], "local");
    let cards = snapshot["projects"].as_array().expect("project cards");
    assert_eq!(cards.len(), 2, "{value}");
    let alpha_card = cards
        .iter()
        .find(|card| card["name"] == "test")
        .unwrap_or_else(|| panic!("missing alpha card: {value}"));
    let beta_card = cards
        .iter()
        .find(|card| card["name"] == "beta")
        .unwrap_or_else(|| panic!("missing beta card: {value}"));
    assert_eq!(alpha_card["jobs_by_state"]["running"], 1, "{value}");
    assert_eq!(alpha_card["jobs_by_state"]["pending"], 1, "{value}");
    assert_eq!(beta_card["jobs_by_state"]["running"], 1, "{value}");
    assert_eq!(beta_card["jobs_by_state"]["pending"], 1, "{value}");
    assert_eq!(alpha_card["running"][0]["job"], "a01-alpha-run");
    assert_eq!(beta_card["running"][0]["job"], "b01-beta-run");
    assert!(
        alpha_card["running"]
            .as_array()
            .expect("running")
            .iter()
            .all(|run| run["job"] != "b01-beta-run"),
        "beta's run leaked into alpha: {value}"
    );

    let sessions = snapshot["sessions"].as_array().expect("session cards");
    let alpha_session = sessions
        .iter()
        .find(|session| session["title"] == "alpha session")
        .unwrap_or_else(|| panic!("missing alpha session: {value}"));
    let beta_session = sessions
        .iter()
        .find(|session| session["title"] == "beta session")
        .unwrap_or_else(|| panic!("missing beta session: {value}"));
    assert_eq!(alpha_session["project"], "test");
    assert_eq!(beta_session["project"], "beta");

    let (code, value, stderr) = run_bare_json(&env, &["fleet", "queue"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = value.as_array().expect("queue rows");
    assert!(
        rows.iter()
            .any(|row| row["project"] == "test" && row["job"] == "a02-alpha-pending"),
        "alpha's queued job is routed to alpha: {value}"
    );
    assert!(
        rows.iter()
            .any(|row| row["project"] == "beta" && row["job"] == "b02-beta-pending"),
        "beta's queued job is routed to beta: {value}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row["project"] == "test" && row["job"] == "b02-beta-pending"),
        "beta's queued job leaked into alpha: {value}"
    );
    assert!(snapshot_path(&env).is_file(), "--refresh wrote the snapshot");
    drop(beta_dir);
}

#[test]
fn fleet_status_reads_fresh_snapshot_or_recomputes() {
    let env = common::TestEnv::new();
    insert_job(&env, &env.config.project.name, "a01-pending");

    let (code, _, stderr) = env.run(&["fleet", "status", "--refresh"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(snapshot_path(&env).is_file(), "--refresh writes the snapshot");

    // A fresh hand-edited file is read as-is.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(snapshot_path(&env)).expect("snapshot")).expect("json");
    value["home"] = serde_json::json!("from-file");
    value["generated_at"] = serde_json::json!(Timestamp::now().as_rfc3339());
    std::fs::write(
        snapshot_path(&env),
        serde_json::to_string_pretty(&value).expect("render"),
    )
    .expect("write");

    let (code, value, stderr) = env.run_json(&["fleet", "status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["snapshot"]["home"], "from-file", "fresh file wins: {value}");
    assert_eq!(value["source"], "local");
    assert_eq!(value["stale"], false);

    // An old file is recomputed from the databases.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(snapshot_path(&env)).expect("snapshot")).expect("json");
    value["home"] = serde_json::json!("stale-file");
    value["generated_at"] = serde_json::json!(Timestamp::now().minus(Duration::from_secs(3600)).as_rfc3339());
    std::fs::write(
        snapshot_path(&env),
        serde_json::to_string_pretty(&value).expect("render"),
    )
    .expect("write");
    insert_job(&env, &env.config.project.name, "a02-pending");

    let (code, value, stderr) = env.run_json(&["fleet", "status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["snapshot"]["home"], "local", "stale file is recomputed: {value}");
    assert_eq!(value["snapshot"]["projects"][0]["jobs_by_state"]["pending"], 2);
    assert_eq!(value["stale"], false);

    // --cached never recomputes and marks the source.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(snapshot_path(&env)).expect("snapshot")).expect("json");
    value["home"] = serde_json::json!("cache-home");
    value["generated_at"] = serde_json::json!(Timestamp::now().as_rfc3339());
    std::fs::write(
        snapshot_path(&env),
        serde_json::to_string_pretty(&value).expect("render"),
    )
    .expect("write");
    let (code, value, stderr) = env.run_json(&["fleet", "status", "--cached"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["snapshot"]["home"], "cache-home");
    assert_eq!(value["source"], "cache");
}

#[test]
fn fleet_status_cached_without_a_snapshot_does_not_recompute() {
    let env = common::TestEnv::new();
    let (code, _, stderr) = env.run(&["fleet", "status", "--cached"]);
    assert_eq!(code, 3, "a missing cache is not found: {stderr}");
    assert!(!snapshot_path(&env).exists(), "--cached must not write a snapshot");
}

#[test]
fn fleet_queue_includes_waiting_reason() {
    let env = common::TestEnv::new();
    let job = env.insert_job("a01-ready", "Ready job", JobKind::Build, &[]);
    let store = env.store();
    store
        .set_job_state(
            &job.id,
            vowp_protocol::JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready");
    store
        .set_job_note(&job.id, "waiting: RAM 1.2 GB short")
        .expect("waiting note");

    let (code, value, stderr) = env.run_json(&["fleet", "queue"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = value.as_array().expect("queue rows");
    let row = rows
        .iter()
        .find(|row| row["job"] == "a01-ready")
        .unwrap_or_else(|| panic!("missing queued job: {value}"));
    assert_eq!(row["project"], "test");
    assert_eq!(row["state"], "ready");
    assert_eq!(row["waiting_reason"], "waiting: RAM 1.2 GB short");
}

#[test]
fn fleet_inbox_merges_node_and_project() {
    let env = common::TestEnv::new();
    let store = env.store();
    let now = store.clock().now();
    store
        .insert_inbox(&InboxItem {
            id: InboxId::new(),
            project: Some(env.config.project.name.clone()),
            job_id: None,
            kind: InboxKind::Question,
            title: "project question".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(10)),
            answered_at: None,
        })
        .expect("project inbox");
    store
        .insert_node_inbox(&InboxItem {
            id: InboxId::new(),
            project: None,
            job_id: None,
            kind: InboxKind::Notice,
            title: "node notice".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(20)),
            answered_at: None,
        })
        .expect("node inbox");
    store
        .insert_inbox(&InboxItem {
            id: InboxId::new(),
            project: Some(env.config.project.name.clone()),
            job_id: None,
            kind: InboxKind::Approval,
            title: "answered item".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Answered,
            answer: Some("yes".to_owned()),
            answered_by: Some("owner".to_owned()),
            created_at: now.minus(Duration::from_secs(30)),
            answered_at: Some(now.minus(Duration::from_secs(20))),
        })
        .expect("answered inbox");

    let (code, value, stderr) = env.run_json(&["fleet", "inbox"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let items = value.as_array().expect("inbox rows");
    assert_eq!(items.len(), 2, "only open items by default: {value}");
    assert_eq!(items[0]["title"], "node notice", "oldest first");
    assert_eq!(items[1]["title"], "project question");
    assert_eq!(items[0]["project"], serde_json::Value::Null);

    let (code, value, stderr) = env.run_json(&["fleet", "inbox", "--all"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value.as_array().expect("inbox rows").len(), 3);
    assert!(
        value
            .as_array()
            .expect("rows")
            .iter()
            .any(|item| item["state"] == "answered"),
        "--all includes answered items: {value}"
    );
}

#[test]
fn schema_snapshot_exports_versioned_schema() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["schema", "snapshot"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let schema: serde_json::Value = serde_json::from_str(&stdout).expect("schema is JSON");
    assert_eq!(schema["title"], "Snapshot");
    assert_eq!(schema["properties"]["schema"]["type"], "string");
    assert_eq!(schema["properties"]["version"]["type"], "integer");
    assert!(
        schema["$defs"].get("ProjectSnapshot").is_some(),
        "the export includes the project card: {schema}"
    );

    let (code, stdout, stderr) = env.run(&["schema"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.lines().any(|line| line == "Snapshot"), "{stdout}");
}
