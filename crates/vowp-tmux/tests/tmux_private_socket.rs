//! Integration tests against a private tmux server.
//!
//! Each test uses `tmux -L vowp-test-<hex>`; the guard kills that server.

mod common;

use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use common::PrivateTmux;
use vowp_tmux::{LayoutSpec, Tmux, TmuxError, WindowManager, WindowSpec};

fn sleep_spec() -> WindowSpec {
    WindowSpec {
        argv: vec!["sleep".to_owned(), "30".to_owned()],
        ..WindowSpec::default()
    }
}

fn layout_spec() -> LayoutSpec {
    let argv = || vec!["sleep".to_owned(), "30".to_owned()];
    LayoutSpec {
        board: argv(),
        supervisor: argv(),
        planner: argv(),
        keeper: argv(),
        inbox: argv(),
        cwd: None,
    }
}

fn poll_until(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    check()
}

#[test]
fn ensure_session_creates_and_is_idempotent() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-one").unwrap();
    assert!(guard.tmux.session_exists("vowp-one").unwrap());
    guard.tmux.ensure_session("vowp-one").unwrap();
    let windows = guard.tmux.list_windows("vowp-one").unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name, "scratch");
    assert!(windows[0].tagged);
}

#[test]
fn ensure_layout_creates_fixed_windows_in_order() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    let spec = layout_spec();
    guard.tmux.ensure_layout("demo", &spec).unwrap();
    let first = guard.tmux.list_windows("vowp-demo").unwrap();
    let names: Vec<&str> = first.iter().map(|window| window.name.as_str()).collect();
    assert_eq!(names, vec!["board", "supervisor", "planner", "keeper", "inbox"]);
    assert_eq!(first[0].name, "board");
    assert!(first.windows(2).all(|pair| pair[0].index < pair[1].index));
    assert!(first.iter().all(|window| window.tagged));

    guard.tmux.ensure_layout("demo", &spec).unwrap();
    let second = guard.tmux.list_windows("vowp-demo").unwrap();
    assert_eq!(first, second);

    guard.tmux.ensure_session("vowp-demo").unwrap();
    let after_ensure = guard.tmux.list_windows("vowp-demo").unwrap();
    assert_eq!(first, after_ensure);
}

#[test]
fn new_window_tags_and_runs_command() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-hello").unwrap();
    let spec = WindowSpec {
        argv: vec!["sh".to_owned(), "-c".to_owned(), "echo hello; sleep 30".to_owned()],
        ..WindowSpec::default()
    };
    guard.tmux.new_window("vowp-hello", "job-hello", &spec).unwrap();
    let windows = guard.tmux.list_windows("vowp-hello").unwrap();
    let window = windows
        .iter()
        .find(|window| window.name == "job-hello")
        .expect("created window is listed");
    assert!(window.tagged);
    assert!(window.pane_pid.is_some());

    assert!(
        poll_until(|| {
            guard
                .tmux
                .capture_pane("vowp-hello", "job-hello", 200)
                .is_ok_and(|text| text.contains("hello"))
        }),
        "pane output did not contain hello"
    );
}

#[test]
fn capture_pane_drains_large_output() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-large").unwrap();
    let history = guard.raw(&["set-option", "-g", "history-limit", "5000"]);
    assert!(
        history.status.success(),
        "raw set-option failed: {}",
        String::from_utf8_lossy(&history.stderr)
    );

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("DONE");
    let script = concat!(
        "i=0; while [ $i -lt 3000 ]; do ",
        "printf \"line-%04d-abcdefghijklmnopqrstuvwxyz0123456789\\n\" \"$i\"; ",
        "i=$((i+1)); done; touch DONE; sleep 60"
    );
    let spec = WindowSpec {
        argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        cwd: Some(dir.path().to_path_buf()),
        env: Vec::new(),
    };
    guard.tmux.new_window("vowp-large", "job-large", &spec).unwrap();
    assert!(poll_until(|| marker.exists()), "large-output command did not finish");

    let captured = guard.tmux.capture_pane("vowp-large", "job-large", 2000).unwrap();
    assert!(captured.len() > 64 * 1024, "captured only {} bytes", captured.len());
    assert!(captured.contains("line-2999"), "last line was not captured");
    assert!(
        captured.lines().count() >= 1500,
        "captured only {} lines",
        captured.lines().count()
    );
}

#[test]
fn new_window_rejects_duplicate_and_empty_argv() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-dup").unwrap();
    guard.tmux.new_window("vowp-dup", "job-a", &sleep_spec()).unwrap();
    assert!(matches!(
        guard.tmux.new_window("vowp-dup", "job-a", &sleep_spec()),
        Err(TmuxError::WindowExists { .. })
    ));
    assert!(matches!(
        guard.tmux.new_window("vowp-dup", "job-empty", &WindowSpec::default()),
        Err(TmuxError::EmptyCommand)
    ));
}

