// Tests for `vowp setup verify`.
//
// Every test builds a real temporary home with `vowp init`, real git and a fake opencode
// script. The fixture project is registered in that home, so the checks exercise the
// configured store, project database and merge queue. Nothing reads the real `~/.vowp`,
// the default tmux server, the network or a real harness.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, Once};
use std::time::Duration;

use tempfile::TempDir;
use vowp_cli::cmd::setup::SetupVerifyArgs;
use vowp_cli::setup::verify;
use vowp_cli::{BufferIo, Ctx, Global};
use vowp_protocol::{
    CapacitySnapshot, NodeName, SNAPSHOT_FORMAT, SNAPSHOT_VERSION, Session, Snapshot, SupervisorSnapshot, Timestamp,
};
use vowp_store::{NodePaths, SessionFilter, Store};

/// Returns the `vowp` binary Cargo built for this package.
fn vowp_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vowp") {
        return PathBuf::from(path);
    }
    let current = std::env::current_exe().expect("test executable");
    current
        .parent()
        .and_then(Path::parent)
        .expect("target directory")
        .join("vowp")
}

/// Builds the real `vowp` binary once so the checks run the same code a user runs.
fn ensure_vowp_binary() {
    if std::env::var_os("CARGO_BIN_EXE_vowp").is_some() {
        return;
    }
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--quiet", "--bin", "vowp"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("run cargo build --bin vowp");
        assert!(status.success(), "cargo build --bin vowp failed");
    });
}

/// Serializes the tests that build a real fixture and drive its supervisor.
///
/// Every fixture starts a real supervisor, merge queue and fake harness process. Running
/// all of them at once starves those threads on a loaded machine until the 300 s merge
/// deadline, so these tests take turns. The plain snapshot tests never start a fixture
/// and stay parallel. A poisoned lock only means an earlier test panicked; recovering
/// the guard lets the remaining tests report their own results.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the process-wide fixture lock, tolerating a panic in an earlier test.
fn fixture_lock() -> MutexGuard<'static, ()> {
    FIXTURE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One temporary home and project, built by the real CLI.
struct TestHome {
    temp: TempDir,
    home: PathBuf,
    project: PathBuf,
    tmux: PathBuf,
}

