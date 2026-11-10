//! End-to-end proof that a normal `vowp up`/`down` never calls tmux without a
//! private socket directory and `-L <private-name>`.
//!
//! The test replaces `tmux` on `PATH` with a logging shim. The shim is the only
//! tmux binary the child process can see, so a missing `-L` or `TMUX_TMPDIR`
//! cannot reach a real server.

mod common;

use std::path::{Path, PathBuf};

use std::os::unix::fs::PermissionsExt;

const SHIM: &str = r#"#!/bin/sh
printf '%s|' "${TMUX_TMPDIR:-unset}" >> "$TMUX_LOG"
printf '%s ' "$@" >> "$TMUX_LOG"
printf '\n' >> "$TMUX_LOG"
case " $* " in
    *" -V "*) printf '%s\n' "tmux 3.4"; exit 0 ;;
esac
case " $* " in
    *" list-sessions "*) printf '%s\n' "4242"; exit 0 ;;
    *" new-session "*|*" new-window "*) printf '%s\n' "@1"; exit 0 ;;
    *" has-session "*) exit 1 ;;
    *" show-options "*) printf '%s\n' "1"; exit 0 ;;
    *" list-windows "*) exit 0 ;;
    *) exit 0 ;;
esac
"#;

fn install_shim(dir: &Path) -> (PathBuf, PathBuf) {
    let script = dir.join("tmux");
    std::fs::write(&script, SHIM).expect("write tmux shim");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let log = dir.join("tmux.log");
    (script, log)
}

#[test]
fn up_and_down_only_call_a_private_tmux_server() {
    let env = common::TestEnv::new();
    let tools = tempfile::tempdir().expect("tools");
    let (_, log) = install_shim(tools.path());
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", &path)
        .env("TMUX_LOG", &log)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["up", "--once", "--no-tmux"])
        .output()
        .expect("run vowp up");
    assert!(
        output.status.success(),
        "up failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", &path)
        .env("TMUX_LOG", &log)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["down", "--kill-windows"])
        .output()
        .expect("run vowp down");
    assert!(
        output.status.success(),
        "down failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = std::fs::read_to_string(&log).expect("tmux shim log");
    let expected_dir = env.tmux_dir();
    let mut server_calls = 0;
    for line in text.lines() {
        let (dir, args) = line.split_once('|').expect("shim log line");
        assert_eq!(
            Path::new(dir),
            expected_dir,
            "tmux call escaped the private TMUX_TMPDIR: {line:?}"
        );
        assert!(
            !args.contains("-L default"),
            "tmux call used the default socket: {line:?}"
        );
        assert!(!args.contains("kill-server"), "unexpected kill-server: {line:?}");
        let is_version = args.split_whitespace().any(|arg| arg == "-V");
        if !is_version {
            server_calls += 1;
            assert!(args.starts_with("-L vowp-"), "server call lacks a private -L: {line:?}");
        }
    }
    assert!(server_calls > 0, "the shim saw no server calls; log: {text}");
    assert!(
        text.contains("list-windows"),
        "up/down did not exercise the tmux window path; log: {text}"
    );
}