#[test]
fn new_window_passes_special_characters_verbatim() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("OUT");
    let expected = "a b'c;$(x)";
    let spec = WindowSpec {
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf '%s' \"$1\" > OUT".to_owned(),
            "x".to_owned(),
            expected.to_owned(),
        ],
        cwd: Some(dir.path().to_path_buf()),
        env: Vec::new(),
    };
    guard.tmux.ensure_session("vowp-special").unwrap();
    guard.tmux.new_window("vowp-special", "job-special", &spec).unwrap();

    let mut content = String::new();
    let matched = poll_until(|| {
        if let Ok(text) = fs::read_to_string(&out) {
            content = text;
            content == expected
        } else {
            false
        }
    });
    assert!(matched, "OUT contained {content:?}");
}

#[test]
fn kill_window_kills_only_the_named_window() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-kill").unwrap();
    guard.tmux.new_window("vowp-kill", "job-a", &sleep_spec()).unwrap();
    guard.tmux.new_window("vowp-kill", "job-b", &sleep_spec()).unwrap();
    let windows = guard.tmux.list_windows("vowp-kill").unwrap();
    let survivor_pid = windows
        .iter()
        .find(|window| window.name == "job-b")
        .and_then(|window| window.pane_pid)
        .expect("job-b has a pane pid");

    guard.tmux.kill_window("vowp-kill", "job-a").unwrap();
    assert!(!guard.tmux.window_exists("vowp-kill", "job-a").unwrap());
    assert!(guard.tmux.window_exists("vowp-kill", "job-b").unwrap());
    let alive = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {survivor_pid}"))
        .status()
        .expect("run kill -0");
    assert!(alive.success(), "surviving pane process exited");
}

#[test]
fn kill_window_refuses_untagged_window() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-tagged").unwrap();
    let out = guard.raw(&["new-window", "-d", "-t", "vowp-tagged:", "-n", "foreign", "sleep 30"]);
    assert!(
        out.status.success(),
        "raw new-window failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(matches!(
        guard.tmux.kill_window("vowp-tagged", "foreign"),
        Err(TmuxError::NotTagged { .. })
    ));
    assert!(guard.tmux.window_exists("vowp-tagged", "foreign").unwrap());
}

#[test]
fn kill_window_refuses_foreign_session() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    let out = guard.raw(&["new-session", "-d", "-s", "other", "-n", "foreign", "sleep 30"]);
    assert!(
        out.status.success(),
        "raw new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(matches!(
        guard.tmux.kill_window("other", "foreign"),
        Err(TmuxError::ForeignSession(_))
    ));
    let check = guard.raw(&["has-session", "-t", "=other"]);
    assert!(check.status.success(), "foreign session was touched");
}

#[test]
fn kill_session_refuses_when_untagged_window_present() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-mixed").unwrap();
    let out = guard.raw(&["new-window", "-d", "-t", "vowp-mixed:", "-n", "foreign", "sleep 30"]);
    assert!(
        out.status.success(),
        "raw new-window failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(matches!(
        guard.tmux.kill_session("vowp-mixed"),
        Err(TmuxError::NotTagged { .. })
    ));
    assert!(guard.tmux.session_exists("vowp-mixed").unwrap());
    assert!(guard.tmux.window_exists("vowp-mixed", "foreign").unwrap());
}

#[test]
fn kill_session_kills_tagged_only_session() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-clean").unwrap();
    guard.tmux.new_window("vowp-clean", "job-one", &sleep_spec()).unwrap();
    guard.tmux.kill_session("vowp-clean").unwrap();
    assert!(!guard.tmux.session_exists("vowp-clean").unwrap());
}

#[test]
fn list_windows_missing_session_is_empty() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    let windows = guard.tmux.list_windows("vowp-nope").unwrap();
    assert!(windows.is_empty());
}

#[test]
fn window_name_reuse_after_kill() {
    let Some(guard) = PrivateTmux::new() else {
        return;
    };
    guard.tmux.ensure_session("vowp-reuse").unwrap();
    guard.tmux.new_window("vowp-reuse", "job-reuse", &sleep_spec()).unwrap();
    guard.tmux.kill_window("vowp-reuse", "job-reuse").unwrap();
    assert!(!guard.tmux.window_exists("vowp-reuse", "job-reuse").unwrap());

    guard.tmux.new_window("vowp-reuse", "job-reuse", &sleep_spec()).unwrap();
    let matches: Vec<_> = guard
        .tmux
        .list_windows("vowp-reuse")
        .unwrap()
        .into_iter()
        .filter(|window| window.name == "job-reuse")
        .collect();
    assert_eq!(matches.len(), 1);
    assert!(matches[0].tagged);
}

#[test]
fn kill_server_for_tests_refuses_default_socket() {
    let tmux = Tmux::with_binary(None, "/nonexistent");
    assert!(matches!(tmux.kill_server_for_tests(), Err(TmuxError::Refused(_))));
}

#[test]
fn attach_argv_includes_socket() {
    let plain = Tmux::new(None);
    assert_eq!(
        plain.attach_argv("vowp-demo"),
        vec!["tmux", "attach-session", "-t", "vowp-demo"]
    );
    let private = Tmux::new(Some("vowp-test-deadbeef".to_owned()));
    assert_eq!(
        private.attach_argv("vowp-demo"),
        vec!["tmux", "-L", "vowp-test-deadbeef", "attach-session", "-t", "vowp-demo"]
    );
}