impl TestHome {
    /// Creates the git checkout and runs `vowp init` against the temporary home.
    fn new(name: &str) -> Self {
        ensure_vowp_binary();
        let temp = TempDir::new().expect("temp home");
        let home = temp.path().join("home/.vowp");
        let project = temp.path().join("project");
        let tmux = temp.path().join("tmux");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::create_dir_all(&tmux).expect("private tmux dir");
        for args in [
            vec!["init", "--quiet"],
            vec!["checkout", "--quiet", "-b", "main"],
            vec![
                "-c",
                "user.name=setup verify",
                "-c",
                "user.email=setup@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture base",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(&project)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }
        let init = Self::command_for(&temp, &home, &project, &tmux)
            .args(["init", "--name", name, "--no-framework"])
            .output()
            .expect("run vowp init");
        assert!(
            init.status.success(),
            "vowp init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        Self {
            temp,
            home,
            project,
            tmux,
        }
    }

    /// Builds a child command isolated to this home.
    fn command_for(temp: &TempDir, home: &Path, project: &Path, tmux: &Path) -> Command {
        let mut command = Command::new(vowp_binary());
        command
            .arg("--home")
            .arg(home)
            .current_dir(project)
            .env("HOME", temp.path())
            .env("TMUX_TMPDIR", tmux)
            .env("NO_COLOR", "1")
            .env("VOWP_CONTROL_HOME", temp.path().join("no-control"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("VOWP_HOME");
        command
    }

    /// Returns a child command isolated to this home.
    fn command(&self) -> Command {
        Self::command_for(&self.temp, &self.home, &self.project, &self.tmux)
    }

    /// Builds an in-process context for this home.
    fn ctx(&self) -> Ctx {
        let global = Global {
            json: true,
            home: Some(self.home.clone()),
            project: Some(self.project.display().to_string()),
            ..Global::default()
        };
        let io = BufferIo::new("");
        Ctx::build(&global, &io).expect("build home context")
    }
}

/// Returns a minimal but valid home snapshot.
fn snapshot(home: &str) -> Snapshot {
    let now = Timestamp::now();
    Snapshot {
        schema: SNAPSHOT_FORMAT.to_owned(),
        version: SNAPSHOT_VERSION,
        sequence: 1,
        generated_at: now,
        home: NodeName::parse(home).expect("home name"),
        supervisor: SupervisorSnapshot {
            running: true,
            last_tick: Some(now),
            tick_secs: 60,
            ..SupervisorSnapshot::default()
        },
        projects: Vec::new(),
        capacity: CapacitySnapshot::default(),
        limits: Vec::new(),
        inbox: Vec::new(),
        sessions: Vec::new(),
        orchestrators: Vec::new(),
    }
}

/// Writes the control config and snapshot cache `setup control --apply` leaves behind.
fn write_control_home(root: &Path, home: &str, fetched_at: Timestamp) {
    std::fs::create_dir_all(root).expect("control dir");
    let config = serde_json::json!({
        "role": "control",
        "home": home,
        "ssh_target": "fake.example",
        "remote_home": "/home/vowp/.vowp",
        "created_at": fetched_at,
    });
    std::fs::write(
        root.join("control.json"),
        serde_json::to_vec_pretty(&config).expect("control json"),
    )
    .expect("control.json");
    std::fs::write(
        root.join("snapshot.json"),
        serde_json::to_vec_pretty(&snapshot(home)).expect("snapshot json"),
    )
    .expect("snapshot.json");
    let meta = serde_json::json!({"fetched_at": fetched_at, "source": "remote"});
    std::fs::write(
        root.join("snapshot.meta.json"),
        serde_json::to_vec_pretty(&meta).expect("meta json"),
    )
    .expect("snapshot.meta.json");
}

/// Writes an executable script.
fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
}

/// Returns the root fixture session rows left in the home's global session index.
///
/// The store has no delete-session API, so a run retains one index row. The fixture uses
/// a run-unique job id; these rows let the tests prove that no later run reuses them.
/// A loaded machine can make the fixture spend one fix round, whose child session has its
/// own row; only the root run's row is counted here.
fn fixture_sessions(home: &Path) -> Vec<Session> {
    Store::open_global(&NodePaths::new(home))
        .expect("open home")
        .list_sessions(&SessionFilter::default())
        .expect("list sessions")
        .into_iter()
        .filter(|session| {
            session.job_id.as_ref().is_some_and(|job| {
                let id = job.as_str();
                id.starts_with("setup-verify-job-") && !id.contains("-fix-") && !id.contains("-merge-")
            })
        })
        .collect()
}

#[test]
fn setup_verify_fixture_job_merges() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-fixture");
    let ctx = env.ctx();
    let check = verify::fixture_job_merges(&ctx).expect("fixture job check");
    assert_eq!(check.status, "ok", "{}", check.detail);
    assert_eq!(check.name, "fixture.job_merge");
    assert!(
        check.detail.contains("merged into main"),
        "the check must prove the merge, not only a done state: {}",
        check.detail
    );
    let projects = Store::open_global(&NodePaths::new(&env.home))
        .expect("open home")
        .list_projects()
        .expect("list projects");
    assert!(
        !projects
            .iter()
            .any(|project| project.name.as_str().starts_with("setup-verify-job-")),
        "fixture project was left registered: {projects:?}"
    );
}

#[test]
fn setup_verify_fixture_runs_twice_on_one_home() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-fixture-twice");
    let ctx = env.ctx();

    let first = verify::fixture_job_merges(&ctx).expect("first fixture check");
    assert_eq!(first.status, "ok", "{}", first.detail);
    let after_first = fixture_sessions(&env.home);
    assert_eq!(
        after_first.len(),
        1,
        "expected exactly one first-run session row: {after_first:?}"
    );
    let first_job = after_first[0].job_id.clone().expect("fixture job id");
    let paths = NodePaths::new(&env.home);
    assert!(
        !paths.session_dir(&after_first[0].id).exists(),
        "the first fixture's session directory must be removed: {:?}",
        paths.session_dir(&after_first[0].id)
    );

