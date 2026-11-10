//! `vowp status` integration tests.

mod common;

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use vowp_protocol::Timestamp;

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
