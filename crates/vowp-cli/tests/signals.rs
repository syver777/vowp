//! Signal watcher integration tests: the documented `~/.vowp/stop` path still works.
//!
//! Every test uses a temporary node home; no real home directory, tmux server or harness
//! is touched. The SIGTERM/SIGINT path is exercised by spawning the real binary in
//! `tests/up.rs` and `tests/status.rs`.

use std::time::{Duration, Instant};

use vowp_cli::signals::{
    SignalWatcher, clear_stop, clear_stopped, heartbeat_file, mark_stopped, stop_file, stop_requested, stopped,
    stopped_file,
};
use vowp_protocol::{CancelToken, Timestamp};
use vowp_store::NodePaths;

/// Polls `predicate` for up to two seconds.
fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn signals_stop_file_still_works() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    let watcher = SignalWatcher::install(&paths).expect("install watcher");
    let token: CancelToken = watcher.cancel_token();
    assert!(!token.is_cancelled(), "no stop request yet");
    assert!(!stop_requested(&paths));

    std::fs::write(stop_file(&paths), "now").expect("write stop file");
    assert!(
        eventually(|| token.is_cancelled()),
        "the stop file cancels the same token the supervisor runs with"
    );
    assert!(!watcher.cancelled_by_signal(), "the stop file is not a signal");

    assert!(clear_stop(&paths).expect("clear stop"), "the file existed");
    assert!(!stop_requested(&paths));
    assert!(
        !clear_stop(&paths).expect("clear missing stop"),
        "clearing a missing stop file reports false"
    );
}

#[test]
fn heartbeat_file_is_unix_seconds_only() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    // A supervisor tick writes a single Unix-seconds line.
    std::fs::write(heartbeat_file(&paths), "1700000000\n").expect("tick heartbeat");
    let tick = std::fs::read_to_string(heartbeat_file(&paths)).expect("tick heartbeat");
    assert_eq!(tick.lines().count(), 1, "a live heartbeat is one line: {tick:?}");
    assert_eq!(tick.trim().parse::<i64>(), Ok(1_700_000_000));
    mark_stopped(&paths, Timestamp::from_unix_secs(1_700_000_123)).expect("mark stopped");

    let heartbeat = std::fs::read_to_string(heartbeat_file(&paths)).unwrap_or_default();
    assert!(
        heartbeat.is_empty() || heartbeat.trim().parse::<i64>().is_ok(),
        "the heartbeat must hold Unix seconds only, never a `stopped` line: {heartbeat:?}"
    );
    assert!(
        !heartbeat_file(&paths).exists(),
        "a clean stop removes the heartbeat so the watchdog sees it missing, not live"
    );

    let stopped_text = std::fs::read_to_string(stopped_file(&paths)).expect("stopped marker");
    assert_eq!(
        stopped_text.lines().count(),
        1,
        "the stopped marker is one line: {stopped_text:?}"
    );
    assert_eq!(stopped_text.trim().parse::<i64>(), Ok(1_700_000_123));
    assert!(stopped(&paths));
    assert!(
        !paths.root().join(".heartbeat.stopped.tmp").exists(),
        "the temporary marker is renamed into place"
    );
}

#[test]
fn clear_stopped_removes_the_marker() {
    let home = tempfile::tempdir().expect("temp home");
    let paths = NodePaths::new(home.path());
    mark_stopped(&paths, Timestamp::from_unix_secs(1_700_000_000)).expect("mark stopped");

    assert!(stopped(&paths));
    assert!(clear_stopped(&paths).expect("clear stopped"));
    assert!(!stopped(&paths));
    assert!(!clear_stopped(&paths).expect("clear missing stopped"));
}
