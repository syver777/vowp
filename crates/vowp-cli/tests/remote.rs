//! `--node` tests: pure ssh argv construction and the exit-7 remote failure path.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use vowp_cli::remote::build_ssh_argv;
use vowp_protocol::{Node, NodeName};
use vowp_store::{NodePaths, Store};

#[test]
fn node_ssh_argv_quoting() {
    let args = vec![
        "job".to_owned(),
        "add".to_owned(),
        "--title".to_owned(),
        "a b's $HOME".to_owned(),
        String::new(),
    ];
    let argv = build_ssh_argv("vps.example", &args, false);
    assert_eq!(
        argv,
        vec![
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "vps.example",
            "vowp 'job' 'add' '--title' 'a b'\\''s $HOME' ''",
        ]
    );
    assert!(!argv.iter().any(|part| part == "-t"));

    let interactive = build_ssh_argv("vps.example", &["attach".to_owned()], true);
    assert_eq!(
        interactive,
        vec![
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-t",
            "vps.example",
            "vowp 'attach'",
        ]
    );
}

#[test]
fn node_flag_remote_failure_exit_7() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    let store = Store::open(&paths).expect("store opens");
    let now = store.clock().now();
    store
        .upsert_node(&Node {
            name: NodeName::parse("vps").expect("slug"),
            ssh_target: Some("fake.example".to_owned()),
            is_local: false,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: now,
        })
        .expect("node row");

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nexit 255\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path_value = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());

    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(bin.path())
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PATH", path_value)
        .arg("--home")
        .arg(home.path())
        .args(["--json", "--node", "vps", "job", "ls"])
        .output()
        .expect("run vowp");
    assert_eq!(
        output.status.code(),
        Some(7),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON error on stdout");
    assert_eq!(value["error"]["kind"], "remote");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fake.example"),
        "{value}"
    );
}

#[test]
fn node_flag_forwards_original_args_without_argv0() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    let store = Store::open(&paths).expect("store opens");
    let now = store.clock().now();
    store
        .upsert_node(&Node {
            name: NodeName::parse("vps").expect("slug"),
            ssh_target: Some("fake.example".to_owned()),
            is_local: false,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: now,
        })
        .expect("node row");

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_SSH_ARGS\"\nexit 0\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path_value = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    let args_file = bin.path().join("ssh-args");

    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(bin.path())
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PATH", path_value)
        .env("FAKE_SSH_ARGS", &args_file)
        .arg("--home")
        .arg(home.path())
        .args(["--node", "vps", "job", "ls"])
        .output()
        .expect("run vowp");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let recorded = std::fs::read_to_string(&args_file).expect("fake ssh recorded its argv");
    let lines: Vec<&str> = recorded.lines().collect();
    assert!(lines.contains(&"fake.example"), "{recorded:?}");
    let remote = lines.last().expect("remote command line");
    assert_eq!(*remote, format!("vowp '--home' '{}' 'job' 'ls'", home.path().display()));
    assert!(
        !recorded.contains(env!("CARGO_BIN_EXE_vowp")),
        "argv[0] leaked into the remote command: {recorded:?}"
    );
}

#[test]
fn node_flag_redirects_doctor_over_ssh() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    let store = Store::open(&paths).expect("store opens");
    let now = store.clock().now();
    store
        .upsert_node(&Node {
            name: NodeName::parse("vps").expect("slug"),
            ssh_target: Some("fake.example".to_owned()),
            is_local: false,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: now,
        })
        .expect("node row");

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_SSH_ARGS\"\nexit 0\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path_value = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    let args_file = bin.path().join("doctor-ssh-args");

    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(bin.path())
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PATH", path_value)
        .env("FAKE_SSH_ARGS", &args_file)
        .arg("--home")
        .arg(home.path())
        .args(["--node", "vps", "doctor"])
        .output()
        .expect("run vowp");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("doctor: ok"), "doctor ran locally: {stdout}");
    let recorded = std::fs::read_to_string(&args_file).expect("fake ssh was not invoked");
    let remote = recorded.lines().last().expect("remote command line");
    assert_eq!(*remote, format!("vowp '--home' '{}' 'doctor'", home.path().display()));
}

#[test]
fn node_flag_returns_remote_exit_code_and_ignores_local_node() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    let store = Store::open(&paths).expect("store opens");
    let now = store.clock().now();
    for (name, target, local) in [("vps", Some("fake.example"), false), ("here", None, true)] {
        store
            .upsert_node(&Node {
                name: NodeName::parse(name).expect("slug"),
                ssh_target: target.map(str::to_owned),
                is_local: local,
                capabilities: serde_json::json!({}),
                last_seen_at: None,
                created_at: now,
            })
            .expect("node row");
    }

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nexit 42\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path_value = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());
    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(bin.path())
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("PATH", path_value)
        .arg("--home")
        .arg(home.path())
        .args(["--node", "vps", "status"])
        .output()
        .expect("run vowp");
    assert_eq!(output.status.code(), Some(42));

    // A local node is not redirected: the command runs locally and fails for lack of a
    // project, not by spawning ssh (the fake exits 42).
    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(bin.path())
        .env("TMUX_TMPDIR", home.path().join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .arg("--home")
        .arg(home.path())
        .args(["--node", "here", "status"])
        .output()
        .expect("run vowp");
    assert_eq!(output.status.code(), Some(2), "no project is invalid input");
}
