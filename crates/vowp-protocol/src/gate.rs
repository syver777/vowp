//! Gates, their short brief forms, evidence and results.
//! Spec: `docs/design/types.md` §8 and `docs/design/crates/vowp-protocol.md` §4 rule 8.
//!
//! [`Gate`] is externally tagged so the JSON/YAML shape matches the plan:
//! `{"cmd":{"run":"cargo test","expect_exit":0}}`, `{"owns_respected":true}`,
//! `{"review":{"by":"verifier"}}`. [`Gate::short`] is the lossy brief front-matter form.

use std::sync::OnceLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::executor::Role;
use crate::ids::{JobId, RunId};
use crate::job::JobKind;
use crate::report::Verdict;
use crate::time::Timestamp;

/// Fields a report must contain by default.
fn default_report_fields() -> Vec<String> {
    ["status", "branch", "commits", "tests"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Default command-gate timeout in seconds.
pub(crate) fn default_gate_timeout() -> u64 {
    1800
}

/// Default remote for a pushed gate.
fn default_origin() -> String {
    "origin".to_owned()
}

/// Default reviewer role for a review gate.
fn default_verifier_role() -> Role {
    Role::Verifier
}

/// The `report` gate: the job's report front matter must contain `fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReportGate {
    /// Required front-matter fields; defaults to `status`, `branch`, `commits` and `tests`.
    #[serde(default = "default_report_fields")]
    pub fields: Vec<String>,
}

impl Default for ReportGate {
    fn default() -> Self {
        Self {
            fields: default_report_fields(),
        }
    }
}

/// A shell command gate, run with `sh -c` in the job worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CmdGate {
    /// Command line to run.
    pub run: String,
    /// Expected exit code (default 0).
    #[serde(default)]
    pub expect_exit: i32,
    /// Regex that stdout plus stderr must match.
    #[serde(default)]
    pub expect_pattern: Option<String>,
    /// Timeout in seconds (default 1800).
    #[serde(default = "default_gate_timeout")]
    pub timeout_secs: u64,
    /// Working directory relative to the worktree.
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Default for CmdGate {
    fn default() -> Self {
        Self {
            run: String::new(),
            expect_exit: 0,
            expect_pattern: None,
            timeout_secs: default_gate_timeout(),
            cwd: None,
        }
    }
}

impl CmdGate {
    /// Validates a command gate: non-empty command, positive timeout, no file-existence-only
    /// anti-pattern, a non-empty pattern for a nonzero exit, and a compilable `expect_pattern`.
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.run.trim().is_empty() {
            return Err(ProtocolError::InvalidGate("command gate run is empty".to_owned()));
        }
        if self.timeout_secs == 0 {
            return Err(ProtocolError::InvalidGate(
                "command gate timeout must be greater than zero".to_owned(),
            ));
        }
        reject_file_existence_only(&self.run)?;
        reject_nonzero_exit_without_pattern(self)?;
        if let Some(pattern) = &self.expect_pattern {
            Regex::new(pattern)
                .map_err(|err| ProtocolError::InvalidGate(format!("invalid expect_pattern {pattern:?}: {err}")))?;
        }
        Ok(())
    }
}

/// Returns true for commands that only check whether a path exists, or do nothing.
fn is_file_existence_only(run: &str) -> bool {
    static BANNED: OnceLock<Regex> = OnceLock::new();
    BANNED
        .get_or_init(|| {
            // invariant: this literal is a valid regex, exercised by
            // gate_validate_rejects_file_exists_only_cmd.
            Regex::new(r"^\s*(?:test\s+-[fed]\b|\[\s+-[fed]\b|ls\b|stat\b|cat\b|true\b|:)")
                .expect("invariant: anti-pattern regex is valid")
        })
        .is_match(run)
}

/// Rejects a command that only checks whether a path exists, or does nothing.
fn reject_file_existence_only(run: &str) -> Result<(), ProtocolError> {
    if is_file_existence_only(run) {
        return Err(ProtocolError::InvalidGate(format!(
            "anti-pattern: gate must execute and check results: {run}"
        )));
    }
    Ok(())
}

/// Rejects a nonzero exit expectation without a non-empty pattern for the output.
///
/// An empty pattern matches everything, so it is treated exactly like `None`.
fn reject_nonzero_exit_without_pattern(cmd: &CmdGate) -> Result<(), ProtocolError> {
    let has_pattern = cmd
        .expect_pattern
        .as_deref()
        .is_some_and(|pattern| !pattern.trim().is_empty());
    if cmd.expect_exit != 0 && !has_pattern {
        return Err(ProtocolError::InvalidGate(format!(
            "anti-pattern: command gate with expect_exit {} needs a non-empty expect_pattern: {}",
            cmd.expect_exit, cmd.run
        )));
    }
    Ok(())
}

/// The `pushed` gate: the job branch exists on `remote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PushedGate {
    /// Git remote name (default `origin`).
    #[serde(default = "default_origin")]
    pub remote: String,
    /// Branch to check; `None` means the job's branch.
    #[serde(default)]
    pub branch: Option<String>,
}

