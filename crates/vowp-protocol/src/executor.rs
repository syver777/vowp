//! Harnesses, executors, roles and model-ladder profiles. Spec: `docs/design/types.md` §3
//! and `docs/design/crates/vowp-protocol.md` §4 rules 5–6.
//!
//! An executor is written `harness:model[@effort][#variant]`; that exact canonical string is
//! also its Display and JSON form. `@effort` is only valid for `claude-code` and `#variant`
//! only for `opencode`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::NodeName;
use crate::time::Timestamp;

/// A harness that runs a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub enum HarnessKind {
    /// Claude Code (`claude`).
    #[serde(rename = "claude-code")]
    ClaudeCode,
    /// opencode.
    #[serde(rename = "opencode")]
    Opencode,
    /// The generic mapping-file CLI adapter.
    #[serde(rename = "cli")]
    Cli,
    /// An Agent Client Protocol (ACP) server.
    #[serde(rename = "acp")]
    Acp,
    /// Events VOWP itself writes (system events, shell and gate runs).
    #[serde(rename = "vowp")]
    Vowp,
}

impl HarnessKind {
    /// Returns the canonical lowercase harness name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Opencode => "opencode",
            Self::Cli => "cli",
            Self::Acp => "acp",
            Self::Vowp => "vowp",
        }
    }

    /// Parses a canonical harness name.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        match s {
            "claude-code" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::Opencode),
            "cli" => Ok(Self::Cli),
            "acp" => Ok(Self::Acp),
            "vowp" => Ok(Self::Vowp),
            other => Err(ProtocolError::InvalidExecutor(
                other.to_owned(),
                "unknown harness".to_owned(),
            )),
        }
    }
}

/// One concrete way to run a model: `harness:model[@effort][#variant]`.
///
/// Examples:
/// * `claude-code:model-a@medium`
/// * `opencode:provider/model-b#max`
/// * `cli:codex`
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct Executor {
    /// Harness that runs the model.
    pub harness: HarnessKind,
    /// Model name as the harness knows it.
    pub model: String,
    /// `claude-code` only: effort passed as `--effort`.
    pub effort: Option<String>,
    /// `opencode` only: variant passed as part of `--model`.
    pub variant: Option<String>,
}

impl Executor {
    /// Parses the canonical executor string.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        let invalid = |reason: &str| ProtocolError::InvalidExecutor(s.to_owned(), reason.to_owned());

        let (harness, rest) = s
            .split_once(':')
            .ok_or_else(|| invalid("missing ':' between harness and model"))?;
        let harness = HarnessKind::parse(harness).map_err(|_| invalid("unknown harness"))?;

        let (model_part, variant) = split_at_most_once(rest, '#').ok_or_else(|| invalid("more than one '#'"))?;
        let (model, effort) = split_at_most_once(model_part, '@').ok_or_else(|| invalid("more than one '@'"))?;

        if model.is_empty() {
            return Err(invalid("empty model"));
        }
        if !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
        {
            return Err(invalid("model contains a character outside [A-Za-z0-9._:/-]"));
        }
        if let Some(effort) = effort {
            if !is_modifier(effort) {
                return Err(invalid("effort must match [a-z0-9_-]+"));
            }
            if harness != HarnessKind::ClaudeCode {
                return Err(invalid("'@effort' is only valid for claude-code"));
            }
        }
        if let Some(variant) = variant {
            if !is_modifier(variant) {
                return Err(invalid("variant must match [a-z0-9_-]+"));
            }
            if harness != HarnessKind::Opencode {
                return Err(invalid("'#variant' is only valid for opencode"));
            }
        }

        Ok(Self {
            harness,
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            variant: variant.map(str::to_owned),
        })
    }

    /// Checks the executor's own field invariants (useful after struct-literal construction).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let display = self.to_string();
        let invalid = |reason: &str| ProtocolError::InvalidExecutor(display.clone(), reason.to_owned());

        if self.model.is_empty() {
            return Err(invalid("empty model"));
        }
        if !self
            .model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
        {
            return Err(invalid("model contains a character outside [A-Za-z0-9._:/-]"));
        }
        match (&self.effort, &self.variant) {
            (Some(effort), _) if !is_modifier(effort) => Err(invalid("effort must match [a-z0-9_-]+")),
            (_, Some(variant)) if !is_modifier(variant) => Err(invalid("variant must match [a-z0-9_-]+")),
            (Some(_), _) if self.harness != HarnessKind::ClaudeCode => {
                Err(invalid("'@effort' is only valid for claude-code"))
            }
            (_, Some(_)) if self.harness != HarnessKind::Opencode => {
                Err(invalid("'#variant' is only valid for opencode"))
            }
            _ => Ok(()),
        }
    }

    /// Returns the provider key used for limits: `claude`, the opencode model prefix, or the
    /// CLI entry name.
    pub fn provider(&self) -> String {
        match self.harness {
            HarnessKind::ClaudeCode => "claude".to_owned(),
            HarnessKind::Opencode => self.model.split('/').next().unwrap_or(self.model.as_str()).to_owned(),
            HarnessKind::Cli | HarnessKind::Acp | HarnessKind::Vowp => self.model.clone(),
        }
    }

    /// Returns the model argument passed to the harness.
    pub fn model_arg(&self) -> String {
        match (self.harness, &self.variant) {
            (HarnessKind::Opencode, Some(variant)) => format!("{}#{variant}", self.model),
            _ => self.model.clone(),
        }
    }
}

