//! Graceful shutdown for the supervisor: watch `~/.vowp/stop` and SIGTERM/SIGINT.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §1 (`SIGTERM/SIGINT -> CancelToken`) and
//! `docs/design/architecture.md` §2 (`signal-hook` sets the supervisor's cancellation
//! token). The stop file and the two signals feed the same [`CancelToken`], so `vowp
//! down`, `systemctl --user stop` and Ctrl-C all cancel runner pools the same way.
//!
//! A second signal exits immediately with the conventional status: 130 for SIGINT and
//! 143 for SIGTERM. That gives an operator a way out when a runner refuses to stop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use vowp_protocol::{CancelToken, Timestamp};
use vowp_store::NodePaths;

/// How often the watcher looks for the stop file and checks the signal flag.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Exit status for a second SIGINT (`128 + SIGINT`).
pub const SIGINT_EXIT: i32 = 130;

/// Exit status for a second SIGTERM (`128 + SIGTERM`).
pub const SIGTERM_EXIT: i32 = 143;

/// Returns the path of the stop file (`<home>/stop`).
pub fn stop_file(paths: &NodePaths) -> PathBuf {
    paths.root().join("stop")
}

/// Returns the path of the heartbeat file (`<home>/heartbeat`).
pub fn heartbeat_file(paths: &NodePaths) -> PathBuf {
    paths.root().join("heartbeat")
}

/// Returns the path of the stopped marker (`<home>/heartbeat.stopped`).
///
/// A clean shutdown removes the live heartbeat and writes this sibling file instead, so
/// `vowp status` can say `stopped` without leaving a fresh timestamp that the watchdog
/// would read as a live supervisor.
pub fn stopped_file(paths: &NodePaths) -> PathBuf {
    paths.root().join("heartbeat.stopped")
}

