//! Shared private-tmux harness for integration tests.
//!
//! Every test runs against its own `tmux -L vowp-test-<hex>` server under its
//! own private `TMUX_TMPDIR`, and the guard kills that server on drop. The
//! constructor asserts that the resolved socket is private, so a test cannot
//! silently fall back to the user's default tmux server.

#![allow(dead_code)]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use vowp_tmux::Tmux;

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private tmux server plus its guard.
pub struct PrivateTmux {
    /// Manager bound to the private socket.
    pub tmux: Tmux,
    /// The private socket name.
    pub socket: String,
    /// Private `TMUX_TMPDIR`.
    pub dir: TempDir,
}

impl PrivateTmux {
    /// Start a guard for a fresh private socket, or return `None` with a
    /// printed notice when tmux is not installed.
    pub fn new() -> Option<Self> {
        let dir = tempfile::tempdir().expect("private tmux dir");
        let socket = socket_name();
        let tmux = Tmux::in_private_dir(socket.clone(), dir.path()).expect("private tmux socket");
        assert_private(&tmux, dir.path());
        if !tmux.has_tmux() {
            eprintln!("SKIP: tmux not installed");
            return None;
        }
        Some(Self { tmux, socket, dir })
    }

    /// Run a raw tmux command against the private server, bypassing the safe
    /// manager (used to install foreign/untagged fixtures).
    pub fn raw(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .env("TMUX_TMPDIR", self.dir.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .expect("run raw tmux")
    }
}

/// Assert that a manager is bound to a private directory and not to tmux's
/// `default` server.
pub fn assert_private(tmux: &Tmux, dir: &std::path::Path) {
    let socket = tmux.socket_name().expect("private socket name");
    assert_ne!(socket, "default", "refusing to test against the default tmux server");
    assert_eq!(tmux.socket_dir(), Some(dir), "unexpected TMUX_TMPDIR");
    assert_eq!(
        tmux.socket_path(),
        Some(dir.join(socket)),
        "tmux socket path must stay under the private directory"
    );
    let path = tmux.socket_path().expect("socket path");
    assert!(!path.ends_with("default"), "private socket path ends in default");
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = self.tmux.kill_server_for_tests();
    }
}

fn socket_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let counter = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ (pid << 32) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("vowp-test-{:08x}", mixed as u32)
}