    let second = verify::fixture_job_merges(&ctx).expect("second fixture check");
    assert_eq!(second.status, "ok", "{}", second.detail);
    let after_second = fixture_sessions(&env.home);
    assert_eq!(
        after_second.len(),
        2,
        "expected one session row per fixture run: {after_second:?}"
    );
    let jobs: Vec<_> = after_second
        .iter()
        .filter_map(|session| session.job_id.clone())
        .collect();
    assert!(
        jobs.iter().filter(|job| **job == first_job).count() == 1,
        "the first run's job id must survive unchanged: {jobs:?}"
    );
    assert!(
        jobs.iter().any(|job| *job != first_job),
        "the second run must use a new job id instead of reusing {first_job}: {jobs:?}"
    );
    for session in &after_second {
        assert!(
            !paths.session_dir(&session.id).exists(),
            "every fixture session directory must be removed: {:?}",
            paths.session_dir(&session.id)
        );
    }
}

#[test]
fn setup_verify_fixture_failure_removes_the_registered_project() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-cleanup");
    let ctx = env.ctx();
    // Force `upsert_project` to insert the row and then fail creating its database:
    // `home/projects` must be a directory, so a regular file blocks the fixture there.
    let projects = env.home.join("projects");
    std::fs::remove_dir_all(&projects).expect("remove projects dir");
    std::fs::write(&projects, b"not a directory").expect("block projects dir");

    let error = verify::fixture_job_merges(&ctx).expect_err("fixture construction must fail");
    assert!(
        !error.to_string().is_empty(),
        "fixture failure must carry a real error: {error}"
    );
    assert!(
        !env.home.join("setup-verify").exists(),
        "the fixture scratch area must be removed after a construction failure"
    );

    let registered = Store::open_global(&NodePaths::new(&env.home))
        .expect("open home")
        .list_projects()
        .expect("list projects");
    assert!(
        !registered
            .iter()
            .any(|project| project.name.as_str().starts_with("setup-verify-job-")),
        "a failed fixture construction must not leave a project row: {registered:?}"
    );
    assert!(
        projects.is_file(),
        "the test's blocking file must still be in place; the fixture must not move it"
    );
}

#[test]
fn setup_verify_snapshot_readable_from_control() {
    let control = TempDir::new().expect("temp control home");
    let fetched_at = Timestamp::now();
    write_control_home(control.path(), "fresh-vps", fetched_at);

    let check = verify::snapshot_readable_from_control(control.path()).expect("snapshot check");
    assert_eq!(check.status, "ok", "{}", check.detail);
    assert_eq!(check.name, "control.snapshot");
    assert!(
        check.detail.contains("snapshot as of"),
        "the check must report the fetch time: {}",
        check.detail
    );
    assert!(
        check.detail.contains("fresh-vps"),
        "the check must name the home: {}",
        check.detail
    );
    assert!(
        check.detail.contains(&fetched_at.as_rfc3339()),
        "the detail must show the cached `as of` time: {}",
        check.detail
    );

    std::fs::remove_file(control.path().join("snapshot.json")).expect("remove snapshot");
    let missing = verify::snapshot_readable_from_control(control.path()).expect("missing check");
    assert_eq!(missing.status, "fail", "{}", missing.detail);
    assert!(
        missing.detail.contains("no readable snapshot cache"),
        "{}",
        missing.detail
    );
}

#[test]
fn setup_verify_snapshot_rejects_a_different_home() {
    let control = TempDir::new().expect("temp control home");
    write_control_home(control.path(), "other-home", Timestamp::now());
    let config = serde_json::json!({
        "role": "control",
        "home": "fresh-vps",
        "ssh_target": "fake.example",
        "remote_home": "/home/vowp/.vowp",
        "created_at": Timestamp::now(),
    });
    std::fs::write(
        control.path().join("control.json"),
        serde_json::to_vec_pretty(&config).expect("control json"),
    )
    .expect("control.json");

    let check = verify::snapshot_readable_from_control(control.path()).expect("snapshot check");
    assert_eq!(check.status, "fail", "{}", check.detail);
    assert!(
        check.detail.contains("other-home") && check.detail.contains("fresh-vps"),
        "{}",
        check.detail
    );
}