/// Removes a stale stop file, returning whether one existed.
pub fn clear_stop(paths: &NodePaths) -> std::io::Result<bool> {
    let path = stop_file(paths);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns whether a shutdown was requested through the stop file.
pub fn stop_requested(paths: &NodePaths) -> bool {
    stop_file(paths).is_file()
}

/// Records a clean shutdown without corrupting the heartbeat contract.
///
/// The marker at [`stopped_file`] holds the stop time as Unix seconds, one line. The live
/// heartbeat is removed: the documented file is Unix seconds only, and a fresh timestamp
/// left behind would make `status --check` and the watchdog treat the stopped supervisor
/// as alive. A later `vowp up` clears the marker and writes numeric heartbeats again.
pub fn mark_stopped(paths: &NodePaths, at: Timestamp) -> std::io::Result<()> {
    let temp = paths.root().join(".heartbeat.stopped.tmp");
    std::fs::write(&temp, format!("{}\n", at.unix_secs()))?;
    std::fs::rename(&temp, stopped_file(paths))?;
    match std::fs::remove_file(heartbeat_file(paths)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes the stopped marker, returning whether one existed.
pub fn clear_stopped(paths: &NodePaths) -> std::io::Result<bool> {
    let path = stopped_file(paths);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns whether a clean shutdown recorded the stopped marker.
pub fn stopped(paths: &NodePaths) -> bool {
    stopped_file(paths).is_file()
}

/// Registers the shutdown actions for SIGTERM and SIGINT.
///
/// Actions registered before a failure are removed again, so a failed install leaves no
/// half-installed signal handling behind.
fn register_handlers(signalled: &Arc<AtomicBool>) -> std::io::Result<Vec<SigId>> {
    let mut registered = Vec::new();
    for (signal, status) in [(SIGTERM, SIGTERM_EXIT), (SIGINT, SIGINT_EXIT)] {
        match flag::register_conditional_shutdown(signal, status, Arc::clone(signalled)) {
            Ok(id) => registered.push(id),
            Err(error) => {
                unregister_all(&registered);
                return Err(error);
            }
        }
    }
    for signal in [SIGTERM, SIGINT] {
        match flag::register(signal, Arc::clone(signalled)) {
            Ok(id) => registered.push(id),
            Err(error) => {
                unregister_all(&registered);
                return Err(error);
            }
        }
    }
    Ok(registered)
}

/// Removes the signal actions registered by [`register_handlers`].
fn unregister_all(ids: &[SigId]) {
    for id in ids {
        signal_hook::low_level::unregister(*id);
    }
}

/// Watches the stop file and SIGTERM/SIGINT, cancelling a [`CancelToken`] on the first
/// request.
///
/// The watcher stops when dropped, when [`SignalWatcher::stop`] is called, or after it has
/// observed a shutdown request. Cloning the token shares the cancellation flag with every
/// runner. Signal handlers stay installed for the life of the process; a second signal
/// exits immediately even after the watcher thread has finished.
pub struct SignalWatcher {
    cancel: CancelToken,
    signalled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    stop_file: PathBuf,
    handle: Option<JoinHandle<()>>,
}

impl SignalWatcher {
    /// Starts watching `<home>/stop`, SIGTERM and SIGINT.
    ///
    /// Returns an I/O error when the process cannot register the signal handlers; the
    /// supervisor must not run without them.
    pub fn install(paths: &NodePaths) -> std::io::Result<Self> {
        let cancel = CancelToken::default();
        let stop = Arc::new(AtomicBool::new(false));
        let signalled = Arc::new(AtomicBool::new(false));
        let stop_file = stop_file(paths);

        // The conditional-shutdown action must be registered before the flag that arms it:
        // on the first signal it sees `false`, on the second it exits without cleanup.
        let registered = register_handlers(&signalled)?;

        let thread_cancel = cancel.clone();
        let thread_signalled = Arc::clone(&signalled);
        let thread_stop = Arc::clone(&stop);
        let thread_file = stop_file.clone();
        let handle = thread::Builder::new().name("vowp-signals".to_owned()).spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                if thread_signalled.load(Ordering::SeqCst) || thread_file.is_file() {
                    thread_cancel.cancel();
                    break;
                }
                thread::sleep(POLL_INTERVAL);
            }
        });
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                unregister_all(&registered);
                return Err(error);
            }
        };

        Ok(Self {
            cancel,
            signalled,
            stop,
            stop_file,
            handle: Some(handle),
        })
    }

    /// Returns the shared cancellation token.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Returns whether a signal (rather than the stop file) requested shutdown.
    pub fn cancelled_by_signal(&self) -> bool {
        self.signalled.load(Ordering::SeqCst)
    }

    /// Returns the stop file path this watcher observes.
    pub fn stop_file(&self) -> &PathBuf {
        &self.stop_file
    }

    /// Stops the watcher thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Signals the watcher to finish and joins it.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Cancels the token directly (used when `down --force` is not needed).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for SignalWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn stop_file_cancels_watcher() {
        let home = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(home.path());
        let watcher = SignalWatcher::install(&paths).expect("install watcher");
        assert!(!watcher.cancel_token().is_cancelled());

        std::fs::write(stop_file(&paths), "stop").expect("write stop file");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !watcher.cancel_token().is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            watcher.cancel_token().is_cancelled(),
            "the stop file must cancel the supervisor token"
        );
        assert!(!watcher.cancelled_by_signal());
        assert!(clear_stop(&paths).expect("clear stop"));
        assert!(!stop_requested(&paths));
    }

    #[test]
    fn stopped_marker_records_the_stop_time_and_removes_the_heartbeat() {
        let home = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(home.path());
        std::fs::write(heartbeat_file(&paths), "1699999999").expect("tick heartbeat");
        mark_stopped(&paths, Timestamp::from_unix_secs(1_700_000_000)).expect("mark stopped");

        assert!(!heartbeat_file(&paths).exists());
        let text = std::fs::read_to_string(stopped_file(&paths)).expect("stopped marker");
        assert_eq!(text, "1700000000\n");
        assert!(stopped(&paths));
        assert!(clear_stopped(&paths).expect("clear stopped"));
        assert!(!stopped(&paths));
        assert!(!clear_stopped(&paths).expect("clear missing stopped"));
    }
}
