//! Product-layer safety: destructive operations only touch a server that
//! `vowp up` created and recorded, and never the default tmux server.
//!
//! Every test uses a temporary node home and a private `tmux -L` socket under
//! that home's own `TMUX_TMPDIR`.

mod common;

use std::path::Path;

use vowp_tmux::{Tmux, TmuxError, WindowManager, WindowSpec, socket_name_for_home};

fn sleep_spec() -> WindowSpec {
    WindowSpec {
        argv: vec!["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
        ..WindowSpec::default()
    }
}

fn skip_without_tmux(tmux: &Tmux) -> bool {
    if tmux.has_tmux() {
        false
    } else {
        eprintln!("SKIP: tmux not installed");
        true
    }
}

fn record_path(home: &Path) -> std::path::PathBuf {
    home.join("tmux-server")
}

#[test]
fn for_home_uses_a_stable_private_socket_and_dir() {
    let home = tempfile::tempdir().expect("home");
    let first = Tmux::for_home_with_socket(home.path(), None);
    let second = Tmux::for_home_with_socket(home.path(), None);

    assert_eq!(first.socket_name(), second.socket_name());
    assert_eq!(first.socket_name(), Some(socket_name_for_home(home.path()).as_str()));
    let expected_dir = std::fs::canonicalize(home.path()).expect("canonical home").join("tmux");
    assert_eq!(first.socket_dir(), Some(expected_dir.as_path()));
    let socket_dir = first.socket_dir().expect("private dir").to_path_buf();
    assert_eq!(
        first.socket_path(),
        Some(socket_dir.join(first.socket_name().expect("socket")))
    );
    common::assert_private(&first, &socket_dir);
}

#[test]
fn process_scope_is_reused_by_the_attach_constructor() {
    let home = tempfile::tempdir().expect("home");
    let scoped = Tmux::for_home_with_socket(home.path(), None);
    let attach = Tmux::new(None);

    assert_eq!(attach.socket_name(), scoped.socket_name());
    assert_eq!(attach.socket_dir(), scoped.socket_dir());
    let dir = scoped.socket_dir().expect("private dir");
    let argv = attach.attach_argv("vowp-demo:board");
    assert_eq!(
        argv,
        vec![
            "env".to_owned(),
            "-u".to_owned(),
            "TMUX".to_owned(),
            "-u".to_owned(),
            "TMUX_PANE".to_owned(),
            format!("TMUX_TMPDIR={}", dir.display()),
            "tmux".to_owned(),
            "-L".to_owned(),
            scoped.socket_name().expect("socket").to_owned(),
            "attach-session".to_owned(),
            "-t".to_owned(),
            "vowp-demo:board".to_owned(),
        ]
    );
    assert!(matches!(scoped.kill_server_for_tests(), Err(TmuxError::Refused(_))));
}

#[test]
fn attach_argv_executes_with_the_private_tmux_tmpdir() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    let tools = tempfile::tempdir().expect("tools");
    let log = tools.path().join("tmux.log");
    let script = tools.path().join("tmux");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"${TMUX-unset}\" \"${TMUX_PANE-unset}\" \"${TMUX_TMPDIR-}\" \"$*\" >> \"$FAKE_TMUX_LOG\"\n",
    )
    .expect("write fake tmux");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let argv = tmux.attach_argv("vowp-demo:board");
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env("PATH", path)
        .env("FAKE_TMUX_LOG", &log)
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .env("TMUX_PANE", "%1")
        .env_remove("TMUX_TMPDIR")
        .status()
        .expect("run attach argv");
    assert!(status.success(), "attach argv failed: {argv:?}");

    let text = std::fs::read_to_string(&log).expect("fake tmux log");
    let mut parts = text.trim().splitn(4, '|');
    let tmux_env = parts.next().unwrap_or_default();
    let pane_env = parts.next().unwrap_or_default();
    let tmpdir = parts.next().unwrap_or_default();
    let args = parts.next().unwrap_or_default();
    assert_eq!(tmux_env, "unset");
    assert_eq!(pane_env, "unset");
    assert_eq!(Path::new(tmpdir), tmux.socket_dir().expect("private dir"));
    assert!(args.contains("-L vowp-"), "attach argv: {args:?}");
    assert!(args.contains("attach-session"), "attach argv: {args:?}");
}

#[test]
fn ensure_session_records_the_socket_and_server_pid() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-record").expect("ensure session");
    let record = std::fs::read_to_string(record_path(home.path())).expect("record");
    assert!(record.contains(&format!("socket={}\n", tmux.socket_name().expect("socket"))));
    let pid = tmux.server_pid().expect("pid").expect("running server");
    assert!(record.contains(&format!("pid={pid}\n")), "record: {record}");
}

