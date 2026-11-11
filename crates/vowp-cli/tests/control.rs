//! Control-machine, routing, snapshot-cache and tunnel integration tests.
//!
//! Every test runs the real binary in a temporary `HOME`, a temporary
//! `VOWP_CONTROL_HOME` and a private `TMUX_TMPDIR`. `ssh` is always a fake script on
//! `PATH`; no test touches the network, a real home directory or the default tmux server.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;
use vowp_cli::node_ctx::ControlConfig;
use vowp_protocol::{
    CapacitySnapshot, FleetStatus, NodeName, SNAPSHOT_FORMAT, SNAPSHOT_VERSION, Snapshot, SupervisorSnapshot, Timestamp,
};

/// A temporary machine: home directory, control directory and fake `bin`.
struct Machine {
    home: TempDir,
    control: TempDir,
    bin: TempDir,
    args: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let control = tempfile::tempdir().expect("temp control");
        let bin = tempfile::tempdir().expect("temp bin");
        std::fs::create_dir_all(home.path().join("tmux")).expect("private tmux dir");
        let args = bin.path().join("ssh-args");
        Self {
            home,
            control,
            bin,
            args,
        }
    }

    fn write_fake_ssh(&self, body: &str) {
        let path = self.bin.path().join("ssh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fake ssh");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");
    }

    /// Writes the `$HOME` and `vowp --version` probe answers used by `control init`.
    fn write_init_ssh(&self) {
        self.write_fake_ssh(
            "printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
             last=\"\"\n\
             for arg in \"$@\"; do last=\"$arg\"; done\n\
             case \"$last\" in\n\
               *HOME*) printf '/home/fake' ;;\n\
               *version*) printf 'vowp 0.1.0\\n' ;;\n\
               *) exit 0 ;;\n\
             esac\n",
        );
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vowp"));
        command
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("VOWP_CONTROL_HOME", self.control.path())
            .env_remove("VOWP_HOME")
            .env("TMUX_TMPDIR", self.home.path().join("tmux"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NO_COLOR")
            .env("FAKE_SSH_ARGS", &self.args)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        command
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().expect("run vowp")
    }

    fn seed_control(&self) -> PathBuf {
        std::fs::create_dir_all(self.control.path().join("spool")).expect("spool dir");
        let config = json!({
            "role": "control",
            "home": "vowp-home",
            "ssh_target": "fake.example",
            "remote_home": "/home/fake/.vowp",
            "created_at": Timestamp::now().as_rfc3339(),
        });
        let path = self.control.path().join("control.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&config).expect("config json")).expect("control.json");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod control.json");
        path
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            schema: SNAPSHOT_FORMAT.to_owned(),
            version: SNAPSHOT_VERSION,
            sequence: 7,
            generated_at: Timestamp::now(),
            home: NodeName::parse("vowp-home").expect("home slug"),
            supervisor: SupervisorSnapshot {
                running: true,
                pid: Some(4242),
                last_tick: Some(Timestamp::now()),
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

    fn write_cache(&self) {
        let snapshot = self.snapshot();
        std::fs::write(
            self.control.path().join("snapshot.json"),
            serde_json::to_vec_pretty(&snapshot).expect("snapshot json"),
        )
        .expect("snapshot cache");
        let meta = json!({
            "fetched_at": Timestamp::now().as_rfc3339(),
            "source": "remote",
        });
        std::fs::write(
            self.control.path().join("snapshot.meta.json"),
            serde_json::to_vec_pretty(&meta).expect("meta json"),
        )
        .expect("snapshot meta");
    }
}

/// Returns the last line the fake ssh recorded, when any.
fn recorded(machine: &Machine) -> Vec<String> {
    std::fs::read_to_string(&machine.args)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Returns true when `pid` currently exists (the same probe the CLI uses).
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Polls `predicate` for up to `timeout`.
fn eventually(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    predicate()
}

/// Kills a fake ssh process group when a test fails before calling `tunnel close`.
struct TunnelGuard(u32);

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.0)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[test]
fn control_init_writes_control_json() {
    let machine = Machine::new();
    machine.write_init_ssh();

    let output = machine.run(&["--json", "control", "init", "--name", "vowp-home", "fake.example"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("control init JSON");
    assert_eq!(value["role"], "control");
    assert_eq!(value["home"], "vowp-home");
    assert_eq!(value["ssh_target"], "fake.example");
    assert_eq!(value["remote_home"], "/home/fake/.vowp");

    let path = machine.control.path().join("control.json");
    let text = std::fs::read_to_string(&path).expect("control.json exists");
    let written: serde_json::Value = serde_json::from_str(&text).expect("control.json JSON");
    assert_eq!(written, value);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path)
        .expect("control.json metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "control.json must be private");
    assert!(machine.control.path().join("spool").is_dir(), "spool directory");
    assert!(machine.control.path().join("surfaces.json").is_file());
    assert!(machine.control.path().join("tunnels.json").is_file());
    let nodes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(machine.control.path().join("nodes.json")).expect("nodes"))
            .expect("nodes JSON");
    assert_eq!(nodes[0]["name"], "vowp-home");
    assert_eq!(nodes[0]["ssh_target"], "fake.example");

    // A second init refuses to overwrite without --force.
    let output = machine.run(&["control", "init", "--name", "vowp-home", "fake.example"]);
    assert_eq!(output.status.code(), Some(4), "overwrite without --force is a conflict");
}

#[test]
fn control_init_force_retargets_the_node_mirror() {
    let machine = Machine::new();
    machine.write_init_ssh();

    let output = machine.run(&["control", "init", "--name", "old-home", "old.example"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = machine.run(&["control", "init", "--force", "--name", "new-home", "new.example"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config: ControlConfig = serde_json::from_str(
        &std::fs::read_to_string(machine.control.path().join("control.json")).expect("control.json"),
    )
    .expect("control.json JSON");
    assert_eq!(config.home, "new-home");
    assert_eq!(config.ssh_target, "new.example");
    assert_eq!(config.remote_home, "/home/fake/.vowp");

    let nodes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(machine.control.path().join("nodes.json")).expect("nodes.json"))
            .expect("nodes JSON");
    let rows = nodes.as_array().expect("nodes array");
    assert_eq!(rows.len(), 1, "the old home's node must not survive: {nodes}");
    assert_eq!(rows[0]["name"], "new-home");
    assert_eq!(rows[0]["ssh_target"], "new.example");
}

#[test]
fn control_init_is_refused_on_a_home() {
    let home = tempfile::tempdir().expect("temp home");
    std::fs::create_dir_all(home.path().join("tmux")).expect("private tmux dir");
    let project = tempfile::tempdir().expect("temp project");
    let config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    std::fs::write(
        project.path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("vowp.toml");
    let store = vowp_store::Store::open(&vowp_store::NodePaths::new(home.path())).expect("store");
    let now = store.clock().now();
    store
        .upsert_project(&vowp_protocol::Project {
            name: config.project.name.clone(),
            path: project.path().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("project");

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    let args = bin.path().join("ssh-args");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\nexit 0\n").expect("fake ssh");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("VOWP_CONTROL_HOME")
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("FAKE_SSH_ARGS", &args)
        .env(
            "PATH",
            format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default()),
        )
        .arg("--home")
        .arg(home.path())
        .args(["control", "init", "--name", "h", "fake.example"])
        .output()
        .expect("run vowp");

    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("this is a home"), "{stderr}");
    assert!(
        !home.path().join(".vowp-control").join("control.json").exists(),
        "control init must not write a control config on a home"
    );
    assert!(!args.exists(), "control init must refuse before probing the ssh target");
}

#[test]
fn control_version_json_runs_store_free_on_control() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["--json", "version"]);
    assert!(
        output.status.success(),
        "version on a control machine must not need a local store; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("version JSON");
    assert!(
        value["version"].as_str().unwrap_or_default().starts_with("0.1"),
        "{value}"
    );
    assert_eq!(value["schema"], 1);
    assert!(recorded(&machine).is_empty(), "version must stay local");
}

#[test]
fn control_routes_status_over_fake_ssh() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh(
        "printf '%s\\n' \"$@\" > \"$FAKE_SSH_ARGS\"\n\
         printf 'remote status output\\n'\n",
    );
    let output = machine.run(&["status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("remote status output"), "{stdout}");

    let calls = recorded(&machine);
    assert_eq!(
        calls,
        vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "ConnectTimeout=10".to_owned(),
            "fake.example".to_owned(),
            "vowp '--home' '/home/fake/.vowp' 'status'".to_owned(),
        ],
        "status must be one routed ssh call with the home path"
    );
}

#[test]
fn control_fleet_status_refreshes_the_cache() {
    let machine = Machine::new();
    machine.seed_control();
    let snapshot = machine.snapshot();
    let snapshot_path = machine.bin.path().join("snapshot.json");
    std::fs::write(
        &snapshot_path,
        serde_json::to_vec_pretty(&snapshot).expect("snapshot json"),
    )
    .expect("snapshot file");
    machine.write_fake_ssh(
        "printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
         last=\"\"\n\
         for arg in \"$@\"; do last=\"$arg\"; done\n\
         case \"$last\" in\n\
           cat\\ *) cat \"$FAKE_SNAPSHOT\" ;;\n\
           *) printf 'remote fleet status\\n' ;;\n\
         esac\n",
    );
    let output = machine
        .command()
        .env("FAKE_SNAPSHOT", &snapshot_path)
        .args(["fleet", "status"])
        .output()
        .expect("run fleet status");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("remote fleet status"));
    assert!(
        calls_include(&machine, "vowp '--home' '/home/fake/.vowp' 'fleet' 'status'"),
        "fleet status was not routed: {:?}",
        recorded(&machine)
    );

    let home_document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snapshot_path).expect("home snapshot")).expect("home snapshot JSON");
    let cached: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(machine.control.path().join("snapshot.json")).expect("cached snapshot"),
    )
    .expect("cached snapshot JSON");
    assert_eq!(
        cached, home_document,
        "the cache must hold the home's document, not a placeholder or an older snapshot"
    );

    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(machine.control.path().join("snapshot.meta.json")).expect("snapshot meta"),
    )
    .expect("snapshot meta JSON");
    assert_eq!(meta["source"], "remote");
    let fetched_at = Timestamp::parse(meta["fetched_at"].as_str().expect("fetched_at")).expect("timestamp");
    let age = Timestamp::now().unix_secs().saturating_sub(fetched_at.unix_secs());
    assert!(age < 60, "the cache metadata must be fresh, age {age}s");
}

/// Returns whether any recorded ssh invocation contains `needle`.
fn calls_include(machine: &Machine, needle: &str) -> bool {
    recorded(machine).iter().any(|line| line == needle)
}

#[test]
fn control_cache_serves_as_of_when_home_down() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("exit 255\n");

    let output = machine.run(&["status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("as of"), "cache fallback needs an as-of time: {stdout}");
    assert!(stdout.contains("vowp-home"), "{stdout}");

    let output = machine.run(&["--json", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["source"], "cache");
    assert_eq!(value["home"], "vowp-home");
    assert_eq!(value["stale"], false);
    assert!(value["as_of"].as_str().is_some());

    let output = machine.run(&["--json", "fleet", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("fleet status JSON");
    assert_eq!(value["source"], "cache");
    assert_eq!(value["snapshot"]["home"], "vowp-home");
    assert_eq!(value["stale"], false);

    let output = machine.run(&["fleet", "status", "--refresh"]);
    assert_eq!(
        output.status.code(),
        Some(7),
        "--refresh must fail when the home is unreachable; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn control_fleet_status_cached_never_uses_ssh() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["--json", "fleet", "status", "--cached"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("fleet status JSON");
    assert_eq!(value["source"], "cache");
    assert_eq!(value["snapshot"]["home"], "vowp-home");
    assert!(recorded(&machine).is_empty(), "--cached must not use SSH");
}

#[test]
fn control_write_never_uses_cache() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\nexit 255\n");

    let output = machine.run(&["job", "add", "--id", "j1", "--title", "T", "--kind", "build"]);
    assert_eq!(
        output.status.code(),
        Some(7),
        "a write must not succeed against the cache; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !recorded(&machine).is_empty(),
        "the write must have been sent to the home"
    );
}

#[test]
fn control_routes_secret_writes_with_tty_and_requests_through_a_tunnel() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh(
        "printf '%s\\n' '---' >> \"$FAKE_SSH_ARGS\"\n\
         printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
         if [ \"$1\" = \"-N\" ]; then exec sleep 30; fi\n\
         last=\"\"\n\
         for arg in \"$@\"; do last=\"$arg\"; done\n\
         case \"$last\" in\n\
           *secret*request*)\n\
             printf 'open http://127.0.0.1:4096/s/test-token\\n'\n\
             printf 'expires 2030-01-01T00:00:00.000Z (single use, 10 minutes)\\n'\n\
             printf 'DEEPSEEK_API_KEY saved, ends in -1234\\n'\n\
             ;;\n\
         esac\n\
         exit 0\n",
    );

    // §9.3: the home's hidden prompt needs a TTY.
    let output = machine.run(&["secret", "set", "DEEPSEEK_API_KEY"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = recorded_calls(&machine);
    let set_call = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg.contains("'secret' 'set'")))
        .expect("the write was routed to the home");
    assert!(
        set_call.iter().any(|arg| arg == "-t"),
        "secret set must use ssh -t: {set_call:?}"
    );
    assert_eq!(
        set_call.last().map(String::as_str),
        Some("vowp '--home' '/home/fake/.vowp' 'secret' 'set' 'DEEPSEEK_API_KEY'")
    );

    std::fs::remove_file(&machine.args).expect("clear recorded ssh calls");
    let output = machine.run(&["secret", "request", "DEEPSEEK_API_KEY"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let calls = recorded_calls(&machine);
    let request_call = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg.contains("'secret' 'request'")))
        .expect("the request was routed to the home");
    assert_eq!(
        request_call.last().map(String::as_str),
        Some("vowp '--home' '/home/fake/.vowp' 'secret' 'request' 'DEEPSEEK_API_KEY'"),
        "the wrapper asks the home for the human header, not a second JSON document"
    );
    assert!(
        !request_call.iter().any(|arg| arg == "--json"),
        "the remote request must not print its raw URL: {request_call:?}"
    );

    let tunnel_call = calls
        .iter()
        .find(|call| call.first().is_some_and(|arg| arg == "-N"))
        .expect("secret request must open a local tunnel");
    let forward = tunnel_call
        .iter()
        .position(|arg| arg == "-L")
        .and_then(|index| tunnel_call.get(index + 1))
        .expect("the tunnel has a -L forward");
    let (local, remote) = forward
        .strip_prefix("127.0.0.1:")
        .and_then(|rest| rest.split_once(':'))
        .expect("the forward names the local and home ports");
    assert_eq!(remote, "127.0.0.1:4096", "{forward}");
    let local_port: u16 = local.parse().expect("local port");
    assert_eq!(tunnel_call.last().map(String::as_str), Some("fake.example"));
    assert!(
        stdout.contains(&format!("http://127.0.0.1:{local_port}/s/test-token")),
        "the owner must see the local URL: {stdout}"
    );
    assert!(
        !stdout.contains(":4096/"),
        "the home's raw URL must never be printed: {stdout}"
    );
    assert!(
        stdout.contains("DEEPSEEK_API_KEY saved, ends in -1234"),
        "the home's completion line is forwarded: {stdout}"
    );

    let tunnels: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(machine.control.path().join("tunnels.json")).expect("tunnels.json"),
    )
    .expect("tunnels JSON");
    assert_eq!(tunnels, json!([]), "the request wrapper closes the tunnel it opened");
}

/// Splits the fake ssh log into one argument vector per invocation.
fn recorded_calls(machine: &Machine) -> Vec<Vec<String>> {
    std::fs::read_to_string(&machine.args)
        .unwrap_or_default()
        .split("---\n")
        .filter_map(|chunk| {
            let lines: Vec<String> = chunk.lines().map(str::to_owned).collect();
            (!lines.is_empty()).then_some(lines)
        })
        .collect()
}

#[test]
fn up_refuses_on_control() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["up"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("control machine"), "{stderr}");
    assert!(
        recorded(&machine).is_empty(),
        "up must refuse locally, not by shelling out to the home"
    );
}

#[test]
fn control_status_prints_cache_metadata_without_ssh() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["--json", "control", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("control status JSON");
    assert_eq!(value["config"]["home"], "vowp-home");
    assert_eq!(value["config"]["remote_home"], "/home/fake/.vowp");
    assert_eq!(value["cache"]["source"], "cache");
    assert_eq!(value["cache"]["stale"], false);
    assert!(value["cache"]["fetched_at"].as_str().is_some());
    assert_eq!(value["cache"]["snapshot"]["home"], "vowp-home");
    assert!(recorded(&machine).is_empty(), "control status must not use SSH");

    let output = machine.run(&["control", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("as of"), "{stdout}");
    assert!(stdout.contains("vowp-home"), "{stdout}");
}

#[test]
fn control_status_json_uses_spec_types() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["--json", "control", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("control status JSON");

    let config: ControlConfig =
        serde_json::from_value(value["config"].clone()).expect("config is a ControlConfig document");
    assert_eq!(config.role, "control");
    assert_eq!(config.home, "vowp-home");
    assert_eq!(config.remote_home, "/home/fake/.vowp");
    assert_eq!(config.ssh_target, "fake.example");

    let cache: FleetStatus = serde_json::from_value(value["cache"].clone()).expect("cache is a FleetStatus document");
    assert_eq!(cache.source, "cache");
    assert!(!cache.stale, "the fixture cache is fresh");
    assert_eq!(cache.snapshot.home.as_str(), "vowp-home");
    assert_eq!(cache.snapshot.supervisor.pid, Some(4242));
    assert!(recorded(&machine).is_empty(), "control status must not use SSH");

    // Before the first refresh there is no FleetStatus; the config document still works.
    std::fs::remove_file(machine.control.path().join("snapshot.json")).expect("remove snapshot");
    std::fs::remove_file(machine.control.path().join("snapshot.meta.json")).expect("remove meta");
    let output = machine.run(&["--json", "control", "status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("control status JSON");
    let config: ControlConfig =
        serde_json::from_value(value["config"].clone()).expect("config is a ControlConfig document");
    assert_eq!(config.home, "vowp-home");
    assert_eq!(value["cache"], serde_json::Value::Null);
}

#[test]
fn control_status_check_fails_on_stale_cached_heartbeat() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_cache();
    machine.write_fake_ssh("exit 255\n");

    // A fresh cache with a fresh heartbeat passes the watchdog even when the home is down.
    let output = machine.run(&["status", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a fresh cached heartbeat must pass; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("as of"), "cache fallback needs an as-of time: {stdout}");
    assert!(
        stdout.contains("fresh"),
        "the human line must say the cache is fresh: {stdout}"
    );

    let output = machine.run(&["--json", "status", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["source"], "cache");
    assert_eq!(value["stale"], false);

    // A freshly fetched cache whose heartbeat is six minutes old is stale for the watchdog.
    let path = machine.control.path().join("snapshot.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("snapshot cache")).expect("snapshot JSON");
    value["supervisor"]["last_tick"] = json!(Timestamp::now().minus(Duration::from_secs(600)).as_rfc3339());
    std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("snapshot json")).expect("write snapshot");

    let output = machine.run(&["status", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a stale cached heartbeat must exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("as of"), "{stdout}");
    assert!(
        stdout.contains("stale"),
        "the human line must say the cache is stale: {stdout}"
    );

    let output = machine.run(&["--json", "status", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["source"], "cache");
    assert_eq!(value["stale"], true);
    assert_eq!(value["home"], "vowp-home");

    // Without --check the same stale cache renders with exit 0 (v0.1 behaviour).
    let output = machine.run(&["status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "status without --check always exits 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("stale"));
}

#[test]
fn node_registry_lives_on_control() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh(
        "printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
         last=\"\"\n\
         for arg in \"$@\"; do last=\"$arg\"; done\n\
         case \"$last\" in\n\
           *version*) printf 'vowp 0.1.0\\n' ;;\n\
           *) exit 0 ;;\n\
         esac\n",
    );

    let output = machine.run(&["--json", "node", "add", "vps", "vps.example"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let nodes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(machine.control.path().join("nodes.json")).expect("nodes.json"))
            .expect("nodes JSON");
    assert_eq!(nodes.as_array().expect("array").len(), 1, "{nodes}");
    assert_eq!(nodes[0]["name"], "vps");
    assert_eq!(nodes[0]["ssh_target"], "vps.example");
    assert_eq!(nodes[0]["capabilities"]["vowp"], "vowp 0.1.0");

    std::fs::remove_file(&machine.args).expect("clear recorded ssh calls");
    let output = machine.run(&["--json", "node", "ls"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("node list JSON");
    assert_eq!(listed[0]["name"], "vps");
    assert!(recorded(&machine).is_empty(), "node ls reads the local mirror");

    let output = machine.run(&["node", "rm", "vps"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let nodes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(machine.control.path().join("nodes.json")).expect("nodes.json"))
            .expect("nodes JSON");
    assert_eq!(nodes, json!([]));
}

#[test]
fn tunnel_open_records_pid_and_port() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh("printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\nexec sleep 30\n");

    let output = machine.run(&[
        "--json",
        "tunnel",
        "open",
        "web",
        "--remote-port",
        "4096",
        "--local-port",
        "41234",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("tunnel JSON");
    assert_eq!(value["name"], "web");
    assert_eq!(value["local_port"], 41234);
    assert_eq!(value["remote_port"], 4096);
    assert_eq!(value["host"], "127.0.0.1");
    let pid = value["pid"].as_u64().expect("pid") as u32;
    let _guard = TunnelGuard(pid);
    assert!(pid > 0);
    assert!(alive(pid), "the tunnel process must still be running");

    let text = std::fs::read_to_string(machine.control.path().join("tunnels.json")).expect("tunnels.json");
    let tunnels: serde_json::Value = serde_json::from_str(&text).expect("tunnels JSON");
    assert_eq!(tunnels[0]["pid"], pid);
    assert_eq!(tunnels[0]["local_port"], 41234);

    let output = machine.run(&["tunnel", "close", "web"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        eventually(|| !alive(pid), Duration::from_secs(3)),
        "close must stop the ssh process"
    );
    let tunnels: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(machine.control.path().join("tunnels.json")).expect("tunnels"))
            .expect("tunnels JSON");
    assert_eq!(tunnels, json!([]));
}

#[test]
fn tunnel_close_kills_group() {
    let machine = Machine::new();
    machine.seed_control();
    let child_file = machine.bin.path().join("ssh-child");
    machine.write_fake_ssh(
        "printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
         sleep 30 &\n\
         printf '%s' \"$!\" > \"$FAKE_SSH_CHILD\"\n\
         wait\n",
    );
    let output = machine
        .command()
        .env("FAKE_SSH_CHILD", &child_file)
        .args([
            "--json",
            "tunnel",
            "open",
            "web",
            "--remote-port",
            "4096",
            "--local-port",
            "41235",
        ])
        .output()
        .expect("open tunnel");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("tunnel JSON");
    let pid = value["pid"].as_u64().expect("pid") as u32;
    let _guard = TunnelGuard(pid);
    let child: u32 = std::fs::read_to_string(&child_file)
        .expect("fake ssh recorded its child")
        .trim()
        .parse()
        .expect("child pid");
    assert!(alive(child), "the fake tunnel child must be alive");

    let output = machine.run(&["tunnel", "close", "web"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        eventually(|| !alive(pid) && !alive(child), Duration::from_secs(3)),
        "close must signal the whole process group (ssh {pid}, child {child})"
    );
}

#[test]
fn tunnel_refuses_privileged_port_without_flag() {
    let machine = Machine::new();
    machine.seed_control();
    machine.write_fake_ssh("printf 'ssh was invoked\\n' >> \"$FAKE_SSH_ARGS\"\nexit 0\n");

    let output = machine.run(&["tunnel", "open", "web", "--remote-port", "4096", "--local-port", "80"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(recorded(&machine).is_empty(), "a refused port must not spawn ssh");
    let path = machine.control.path().join("tunnels.json");
    if path.is_file() {
        let text = std::fs::read_to_string(path).expect("tunnels.json");
        assert_eq!(text.trim(), "[]");
    }
}

#[test]
fn tunnel_list_cleans_dead_pid() {
    let machine = Machine::new();
    machine.seed_control();
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().expect("spawn child");
    let dead_pid = child.id();
    assert!(child.wait().expect("reap child").success());
    let live_pid = std::process::id();
    let started = Timestamp::now().as_rfc3339();
    let tunnels = json!([
        {
            "name": "dead",
            "local_port": 41000,
            "remote_port": 4000,
            "host": "127.0.0.1",
            "pid": dead_pid,
            "started_at": started,
        },
        {
            "name": "live",
            "local_port": 41001,
            "remote_port": 4001,
            "host": "127.0.0.1",
            "pid": live_pid,
            "started_at": started,
        }
    ]);
    std::fs::write(
        machine.control.path().join("tunnels.json"),
        serde_json::to_vec_pretty(&tunnels).expect("tunnels json"),
    )
    .expect("tunnels.json");

    let output = machine.run(&["--json", "tunnel", "ls"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("tunnel list JSON");
    assert_eq!(listed.as_array().expect("array").len(), 1, "{listed}");
    assert_eq!(listed[0]["name"], "live");

    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(machine.control.path().join("tunnels.json")).expect("tunnels.json"),
    )
    .expect("tunnels JSON");
    assert_eq!(persisted.as_array().expect("array").len(), 1, "{persisted}");
    assert_eq!(persisted[0]["name"], "live");
}

#[test]
fn tunnel_refuses_on_home() {
    let home = tempfile::tempdir().expect("temp home");
    std::fs::create_dir_all(home.path().join("tmux")).expect("private tmux dir");
    let project = tempfile::tempdir().expect("temp project");
    let config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    std::fs::write(
        project.path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("vowp.toml");
    let store = vowp_store::Store::open(&vowp_store::NodePaths::new(home.path())).expect("store");
    let now = store.clock().now();
    store
        .upsert_project(&vowp_protocol::Project {
            name: config.project.name.clone(),
            path: project.path().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("project");

    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("VOWP_CONTROL_HOME")
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .arg("--home")
        .arg(home.path())
        .args(["tunnel", "ls"])
        .output()
        .expect("run vowp");
    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("machine that will use them"));
}