impl Default for PushedGate {
    fn default() -> Self {
        Self {
            remote: default_origin(),
            branch: None,
        }
    }
}

/// The `review` gate: a verifier role must review the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewGate {
    /// Role that reviews (default `verifier`).
    #[serde(default = "default_verifier_role")]
    pub by: Role,
}

impl Default for ReviewGate {
    fn default() -> Self {
        Self {
            by: default_verifier_role(),
        }
    }
}

/// A deployment gate: run a command, then probe the deployed environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeployGate {
    /// Deployment command.
    pub run: String,
    /// Probe run after deployment.
    pub probe: CmdGate,
    /// Environment name.
    #[serde(default)]
    pub env: Option<String>,
}

impl DeployGate {
    /// Validates the deployment command and its probe.
    ///
    /// The probe error carries the gate's environment name so evidence about a bad probe says
    /// which deployment it belongs to.
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.run.trim().is_empty() {
            return Err(ProtocolError::InvalidGate("deploy gate run is empty".to_owned()));
        }
        reject_file_existence_only(&self.run)?;
        self.probe.validate().map_err(|err| match err {
            ProtocolError::InvalidGate(message) => {
                let env = self.env.as_deref().unwrap_or("default");
                ProtocolError::InvalidGate(format!("deploy probe (env {env}): {message}"))
            }
            other => other,
        })
    }
}

/// A checkable condition on a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Report front-matter fields must be present.
    Report(ReportGate),
    /// A shell command must exit as expected.
    Cmd(CmdGate),
    /// The job's diff must stay inside its ownership globs; `false` disables the gate.
    OwnsRespected(bool),
    /// The job branch must exist on the remote.
    Pushed(PushedGate),
    /// A verifier must review the work.
    Review(ReviewGate),
    /// A project script must exit as expected; evidence records `script`.
    Script(CmdGate),
    /// Deploy and probe an environment.
    Deploy(DeployGate),
    /// Repo-wide secret scan of the job diff; `false` disables the gate.
    SecretScan(bool),
}

/// The kind of a gate, also the evidence tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    /// Report gate.
    Report,
    /// Command gate.
    Cmd,
    /// Ownership gate.
    OwnsRespected,
    /// Pushed-branch gate.
    Pushed,
    /// Review gate.
    Review,
    /// Project-script gate.
    Script,
    /// Deployment gate.
    Deploy,
    /// Secret-scan gate.
    SecretScan,
}

impl Gate {
    /// Returns the gate's kind.
    pub fn kind(&self) -> GateKind {
        match self {
            Self::Report(_) => GateKind::Report,
            Self::Cmd(_) => GateKind::Cmd,
            Self::OwnsRespected(_) => GateKind::OwnsRespected,
            Self::Pushed(_) => GateKind::Pushed,
            Self::Review(_) => GateKind::Review,
            Self::Script(_) => GateKind::Script,
            Self::Deploy(_) => GateKind::Deploy,
            Self::SecretScan(_) => GateKind::SecretScan,
        }
    }

    /// Returns the brief front-matter short form.
    ///
    /// Gates that equal their defaults render as their keyword. Command gates render their
    /// command string, so non-default fields are lost by design; graph YAML export keeps the
    /// full gate.
    pub fn short(&self) -> String {
        match self {
            Self::Report(_) => "report".to_owned(),
            Self::Cmd(gate) | Self::Script(gate) => gate.run.clone(),
            Self::OwnsRespected(_) => "owns_respected".to_owned(),
            Self::Pushed(_) => "pushed".to_owned(),
            Self::Review(_) => "review".to_owned(),
            Self::Deploy(gate) => gate.run.clone(),
            Self::SecretScan(_) => "secret_scan".to_owned(),
        }
    }

    /// Parses a brief front-matter short form.
    ///
    /// Known keywords map to their gate; any other non-empty string becomes a command gate
    /// with the default exit code and timeout. Empty input is [`ProtocolError::InvalidGate`].
    pub fn from_short(s: &str) -> Result<Gate, ProtocolError> {
        let trimmed = s.trim();
        match trimmed {
            "" => Err(ProtocolError::InvalidGate("empty gate".to_owned())),
            "report" => Ok(Self::Report(ReportGate::default())),
            "owns_respected" => Ok(Self::OwnsRespected(true)),
            "review" => Ok(Self::Review(ReviewGate::default())),
            "pushed" => Ok(Self::Pushed(PushedGate::default())),
            "secret_scan" => Ok(Self::SecretScan(true)),
            command => Ok(Self::Cmd(CmdGate {
                run: command.to_owned(),
                ..CmdGate::default()
            })),
        }
    }