#[test]
fn setup_verify_orchestrator_answers_chat() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-chat");
    let ctx = env.ctx();
    let check = verify::orchestrator_answers_chat(&ctx).expect("orchestrator chat check");
    assert_eq!(check.status, "ok", "{}", check.detail);
    assert_eq!(check.name, "orchestrator.chat");
    assert!(
        check.detail.contains("answered through `vowp chat`"),
        "{}",
        check.detail
    );
    assert!(
        check.detail.contains("session "),
        "the check must name the orchestrator session: {}",
        check.detail
    );
}

#[test]
fn setup_verify_run_reports_every_check() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-run");
    let ctx = env.ctx();
    let mut io = BufferIo::new("");
    let code = verify::run(&ctx, &mut io, &SetupVerifyArgs::default()).expect("run setup verify");
    let stdout = io.stdout();
    assert_eq!(code, 0, "{stdout}");
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
    assert_eq!(outcome["status"], "ok", "{outcome}");
    assert_eq!(outcome["phase"], "verify", "{outcome}");
    let checks = outcome["checks"].as_array().expect("checks array");
    let by_name = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing check {name}: {outcome}"))
    };
    assert_eq!(by_name("fixture.job_merge")["status"], "ok", "{outcome}");
    assert_eq!(by_name("orchestrator.chat")["status"], "ok", "{outcome}");
    assert_eq!(by_name("gc.dry_run")["status"], "ok", "{outcome}");
    assert_ne!(by_name("control.snapshot")["status"], "fail", "{outcome}");
    assert!(
        env.home.join("snapshot.json").is_file(),
        "verify must write a fresh home snapshot"
    );
}

#[test]
fn setup_verify_cli_runs_every_check() {
    let _fixture = fixture_lock();
    let env = TestHome::new("setup-verify-cli");
    // A second identical run on the same home must work: the first run's fixture
    // sessions and project are gone, so verify must not reuse a stale session.
    for run in 1..=2 {
        let output = env
            .command()
            .args(["--json", "setup", "verify"])
            .output()
            .expect("run vowp setup verify");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "setup verify run {run} failed: {stdout}\n{stderr}"
        );
        let outcome: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("invalid JSON in run {run} ({error}): {stdout}"));
        assert_eq!(outcome["status"], "ok", "run {run}: {outcome}");
        let checks = outcome["checks"].as_array().expect("checks array");
        let by_name = |name: &str| {
            checks
                .iter()
                .find(|check| check["name"] == name)
                .unwrap_or_else(|| panic!("missing check {name} in run {run}: {outcome}"))
        };
        assert_eq!(by_name("fixture.job_merge")["status"], "ok", "run {run}: {outcome}");
        assert_eq!(by_name("orchestrator.chat")["status"], "ok", "run {run}: {outcome}");
        assert_ne!(by_name("control.snapshot")["status"], "fail", "run {run}: {outcome}");
    }
}

