//! Deterministic job gates: report, command, ownership, push, review, deploy and secret scan.
//! Spec: `docs/design/loops.md` §4 and `docs/design/crates/vowp-core.md` §4.
//!
//! Gates are evaluated in the job worktree, in `jobs.gates_json` order. A bad gate definition
//! or an unexpected environment never panics: it produces a failed [`GateResult`] with
//! evidence describing what happened.

use std::path::Path;

use vowp_protocol::{Evidence, Gate, Job, RunId};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

pub mod cmd;
pub mod deploy;
pub mod owns;
pub mod pushed;
pub mod report;
pub mod review;
pub mod runner;
pub mod secret_scan;

pub use runner::{GateRunner, GatesOutcome};

/// Everything one gate evaluation may read.
pub struct GateCtx<'a> {
    /// Injected dependencies (clock, command runner, git, config, log).
    pub deps: &'a Deps,
    /// Job whose gates are being evaluated.
    pub job: &'a Job,
    /// Run that produced the work, when there was one.
    pub run_id: Option<RunId>,
    /// Job worktree the commands and scans run in.
    pub worktree: &'a Path,
    /// Project repository (main checkout or worktree root).
    pub repo: &'a Path,
    /// Store used for gate rows, verifier jobs and their reports.
    pub store: &'a Store,
    /// Fix-round attempt number.
    pub attempt: u32,
}

/// One gate's outcome before it is stamped into a [`GateResult`].
pub(crate) struct Outcome {
    /// Whether the gate passed.
    pub passed: bool,
    /// Evidence describing the evaluation.
    pub evidence: Evidence,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Blob reference when output was spilled.
    pub output_blob: Option<String>,
}

impl Outcome {
    /// Builds a zero-duration outcome with no spilled output.
    pub(crate) fn new(passed: bool, evidence: Evidence) -> Self {
        Self {
            passed,
            evidence,
            duration_ms: 0,
            output_blob: None,
        }
    }
}

/// Reads and parses a report file.
///
/// Used by the `report` gate, the review gate and the loops. A missing file is an
/// [`CoreError::Io`]; malformed front matter is [`vowp_protocol::ProtocolError::FrontMatter`].
pub fn parse_report_file(path: &Path) -> Result<vowp_protocol::Report, CoreError> {
    let text = std::fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(vowp_protocol::Report::parse(&text)?)
}

/// Validates a gate definition, including the anti-pattern ban from `loops.md` §4.
///
/// A thin wrapper over [`Gate::validate`], which is the single source of truth for the rules:
/// command and script gates may not merely check that a file exists (`test -f`, `[ -e ]`,
/// `ls`, `stat`, `cat`, bare `true` or `:`), and a command gate that expects a nonzero exit
/// must also assert a non-empty `expect_pattern`. Deploy gates must run a real command and end
/// with a real probe. The protocol error is mapped to [`CoreError::Invalid`] so callers and
/// tests keep seeing an invalid-input failure.
pub fn validate_gate(gate: &Gate) -> Result<(), CoreError> {
    gate.validate().map_err(|err| CoreError::Invalid(err.to_string()))
}