#[test]
fn kill_window_refuses_when_the_server_was_not_recorded() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-unrecorded").expect("session");
    tmux.new_window("vowp-unrecorded", "job-a", &sleep_spec())
        .expect("window");
    std::fs::remove_file(record_path(home.path())).expect("remove record");

    assert!(matches!(
        tmux.list_windows("vowp-unrecorded"),
        Err(TmuxError::Refused(_))
    ));
    assert!(matches!(
        tmux.kill_window("vowp-unrecorded", "job-a"),
        Err(TmuxError::Refused(_))
    ));
    assert!(
        tmux.window_exists("vowp-unrecorded", "job-a").expect("window exists"),
        "an unrecorded server must not lose a window"
    );
}

#[test]
fn kill_window_refuses_when_the_recorded_pid_changed() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-replaced").expect("session");
    tmux.new_window("vowp-replaced", "job-a", &sleep_spec())
        .expect("window");
    let socket = tmux.socket_name().expect("socket").to_owned();
    std::fs::write(record_path(home.path()), format!("socket={socket}\npid=1\n")).expect("record");

    assert!(matches!(
        tmux.kill_window("vowp-replaced", "job-a"),
        Err(TmuxError::Refused(_))
    ));
    assert!(tmux.window_exists("vowp-replaced", "job-a").expect("window exists"));
}

#[test]
fn kill_window_refuses_when_the_recorded_socket_changed() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-other-socket").expect("session");
    tmux.new_window("vowp-other-socket", "job-a", &sleep_spec())
        .expect("window");
    let pid = tmux.server_pid().expect("pid").expect("running server");
    std::fs::write(
        record_path(home.path()),
        format!("socket=vowp-somewhere-else\npid={pid}\n"),
    )
    .expect("record");

    assert!(matches!(
        tmux.kill_window("vowp-other-socket", "job-a"),
        Err(TmuxError::Refused(_))
    ));
    assert!(tmux.window_exists("vowp-other-socket", "job-a").expect("window exists"));
}

#[test]
fn kill_session_refuses_when_the_server_was_not_recorded() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-unrecorded-session").expect("session");
    std::fs::remove_file(record_path(home.path())).expect("remove record");

    assert!(matches!(
        tmux.kill_session("vowp-unrecorded-session"),
        Err(TmuxError::Refused(_))
    ));
    assert!(
        tmux.session_exists("vowp-unrecorded-session").expect("session exists"),
        "an unrecorded server must not lose a session"
    );
}

#[test]
fn recorded_server_allows_window_and_session_cleanup() {
    let home = tempfile::tempdir().expect("home");
    let tmux = Tmux::for_home_with_socket(home.path(), None);
    if skip_without_tmux(&tmux) {
        return;
    }
    let guard = Tmux::in_private_dir(
        tmux.socket_name().expect("socket").to_owned(),
        tmux.socket_dir().expect("dir"),
    )
    .expect("guard");
    let _cleanup = Cleanup(guard);

    tmux.ensure_session("vowp-recorded").expect("session");
    tmux.new_window("vowp-recorded", "job-a", &sleep_spec())
        .expect("window");
    tmux.kill_window("vowp-recorded", "job-a").expect("kill window");
    assert!(!tmux.window_exists("vowp-recorded", "job-a").expect("window gone"));
    tmux.kill_session("vowp-recorded").expect("kill session");
    assert!(!tmux.session_exists("vowp-recorded").expect("session gone"));
}

#[test]
fn default_manager_refuses_every_server_command_without_spawning() {
    // `/nonexistent` would fail with NotInstalled if a command were spawned;
    // Refused proves the guard runs first.
    let tmux = Tmux::with_binary(None, "/nonexistent");
    assert!(matches!(tmux.list_windows("vowp-demo"), Err(TmuxError::Refused(_))));
    assert!(matches!(tmux.kill_session("vowp-demo"), Err(TmuxError::Refused(_))));
    assert!(matches!(
        tmux.kill_window("vowp-demo", "job-a"),
        Err(TmuxError::Refused(_))
    ));
    assert!(matches!(tmux.kill_server_for_tests(), Err(TmuxError::Refused(_))));
}

#[test]
fn explicit_default_socket_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    assert!(matches!(
        Tmux::in_private_dir("default", dir.path()),
        Err(TmuxError::Refused(_))
    ));

    let tmux = Tmux::new(Some("default".to_owned()));
    assert!(matches!(tmux.list_windows("vowp-demo"), Err(TmuxError::Refused(_))));
    assert!(matches!(tmux.kill_server_for_tests(), Err(TmuxError::Refused(_))));
}

struct Cleanup(Tmux);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.kill_server_for_tests();
    }
}