/// Creates a remote home and installs the per-user `vowp` wrapper setup home would install.
///
/// Returns the home path, the project path and the home's own node name (the name
/// `control.json` records and the snapshot reports).
fn init_remote_home(root: &Path) -> (PathBuf, PathBuf, String) {
    let home = root.join(".vowp");
    let project = root.join("project");
    let tmux = root.join("tmux");
    std::fs::create_dir_all(&project).expect("remote project");
    std::fs::create_dir_all(&tmux).expect("remote tmux");
    for args in [
        vec!["init", "--quiet"],
        vec!["checkout", "--quiet", "-b", "main"],
        vec![
            "-c",
            "user.name=setup verify",
            "-c",
            "user.email=setup@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "remote base",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }
    let init = Command::new(vowp_binary())
        .arg("--home")
        .arg(&home)
        .args(["init", "--name", "remote-fixture", "--no-framework"])
        .current_dir(&project)
        .env("HOME", root)
        .env("TMUX_TMPDIR", &tmux)
        .env("NO_COLOR", "1")
        .env("VOWP_CONTROL_HOME", root.join("no-control"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_HOME")
        .output()
        .expect("run remote vowp init");
    assert!(
        init.status.success(),
        "remote vowp init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let binary = root.join(".local/bin/vowp");
    std::fs::create_dir_all(binary.parent().expect("binary dir")).expect("install dir");
    std::fs::write(
        &binary,
        format!("#!/bin/sh\nexec '{}' \"$@\"\n", vowp_binary().display()),
    )
    .expect("write remote vowp");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod remote vowp");
    }
    let node = Store::open_global(&NodePaths::new(&home))
        .expect("open remote home")
        .local_node()
        .expect("read remote node")
        .expect("remote home has a local node")
        .name
        .as_str()
        .to_owned();
    (home, project, node)
}

/// The fake ssh every control-path test starts from: run the remote command locally.
const FAKE_SSH: &str = r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
HOME="$FAKE_REMOTE_HOME"
VOWP_HOME="$FAKE_REMOTE_HOME/.vowp"
export HOME VOWP_HOME
unset VOWP_CONTROL_HOME
exec sh -c "$last"
"#;

/// A fake ssh whose probe succeeds but whose remote `setup verify` fails on stderr only.
const FAILING_VERIFY_SSH: &str = r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *setup*verify*) printf 'remote verify exploded\n' >&2; exit 1 ;;
esac
HOME="$FAKE_REMOTE_HOME"
VOWP_HOME="$FAKE_REMOTE_HOME/.vowp"
export HOME VOWP_HOME
unset VOWP_CONTROL_HOME
exec sh -c "$last"
"#;

/// A fake ssh whose home reports a successful verify document but whose readable
/// `snapshot.json` was generated an hour ago.
const OK_VERIFY_OLD_SNAPSHOT_SSH: &str = r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *setup*verify*)
    printf '%s\n' '{"status":"ok","phase":"verify","changed":[],"handoff":null,"checks":[{"name":"gc.dry_run","status":"ok","detail":"gc ok"},{"name":"fixture.job_merge","status":"ok","detail":"fixture merged into main"},{"name":"control.snapshot","status":"ok","detail":"snapshot as of now"},{"name":"orchestrator.chat","status":"ok","detail":"chat ok"}]}'
    exit 0
    ;;
esac
HOME="$FAKE_REMOTE_HOME"
VOWP_HOME="$FAKE_REMOTE_HOME/.vowp"
export HOME VOWP_HOME
unset VOWP_CONTROL_HOME
exec sh -c "$last"
"#;

/// Writes `control.json` and the fake `ssh`/`sudo` used by the control-path tests.
///
/// The fake ssh runs the remote command on this machine with the temporary remote home.
/// A CI runner usually has passwordless sudo, which would make the probe choose the
/// `vowp` system user; the fake target owns its home and has no such user, so its fake
/// sudo behaves like a target that asks for a password and setup stays the login user.
fn write_control_transport(control_root: &Path, bin: &Path, remote_node: &str, remote_home: &Path, ssh_body: &str) {
    std::fs::create_dir_all(control_root).expect("control dir");
    std::fs::create_dir_all(bin).expect("fake bin");
    let config = serde_json::json!({
        "role": "control",
        "home": remote_node,
        "ssh_target": "fake.example",
        "remote_home": remote_home.display().to_string(),
        "created_at": Timestamp::now(),
    });
    std::fs::write(
        control_root.join("control.json"),
        serde_json::to_vec_pretty(&config).expect("control json"),
    )
    .expect("control.json");
    write_script(&bin.join("ssh"), ssh_body);
    write_script(
        &bin.join("sudo"),
        "#!/bin/sh\nprintf 'sudo: a password is required\\n' >&2\nexit 1\n",
    );
}

#[test]
fn setup_verify_control_runs_the_checks_on_the_home() {
    let _fixture = fixture_lock();
    ensure_vowp_binary();
    let control = TempDir::new().expect("control home");
    let remote = TempDir::new().expect("remote home");
    let (remote_home, _remote_project, remote_node) = init_remote_home(remote.path());
    let control_root = control.path().join("control");
    let bin = control.path().join("bin");
    let tmux = control.path().join("tmux");
    std::fs::create_dir_all(&tmux).expect("control tmux");
    write_control_transport(&control_root, &bin, &remote_node, &remote_home, FAKE_SSH);

    let output = Command::new(vowp_binary())
        .args(["--json", "setup", "verify"])
        .env("HOME", control.path())
        .env("VOWP_CONTROL_HOME", &control_root)
        .env("TMUX_TMPDIR", &tmux)
        .env("NO_COLOR", "1")
        .env("FAKE_REMOTE_HOME", remote.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()),
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_HOME")
        .output()
        .expect("run control setup verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "control setup verify failed: {stdout}\n{stderr}"
    );
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
    assert_eq!(outcome["status"], "ok", "{outcome}");
    let checks = outcome["checks"].as_array().expect("checks array");
    let by_name = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing check {name}: {outcome}"))
    };
    assert_eq!(by_name("fixture.job_merge")["status"], "ok", "{outcome}");
    assert_eq!(by_name("orchestrator.chat")["status"], "ok", "{outcome}");
    assert_eq!(by_name("control.snapshot")["status"], "ok", "{outcome}");
    assert!(
        control_root.join("snapshot.json").is_file(),
        "the control machine must cache the fetched home snapshot"
    );
    assert!(
        remote_home.join("snapshot.json").is_file(),
        "the home checks must write a fresh snapshot on the home"
    );
}

#[test]
fn setup_verify_control_rejects_a_stale_home_snapshot() {
    let _fixture = fixture_lock();
    ensure_vowp_binary();
    let control = TempDir::new().expect("control home");
    let remote = TempDir::new().expect("remote home");
    let (remote_home, _remote_project, remote_node) = init_remote_home(remote.path());
    // A previous run's valid snapshot. If the fresh write below fails, the control must
    // not fetch this file and report it as the result of this verification.
    let mut old = snapshot(&remote_node);
    old.generated_at = Timestamp::now().minus(Duration::from_secs(3_600));
    std::fs::write(
        remote_home.join("snapshot.json"),
        serde_json::to_vec_pretty(&old).expect("old snapshot json"),
    )
    .expect("old snapshot");
    // `write_snapshot` writes through `<home>/tmp`; a read-only directory there makes
    // the home's fresh snapshot write fail while leaving the old snapshot in place.
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(remote_home.join("tmp"), std::fs::Permissions::from_mode(0o555)).expect("chmod tmp");
    }

    let control_root = control.path().join("control");
    let bin = control.path().join("bin");
    let tmux = control.path().join("tmux");
    std::fs::create_dir_all(&tmux).expect("control tmux");
    write_control_transport(&control_root, &bin, &remote_node, &remote_home, FAKE_SSH);
    let output = Command::new(vowp_binary())
        .args(["--json", "setup", "verify"])
        .env("HOME", control.path())
        .env("VOWP_CONTROL_HOME", &control_root)
        .env("TMUX_TMPDIR", &tmux)
        .env("NO_COLOR", "1")
        .env("FAKE_REMOTE_HOME", remote.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()),
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_HOME")
        .output()
        .expect("run control setup verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a failed home snapshot must fail control setup verify: {stdout}\n{stderr}"
    );
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
    assert_eq!(outcome["status"], "failed", "{outcome}");
    let checks = outcome["checks"].as_array().expect("checks array");
    let snapshot_check = checks
        .iter()
        .find(|check| check["name"] == "control.snapshot")
        .unwrap_or_else(|| panic!("missing control.snapshot: {outcome}"));
    assert_eq!(snapshot_check["status"], "fail", "{outcome}");
    let detail = snapshot_check["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("the home's own snapshot check failed"),
        "the control must preserve the home's own snapshot failure, not replace it \
         with the old file or a parse error: {outcome}"
    );
    // A wholesale remote-parse failure also marks every check `fail`. The other three
    // checks prove the home's complete verify document was parsed and ran.
    for name in ["gc.dry_run", "fixture.job_merge", "orchestrator.chat"] {
        let check = checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {outcome}"));
        assert_ne!(
            check["status"], "fail",
            "the home's {name} check must still run; a wholesale remote failure must \
             not satisfy this test: {outcome}"
        );
    }
    assert!(
        !control_root.join("snapshot.json").exists(),
        "a stale snapshot must not be cached as this run's result: {outcome}"
    );
}