impl fmt::Display for Executor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.harness.as_str(), self.model)?;
        if let Some(effort) = &self.effort {
            write!(f, "@{effort}")?;
        }
        if let Some(variant) = &self.variant {
            write!(f, "#{variant}")?;
        }
        Ok(())
    }
}

impl FromStr for Executor {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Executor {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Executor> for String {
    fn from(value: Executor) -> Self {
        value.to_string()
    }
}

/// Splits on the first `sep`, returning `None` when it occurs more than once.
fn split_at_most_once(s: &str, sep: char) -> Option<(&str, Option<&str>)> {
    let mut parts = s.split(sep);
    let first = parts.next()?;
    let second = parts.next();
    if parts.next().is_some() {
        return None;
    }
    Some((first, second))
}

/// Returns true for a non-empty `[a-z0-9_-]+` modifier such as an effort or variant.
fn is_modifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_harness_round_trips_and_has_no_modifiers() {
        assert_eq!(HarnessKind::parse("acp").expect("acp is a harness"), HarnessKind::Acp);
        assert_eq!(HarnessKind::Acp.as_str(), "acp");
        assert_eq!(
            serde_json::to_value(HarnessKind::Acp).expect("harness serialises"),
            serde_json::Value::String("acp".to_owned())
        );

        let executor = Executor::parse("acp:claude-code-acp").expect("ACP executor parses");
        assert_eq!(executor.harness, HarnessKind::Acp);
        assert_eq!(executor.model, "claude-code-acp");
        assert_eq!(executor.provider(), "claude-code-acp");
        assert_eq!(executor.model_arg(), "claude-code-acp");
        assert_eq!(executor.to_string(), "acp:claude-code-acp");
        assert!(executor.validate().is_ok());

        // Effort and variant remain claude-code/opencode modifiers only.
        assert!(Executor::parse("acp:m@high").is_err());
        assert!(Executor::parse("acp:m#max").is_err());
    }
}

/// Where an executor runs: the executor plus an optional node (`None` means this node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutorRef {
    /// The executor to run.
    pub executor: Executor,
    /// The node to run it on; `None` means the local node.
    pub node: Option<NodeName>,
}

/// The role a profile and its runs serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Plans the job graph.
    Planner,
    /// Diagnoses stalled or failed jobs.
    Keeper,
    /// Does job work.
    Worker,
    /// Reviews finished work.
    Verifier,
}

/// What to do when the rung's provider reports a quota limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnQuota {
    /// Move to the next rung and probe the earlier rung again after its reset time.
    NextThenProbeBack,
    /// Move to the next rung and stay there.
    Next,
    /// Park the jobs and raise an inbox item.
    Park,
}

/// Returns the default [`OnQuota`] value for serde.
fn default_on_quota() -> OnQuota {
    OnQuota::NextThenProbeBack
}

/// A role's model ladder. Rung 0 is preferred; duplicates are rejected by [`Profile::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    /// Ordered ladder of executors; must be non-empty.
    pub ladder: Vec<Executor>,
    /// What to do when a rung's provider reports a quota limit.
    #[serde(default = "default_on_quota")]
    pub on_quota: OnQuota,
    /// Run on that node instead of the project's home node.
    #[serde(default)]
    pub node: Option<NodeName>,
}

impl Profile {
    /// Validates the ladder: non-empty, no duplicates, every executor valid, node slug valid.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.ladder.is_empty() {
            return Err(ProtocolError::Config("profile ladder is empty".to_owned()));
        }
        let mut seen: HashSet<&Executor> = HashSet::new();
        for executor in &self.ladder {
            executor.validate()?;
            if !seen.insert(executor) {
                return Err(ProtocolError::Config(format!(
                    "duplicate executor in profile ladder: {executor}"
                )));
            }
        }
        if let Some(node) = &self.node {
            NodeName::parse(node.as_str())?;
        }
        Ok(())
    }

    /// Picks the first rung whose provider is not limited at `now`.
    ///
    /// `limited` maps a provider to its reset time; a missing key means not limited, `None`
    /// means limited indefinitely, and `Some(t)` means limited until `t` (exclusive).
    /// Returns the rung index and executor, or `None` when every rung is limited.
    pub fn pick(&self, limited: &HashMap<String, Option<Timestamp>>, now: Timestamp) -> Option<(usize, &Executor)> {
        for (index, executor) in self.ladder.iter().enumerate() {
            let blocked = match limited.get(&executor.provider()) {
                Some(None) => true,
                Some(Some(resets_at)) => *resets_at > now,
                None => false,
            };
            if !blocked {
                return Some((index, executor));
            }
        }
        None
    }
}