    /// Validates the gate definition, including the `loops.md` §4 anti-pattern ban.
    ///
    /// Command, script and deploy gates may not merely check that a file exists (`test -f`,
    /// `[ -e ]`, `ls`, `stat`, `cat`, bare `true` or `:`), because that does not show the work
    /// runs. A command gate that expects a nonzero exit must also assert a non-empty
    /// `expect_pattern`, so a command that fails for an unrelated reason cannot satisfy it. A
    /// deploy gate must run a real command and end with a real probe. Protocol-level checks
    /// (non-empty command, compilable pattern, positive timeout) run first.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Cmd(gate) | Self::Script(gate) => gate.validate(),
            Self::Deploy(gate) => gate.validate(),
            Self::Report(_) | Self::OwnsRespected(_) | Self::Pushed(_) | Self::Review(_) | Self::SecretScan(_) => {
                Ok(())
            }
        }
    }
}

/// The default gate set for a job kind.
///
/// Default sets contain no command gates: crate-specific commands are added per job, and the
/// project's `check_cmds` run only in the merge queue.
pub fn default_gates(kind: JobKind) -> Vec<Gate> {
    let report = Gate::Report(ReportGate::default());
    let owns = Gate::OwnsRespected(true);
    let secret_scan = Gate::SecretScan(true);
    let review = Gate::Review(ReviewGate::default());
    match kind {
        JobKind::Build => vec![report, owns, secret_scan, review],
        JobKind::Fix | JobKind::Test => vec![report, owns, secret_scan],
        JobKind::Docs | JobKind::Spec => vec![report, owns, secret_scan, review],
        JobKind::Verify | JobKind::Research | JobKind::Plan | JobKind::Merge => vec![report],
        JobKind::Decision | JobKind::Promote | JobKind::Release => Vec::new(),
    }
}

/// A secret-scan hit: file, line number and rule name only.
///
/// The matched text is never stored, so this struct deliberately has no value field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretHit {
    /// Repo-relative path of the offending file.
    pub path: String,
    /// One-based line number.
    pub line: u32,
    /// Name of the rule that matched.
    pub rule: String,
}

/// Evidence recorded for one gate evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    /// Report gate evidence: parsed front matter and what was missing or malformed.
    Report {
        /// Parsed front matter, when the file had a valid block.
        front_matter: Option<serde_json::Value>,
        /// Required fields that were absent or empty.
        missing_fields: Vec<String>,
        /// Structural problems found while parsing.
        problems: Vec<String>,
    },
    /// Command gate evidence.
    Cmd {
        /// The command that ran.
        command: String,
        /// Process exit code; `None` when it could not run.
        exit_code: Option<i32>,
        /// Whether the command hit its timeout.
        timed_out: bool,
        /// Last 200 lines of combined output.
        tail: String,
        /// Wall-clock duration.
        duration_ms: u64,
    },
    /// Project-script gate evidence.
    Script {
        /// The command that ran.
        command: String,
        /// Process exit code; `None` when it could not run.
        exit_code: Option<i32>,
        /// Whether the command hit its timeout.
        timed_out: bool,
        /// Last 200 lines of combined output.
        tail: String,
        /// Wall-clock duration.
        duration_ms: u64,
    },
    /// Ownership gate evidence.
    OwnsRespected {
        /// Files changed by the job.
        changed: Vec<String>,
        /// Changed files outside the job's `owns` globs.
        violations: Vec<String>,
    },
    /// Pushed-branch gate evidence.
    Pushed {
        /// Remote checked.
        remote: String,
        /// Branch checked.
        branch: String,
        /// `git ls-remote` output.
        ls_remote: String,
        /// Local branch head, when known.
        local_sha: Option<String>,
    },
    /// Review gate evidence.
    Review {
        /// Verify job that produced the verdict, when one ran.
        verify_job: Option<JobId>,
        /// Verifier verdict.
        verdict: Option<Verdict>,
        /// Verifier findings.
        findings: Vec<String>,
    },
    /// Deployment gate evidence, including its probe.
    Deploy {
        /// Deployment command.
        command: String,
        /// Process exit code; `None` when it could not run.
        exit_code: Option<i32>,
        /// Probe evidence.
        probe: Box<Evidence>,
        /// Deployed version before the change.
        version_before: Option<String>,
        /// Deployed version after the change.
        version_after: Option<String>,
        /// Command that rolls the deployment back.
        rollback_cmd: Option<String>,
    },
    /// Secret-scan gate evidence; only rule names and locations, never values.
    SecretScan {
        /// Number of files scanned.
        files_scanned: u64,
        /// Matches found.
        hits: Vec<SecretHit>,
    },
}

/// The stored result of one gate evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GateResult {
    /// Row id assigned by the store.
    pub id: Option<i64>,
    /// Job the gate belongs to.
    pub job_id: JobId,
    /// Run that evaluated it, when one exists.
    pub run_id: Option<RunId>,
    /// Fix-round attempt number.
    pub attempt: u32,
    /// Gate index in the job's gate list; -1 for repo-wide merge checks.
    pub gate_index: i32,
    /// The gate definition that was evaluated.
    pub gate: Gate,
    /// Whether the gate passed.
    pub passed: bool,
    /// Evidence collected during evaluation.
    pub evidence: Evidence,
    /// Blob reference for output too large to inline.
    pub output_blob: Option<String>,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// When the gate was evaluated.
    pub ts: Timestamp,
}