#[test]
fn setup_verify_control_rejects_an_old_generated_at() {
    ensure_vowp_binary();
    let control = TempDir::new().expect("control home");
    let remote = TempDir::new().expect("remote home");
    let remote_home = remote.path().join(".vowp");
    std::fs::create_dir_all(&remote_home).expect("remote home dir");
    // The home's own check passes and the file is valid and readable, but it was
    // generated an hour before this verification started, beyond the 60 s allowance.
    let stale_at = Timestamp::now().minus(Duration::from_secs(3_600));
    let mut stale = snapshot("remote-fixture");
    stale.generated_at = stale_at;
    std::fs::write(
        remote_home.join("snapshot.json"),
        serde_json::to_vec_pretty(&stale).expect("stale snapshot json"),
    )
    .expect("stale snapshot");

    let control_root = control.path().join("control");
    let bin = control.path().join("bin");
    let tmux = control.path().join("tmux");
    std::fs::create_dir_all(&tmux).expect("control tmux");
    write_control_transport(
        &control_root,
        &bin,
        "remote-fixture",
        &remote_home,
        OK_VERIFY_OLD_SNAPSHOT_SSH,
    );
    let output = Command::new(vowp_binary())
        .args(["--json", "setup", "verify"])
        .env("HOME", control.path())
        .env("VOWP_CONTROL_HOME", &control_root)
        .env("TMUX_TMPDIR", &tmux)
        .env("NO_COLOR", "1")
        .env("FAKE_REMOTE_HOME", remote.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()),
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_HOME")
        .output()
        .expect("run control setup verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an old snapshot must fail control setup verify: {stdout}\n{stderr}"
    );
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
    assert_eq!(outcome["status"], "failed", "{outcome}");
    let snapshot_check = outcome["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == "control.snapshot")
        .unwrap_or_else(|| panic!("missing control.snapshot: {outcome}"));
    assert_eq!(snapshot_check["status"], "fail", "{outcome}");
    let detail = snapshot_check["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("refusing a stale snapshot"),
        "the freshness branch must reject the old file: {outcome}"
    );
    assert!(
        detail.contains(&stale_at.as_rfc3339()),
        "the failure must show the snapshot's generated_at time: {outcome}"
    );
    assert!(
        !control_root.join("snapshot.json").exists(),
        "a rejected snapshot must not be cached: {outcome}"
    );
}

#[test]
fn setup_verify_control_surfaces_remote_stderr() {
    let _fixture = fixture_lock();
    ensure_vowp_binary();
    let control = TempDir::new().expect("control home");
    let remote = TempDir::new().expect("remote home");
    let remote_home = remote.path().join(".vowp");
    let control_root = control.path().join("control");
    let bin = control.path().join("bin");
    let tmux = control.path().join("tmux");
    std::fs::create_dir_all(&tmux).expect("control tmux");
    write_control_transport(&control_root, &bin, "remote-fixture", &remote_home, FAILING_VERIFY_SSH);
    let output = Command::new(vowp_binary())
        .args(["--json", "setup", "verify"])
        .env("HOME", control.path())
        .env("VOWP_CONTROL_HOME", &control_root)
        .env("TMUX_TMPDIR", &tmux)
        .env("NO_COLOR", "1")
        .env("FAKE_REMOTE_HOME", remote.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()),
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_HOME")
        .output()
        .expect("run control setup verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a failed remote verify must fail the control command: {stdout}\n{stderr}"
    );
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("invalid JSON ({error}): {stdout}"));
    assert_eq!(outcome["status"], "failed", "{outcome}");
    let checks = outcome["checks"].as_array().expect("checks array");
    for name in [
        "gc.dry_run",
        "fixture.job_merge",
        "control.snapshot",
        "orchestrator.chat",
    ] {
        let check = checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {outcome}"));
        let detail = check["detail"].as_str().unwrap_or_default();
        assert_eq!(check["status"], "fail", "{outcome}");
        assert!(
            detail.contains("remote verify exploded"),
            "the {name} failure must surface the remote stderr: {outcome}"
        );
    }
}
