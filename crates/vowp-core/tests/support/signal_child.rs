//! Shell children with predictable signal dispositions for signal tests.
//!
//! A POSIX shell cannot install a trap for a signal that was already ignored when the
//! shell started, and a blocked signal never reaches the trap. Test runners sometimes
//! inherit such a disposition: a gate launched as a background shell job has SIGINT
//! ignored, for example. A `trap ... INT` child then silently misses SIGINT even though
//! the trap command succeeds and the child reports itself ready.
//!
//! [`shell_command`] starts `sh -c` through `env --default-signal` (GNU coreutils) or a
//! small Perl shim, both of which reset SIGINT/SIGTERM to their default disposition and
//! unblock them before the shell runs. [`wait_for_markers`] polls the files those
//! children append to, so a test observes an asynchronous signal acknowledgment
//! instead of reading once and racing the handler.

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Polls a file written asynchronously by a fake child until every `marker` appears.
///
/// Signal handlers run asynchronously, so reading the file once right after the child
/// is signalled can race under load. Re-read every 20 ms for up to 10 s, then panic
/// with the file's actual contents so the failure names the missing marker.
pub fn wait_for_markers(path: &Path, markers: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = std::fs::read_to_string(path).unwrap_or_default();
        if markers.iter().all(|marker| contents.contains(marker)) {
            return contents;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after 10s waiting for {markers:?} in {}; contents: {contents:?}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Perl shim used when `env` is not GNU coreutils (for example on macOS).
///
/// `$SIG{...} = "DEFAULT"` also overrides a disposition inherited as `SIG_IGN`, unlike
/// a shell's `trap` builtin in a non-interactive shell.
const PERL_RESET: &str = r#"use POSIX qw(SIG_UNBLOCK SIGINT SIGTERM);
$SIG{INT} = "DEFAULT";
$SIG{TERM} = "DEFAULT";
my $set = POSIX::SigSet->new(SIGINT, SIGTERM);
POSIX::sigprocmask(SIG_UNBLOCK, $set);
exec @ARGV or die "exec: $!";"#;

/// Returns a command that runs `script` with `sh -c` and with SIGINT/SIGTERM unblocked
/// and reset to their default dispositions.
///
/// Falls back to a plain `sh -c` when neither helper is available; a normal test runner
/// already starts children with default dispositions, and an inherited ignored SIGINT
/// then shows up as a missing marker instead of a false pass.
pub fn shell_command(script: &str) -> Command {
    match SignalReset::detect() {
        SignalReset::GnuEnv => {
            let mut command = Command::new("env");
            command.arg("--default-signal=INT,TERM").arg("sh").arg("-c").arg(script);
            command
        }
        SignalReset::Perl => {
            let mut command = Command::new("perl");
            command.arg("-e").arg(PERL_RESET).arg("sh").arg("-c").arg(script);
            command
        }
        SignalReset::None => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(script);
            command
        }
    }
}

/// How the test can reset inherited signal state before starting the shell.
#[derive(Clone, Copy)]
enum SignalReset {
    /// GNU `env` supports `--default-signal` and unblocks the listed signals.
    GnuEnv,
    /// Fall back to Perl's POSIX bindings.
    Perl,
    /// No helper is available; run the shell as-is.
    None,
}

impl SignalReset {
    /// Detects once, so every fake child in a test binary uses the same helper.
    fn detect() -> Self {
        static DETECTED: OnceLock<SignalReset> = OnceLock::new();
        *DETECTED.get_or_init(|| {
            if Command::new("env")
                .args(["--default-signal=INT,TERM", "sh", "-c", "exit 0"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                return Self::GnuEnv;
            }
            if Command::new("perl")
                .args(["-e", "exit 0"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                return Self::Perl;
            }
            Self::None
        })
    }
}
