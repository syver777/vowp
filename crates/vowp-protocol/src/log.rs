//! Logging without a logging crate: the [`Log`] sink trait, [`Level`], the discarding
//! [`NullLog`], the collecting [`MemoryLog`] used by tests, and the canonical line format.
//! Spec: `docs/design/crates/vowp-protocol.md` §3 and `docs/design/conventions.md` §4.

use std::sync::{Arc, Mutex};

use crate::time::Timestamp;

/// Severity of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Fine-grained diagnostic output.
    Debug,
    /// Normal operational output.
    Info,
    /// Something worth attention that did not stop the work.
    Warn,
    /// A failure.
    Error,
}

impl Level {
    /// Returns the canonical uppercase name (`DEBUG`, `INFO`, `WARN`, `ERROR`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Sink for supervisor/loop log lines. Implementations must be cheap and never panic.
pub trait Log: Send + Sync {
    /// Records one line. `target` names the component (for example `l1` or `job example-job`).
    fn log(&self, level: Level, target: &str, msg: &str);
}

/// A [`Log`] that discards everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullLog;

impl Log for NullLog {
    fn log(&self, _level: Level, _target: &str, _msg: &str) {}
}

/// A [`Log`] that collects `(level, target, message)` tuples for tests.
///
/// Clones share the same buffer.
#[derive(Debug, Clone, Default)]
pub struct MemoryLog {
    lines: Arc<Mutex<Vec<(Level, String, String)>>>,
}

impl MemoryLog {
    /// Creates an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of every recorded line, in order.
    pub fn lines(&self) -> Vec<(Level, String, String)> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns true when any recorded line contains `needle` in its target or message.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(_, target, msg)| target.contains(needle) || msg.contains(needle))
    }
}

impl Log for MemoryLog {
    fn log(&self, level: Level, target: &str, msg: &str) {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((level, target.to_owned(), msg.to_owned()));
    }
}

/// Formats the canonical log line:
/// `2026-11-08T19:25:47.123Z INFO l1 job example-job: running -> gating`.
pub fn format_log_line(ts: Timestamp, level: Level, target: &str, msg: &str) -> String {
    format!("{ts} {} {target} {msg}", level.as_str())
}
