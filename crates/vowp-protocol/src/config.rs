//! `vowp.toml` and the node-level config file.
//! Spec: `docs/design/types.md` §11, `docs/design/architecture.md` §5.1–5.3 and
//! `docs/design/crates/vowp-protocol.md` §4 rule 11.
//!
//! Parsing is strict: every config struct denies unknown fields. The one exception is
//! [`LoopConfig`], whose flattened [`LoopBounds`] cannot be combined with
//! `deny_unknown_fields`; [`VowpConfig::from_toml`] therefore checks loop keys against the
//! raw TOML table before decoding. Validation covers the rules in architecture.md §5.3.

use std::collections::{BTreeMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::executor::{Executor, HarnessKind, Profile};
use crate::gate::Gate;
use crate::ids::{NodeName, ProjectName};
use crate::loops::{ActiveState, LoopBounds, MissedPolicy, ScheduleAction};

/// The keys a `[loops.<name>]` table may contain.
const LOOP_KEYS: [&str; 11] = [
    "interval_secs",
    "state",
    "max_continues",
    "stall_minutes",
    "silent_minutes",
    "max_hang_restarts",
    "same_action_max",
    "probe_interval_minutes",
    "planner_min_gap_minutes",
    "idle_gap_minutes",
    "checkpoint_minutes",
];

/// The nine loop names that may appear under `[loops]`.
const KNOWN_LOOPS: [&str; 9] = [
    "l1-supervisor",
    "l2-worker",
    "l3-verify",
    "l4-planner",
    "l5-keeper",
    "l6-failover",
    "l7-inbox",
    "l8-watchdog",
    "l9-schedules",
];

/// The role-profile names that must exist after merging project and node config.
const REQUIRED_PROFILES: [&str; 4] = ["planner", "keeper", "worker", "verifier"];

/// Returns the default node name.
fn default_node_name() -> NodeName {
    // invariant: "local" is a valid slug.
    NodeName::parse("local").expect("invariant: 'local' is a valid node slug")
}

/// Returns the default worker slot count.
fn default_max_workers() -> u32 {
    4
}

/// Returns the default verifier slot count.
fn default_max_verifiers() -> u32 {
    2
}

/// Returns the default supervisor tick.
fn default_tick_secs() -> u64 {
    60
}

/// Returns the default project remote.
fn default_remote() -> String {
    "origin".to_owned()
}

/// Returns the default target branch.
fn default_default_branch() -> String {
    "main".to_owned()
}

/// Returns the default lock-file list.
fn default_lock_files() -> Vec<String> {
    vec!["Cargo.lock".to_owned()]
}

/// Returns the default always-allowed path list.
fn default_always_allowed() -> Vec<String> {
    vec!["Cargo.lock".to_owned()]
}

/// Returns `true` for serde defaults that are on.
fn default_true() -> bool {
    true
}

/// Returns the default provider concurrency.
fn default_max_concurrent() -> u32 {
    4
}

/// Returns the default capsule writer interval.
fn default_min_interval_secs() -> u64 {
    120
}

/// Returns the default capsule input cap.
fn default_max_input_chars() -> u64 {
    200_000
}

/// Returns the default number of tail turns seeded into a capsule prompt.
fn default_tail_turns() -> u32 {
    2
}

/// Returns the default merge-queue timeout.
fn default_merge_timeout_secs() -> u64 {
    2700
}

/// Node-wide settings from `[node]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Name of this node; matches `nodes.name`.
    #[serde(default = "default_node_name")]
    pub name: NodeName,
    /// Concurrent worker slots.
    #[serde(default = "default_max_workers")]
    pub max_workers: u32,
    /// Concurrent verifier slots.
    #[serde(default = "default_max_verifiers")]
    pub max_verifiers: u32,
    /// L1 supervisor tick in seconds.
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    /// Extra allowed working directories, on top of the project path and worktree root.
    #[serde(default)]
    pub cwd_allow: Vec<String>,
    /// `VOWP_HOME` override.
    #[serde(default)]
    pub home: Option<String>,
    /// Local UTC offset used to read wall-clock limit resets such as `resets 11pm`.
    #[serde(default)]
    pub tz_offset_minutes: i32,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: default_node_name(),
            max_workers: default_max_workers(),
            max_verifiers: default_max_verifiers(),
            tick_secs: default_tick_secs(),
            cwd_allow: Vec::new(),
            home: None,
            tz_offset_minutes: 0,
        }
    }
}

/// Project settings from the required `[project]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Project name slug.
    pub name: ProjectName,
    /// Main checkout; defaults to the directory containing `vowp.toml`.
    #[serde(default)]
    pub path: Option<String>,
    /// Git remote name.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Branch the merge queue targets.
    #[serde(default = "default_default_branch")]
    pub default_branch: String,
    /// Repo-wide checks run only by the merge queue.
    #[serde(default)]
    pub check_cmds: Vec<String>,
    /// Files regenerated, not merged, on rebase conflicts.
    #[serde(default = "default_lock_files")]
    pub lock_files: Vec<String>,
    /// Worktree root; defaults to the documented per-node location.
    #[serde(default)]
    pub worktree_root: Option<String>,
    /// Paths any job may touch.
    #[serde(default = "default_always_allowed")]
    pub always_allowed: Vec<String>,
    /// Whether the merge queue pushes the default branch.
    #[serde(default = "default_true")]
    pub push: bool,
}

/// What to do when a provider reports exhausted credits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnCreditsExhausted {
    /// Park affected jobs and raise an inbox item; never switch silently.
    #[default]
    Park,
}

/// One model price entry, in USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    /// Input price per million tokens.
    pub input_per_m: f64,
    /// Output price per million tokens.
    pub output_per_m: f64,
    /// Cache-read price per million tokens.
    #[serde(default)]
    pub cache_read_per_m: Option<f64>,
    /// Cache-write price per million tokens.
    #[serde(default)]
    pub cache_write_per_m: Option<f64>,
}

/// Provider settings from `[providers.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Concurrent runs allowed for this provider.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Credits-exhaustion policy; v0.1 only parks.
    #[serde(default)]
    pub on_credits_exhausted: OnCreditsExhausted,
    /// Price table keyed by model name.
    #[serde(default)]
    pub prices: BTreeMap<String, PriceEntry>,
}

/// Per-harness overrides and generic `cli` entries from `[harnesses.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Binary to run; defaults to a PATH lookup of the harness name.
    #[serde(default)]
    pub binary: Option<String>,
    /// Extra flags appended after the adapter's own argv.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Environment variable NAMES to forward; never values.
    #[serde(default)]
    pub env_pass: Vec<String>,
    /// Permission mode passed to the harness.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Mapping file for the generic `cli` adapter.
    #[serde(default)]
    pub mapping: Option<String>,
}

/// How a promotion is approved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// A human must approve.
    #[default]
    Human,
    /// The configured guards and policy decide.
    Policy,
}

/// One Work-to-Production environment from `[env.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    /// Checkout directory for this environment.
    pub checkout: String,
    /// Commands run in the checkout by the `build` guard; empty means the
    /// `[merge].check_cmds` / `[project].check_cmds` fallback.
    #[serde(default)]
    pub build: Vec<String>,
    /// Environment-variable **names** to resolve from the vault and pass to the
    /// serving-bank, deploy, probe and rollback commands; never values.
    #[serde(default)]
    pub env_pass: Vec<String>,
    /// Deployment commands.
    #[serde(default)]
    pub deploy: Vec<String>,
    /// Guards that must pass before deployment.
    #[serde(default)]
    pub guards: Vec<String>,
    /// Who approves a promotion.
    #[serde(default)]
    pub approval: Approval,
    /// Command that banks the live state before deploying.
    #[serde(default)]
    pub serving_bank: Option<String>,
    /// Command that probes the environment after deploying.
    #[serde(default)]
    pub probe: Option<String>,
    /// Command that rolls the deployment back.
    #[serde(default)]
    pub rollback: Option<String>,
}

/// Per-loop overrides from `[loops.<name>]`.
///
/// Spec: `types.md` §11. This is the only config struct without `deny_unknown_fields`
/// because serde cannot combine it with the flattened [`LoopBounds`]; the parser checks the
/// keys against [`LOOP_KEYS`] instead.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LoopConfig {
    /// Tick interval override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// Whether the loop is active or paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ActiveState>,
    /// Numeric bounds shared by the loops.
    #[serde(flatten)]
    pub bounds: LoopBounds,
}

/// One `[[schedules]]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// Unique schedule name.
    pub name: String,
    /// Five-field cron expression; exactly one of `cron`/`every` is required.
    #[serde(default)]
    pub cron: Option<String>,
    /// Interval such as `30m`; exactly one of `cron`/`every` is required.
    #[serde(default)]
    pub every: Option<String>,
    /// `UTC` or a fixed offset such as `+02:00`.
    #[serde(default)]
    pub tz: Option<String>,
    /// What to do when occurrences were missed.
    #[serde(default)]
    pub missed: Option<MissedPolicy>,
    /// What to do when due.
    pub action: ScheduleAction,
}

/// Capsule-writer settings from `[capsule]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapsuleConfig {
    /// Executor for capsule turns; defaults to the last rung of the `keeper` profile.
    #[serde(default)]
    pub executor: Option<Executor>,
    /// Minimum seconds between automatic capsule turns.
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u64,
    /// Maximum input characters fed to a capsule turn.
    #[serde(default = "default_max_input_chars")]
    pub max_input_chars: u64,
    /// Number of recent turns seeded into the capsule prompt.
    #[serde(default = "default_tail_turns")]
    pub tail_turns: u32,
}

impl Default for CapsuleConfig {
    fn default() -> Self {
        Self {
            executor: None,
            min_interval_secs: default_min_interval_secs(),
            max_input_chars: default_max_input_chars(),
            tail_turns: default_tail_turns(),
        }
    }
}

/// Merge-queue settings from `[merge]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeConfig {
    /// Rebase the job branch onto the target before merging.
    #[serde(default = "default_true")]
    pub rebase: bool,
    /// Repo-wide checks; defaults to `project.check_cmds`.
    #[serde(default)]
    pub check_cmds: Vec<String>,
    /// Overall timeout for one merge in seconds.
    #[serde(default = "default_merge_timeout_secs")]
    pub timeout_secs: u64,
    /// Run the secret scan over the job diff before pushing.
    #[serde(default = "default_true")]
    pub secret_scan: bool,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            rebase: true,
            check_cmds: Vec::new(),
            timeout_secs: default_merge_timeout_secs(),
            secret_scan: true,
        }
    }
}

/// The project configuration file, `vowp.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VowpConfig {
    /// Node-wide settings.
    #[serde(default)]
    pub node: NodeConfig,
    /// Project identity and merge target; required.
    pub project: ProjectConfig,
    /// Role profiles and custom ladders.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Provider concurrency and price tables.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Per-harness overrides.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessConfig>,
    /// Work-to-Production environments.
    #[serde(default)]
    pub env: BTreeMap<String, EnvConfig>,
    /// Named gate sets.
    #[serde(default)]
    pub gates: BTreeMap<String, Vec<Gate>>,
    /// Loop overrides.
    #[serde(default)]
    pub loops: BTreeMap<String, LoopConfig>,
    /// L9 schedules.
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
    /// Capsule-writer settings.
    #[serde(default)]
    pub capsule: CapsuleConfig,
    /// Merge-queue settings.
    #[serde(default)]
    pub merge: MergeConfig,
}

impl VowpConfig {
    /// Parses and validates a `vowp.toml` document.
    ///
    /// Unknown keys are rejected with the key path, including keys inside a `[loops.*]`
    /// table, which serde cannot reject because of the flattened bounds.
    pub fn from_toml(s: &str) -> Result<Self, ProtocolError> {
        let raw: toml::Table = s
            .parse()
            .map_err(|err| ProtocolError::Config(format!("invalid TOML: {err}")))?;
        reject_unknown_loop_keys(&raw)?;
        let config: Self = toml::from_str(s).map_err(|err| ProtocolError::Config(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Validates the configuration against `architecture.md` §5.3.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.validate_node()?;
        self.validate_profiles()?;
        self.validate_providers()?;
        self.validate_schedules()?;
        self.validate_envs()?;
        self.validate_gate_sets()?;
        self.validate_loops()?;
        self.validate_globs()?;
        Ok(())
    }

    /// Returns a small valid configuration for tests and fixtures.
    pub fn with_defaults_for_tests() -> Self {
        let executor = Executor {
            harness: HarnessKind::Opencode,
            model: "deepseek/deepseek-flash".to_owned(),
            effort: None,
            variant: None,
        };
        let profile = || Profile {
            ladder: vec![executor.clone()],
            on_quota: crate::executor::OnQuota::NextThenProbeBack,
            node: None,
        };
        let mut profiles = BTreeMap::new();
        for role in REQUIRED_PROFILES {
            profiles.insert(role.to_owned(), profile());
        }
        // invariant: "test" is a valid project slug.
        let project = ProjectName::parse("test").expect("invariant: 'test' is a valid project slug");
        Self {
            node: NodeConfig::default(),
            project: ProjectConfig {
                name: project,
                path: None,
                remote: default_remote(),
                default_branch: default_default_branch(),
                check_cmds: Vec::new(),
                lock_files: default_lock_files(),
                worktree_root: None,
                always_allowed: default_always_allowed(),
                push: true,
            },
            profiles,
            providers: BTreeMap::new(),
            harnesses: BTreeMap::new(),
            env: BTreeMap::new(),
            gates: BTreeMap::new(),
            loops: BTreeMap::new(),
            schedules: Vec::new(),
            capsule: CapsuleConfig::default(),
            merge: MergeConfig::default(),
        }
    }

    /// Fills node-wide defaults from `~/.vowp/config.toml`.
    ///
    /// Values in `self` win. Map entries missing from `self` are copied from `node_file`;
    /// scalar `[node]` fields and `[capsule]` fields are copied only where `self` still holds
    /// the documented default, since the typed config cannot distinguish an omitted key from
    /// one written with the default value.
    pub fn with_node_defaults(mut self, node_file: &NodeFile) -> VowpConfig {
        let defaults = NodeConfig::default();
        if self.node.name == defaults.name {
            self.node.name = node_file.node.name.clone();
        }
        if self.node.max_workers == defaults.max_workers {
            self.node.max_workers = node_file.node.max_workers;
        }
        if self.node.max_verifiers == defaults.max_verifiers {
            self.node.max_verifiers = node_file.node.max_verifiers;
        }
        if self.node.tick_secs == defaults.tick_secs {
            self.node.tick_secs = node_file.node.tick_secs;
        }
        if self.node.cwd_allow.is_empty() {
            self.node.cwd_allow = node_file.node.cwd_allow.clone();
        }
        if self.node.home.is_none() {
            self.node.home = node_file.node.home.clone();
        }
        if self.node.tz_offset_minutes == defaults.tz_offset_minutes {
            self.node.tz_offset_minutes = node_file.node.tz_offset_minutes;
        }

        for (name, profile) in &node_file.profiles {
            self.profiles.entry(name.clone()).or_insert_with(|| profile.clone());
        }
        for (name, provider) in &node_file.providers {
            self.providers.entry(name.clone()).or_insert_with(|| provider.clone());
        }
        for (name, harness) in &node_file.harnesses {
            self.harnesses.entry(name.clone()).or_insert_with(|| harness.clone());
        }

        let capsule_defaults = CapsuleConfig::default();
        if self.capsule.executor.is_none() {
            self.capsule.executor = node_file.capsule.executor.clone();
        }
        if self.capsule.min_interval_secs == capsule_defaults.min_interval_secs {
            self.capsule.min_interval_secs = node_file.capsule.min_interval_secs;
        }
        if self.capsule.max_input_chars == capsule_defaults.max_input_chars {
            self.capsule.max_input_chars = node_file.capsule.max_input_chars;
        }
        if self.capsule.tail_turns == capsule_defaults.tail_turns {
            self.capsule.tail_turns = node_file.capsule.tail_turns;
        }

        self
    }

    /// Validates the `[node]` table (rule 9).
    fn validate_node(&self) -> Result<(), ProtocolError> {
        if self.node.max_workers == 0 {
            return Err(ProtocolError::Config("node.max_workers must be at least 1".to_owned()));
        }
        if self.node.max_verifiers == 0 {
            return Err(ProtocolError::Config(
                "node.max_verifiers must be at least 1".to_owned(),
            ));
        }
        if self.node.tick_secs == 0 {
            return Err(ProtocolError::Config("node.tick_secs must be at least 1".to_owned()));
        }
        Ok(())
    }

    /// Validates profiles and their ladders (rule 1).
    fn validate_profiles(&self) -> Result<(), ProtocolError> {
        for role in REQUIRED_PROFILES {
            if !self.profiles.contains_key(role) {
                return Err(ProtocolError::Config(format!("missing required profile: {role}")));
            }
        }
        for (name, profile) in &self.profiles {
            profile
                .validate()
                .map_err(|err| ProtocolError::Config(format!("profiles.{name}: {err}")))?;
        }
        Ok(())
    }

    /// Validates provider concurrency referenced by a ladder rung (rule 2).
    fn validate_providers(&self) -> Result<(), ProtocolError> {
        for profile in self.profiles.values() {
            for executor in &profile.ladder {
                let provider = executor.provider();
                if let Some(config) = self.providers.get(&provider)
                    && config.max_concurrent == 0
                {
                    return Err(ProtocolError::Config(format!(
                        "providers.{provider}.max_concurrent must be at least 1"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validates schedule specs, timezones and names (rule 3).
    fn validate_schedules(&self) -> Result<(), ProtocolError> {
        let mut names = HashSet::new();
        for schedule in &self.schedules {
            if !names.insert(schedule.name.as_str()) {
                return Err(ProtocolError::Config(format!(
                    "duplicate schedule name: {}",
                    schedule.name
                )));
            }
            match (&schedule.cron, &schedule.every) {
                (Some(cron), None) => {
                    if cron.split_whitespace().count() != 5 {
                        return Err(ProtocolError::Config(format!(
                            "schedules.{}.cron must have exactly 5 fields",
                            schedule.name
                        )));
                    }
                }
                (None, Some(every)) => match every_seconds(every) {
                    Some(secs) if secs >= 60 => {}
                    _ => {
                        return Err(ProtocolError::Config(format!(
                            "schedules.{}.every must match ^\\d+[smhd]$ and be at least 60s",
                            schedule.name
                        )));
                    }
                },
                _ => {
                    return Err(ProtocolError::Config(format!(
                        "schedules.{}: exactly one of cron and every is required",
                        schedule.name
                    )));
                }
            }
            if let Some(tz) = &schedule.tz
                && !valid_tz(tz)
            {
                return Err(ProtocolError::Config(format!(
                    "schedules.{}.tz must be UTC or ±HH:MM",
                    schedule.name
                )));
            }
        }
        Ok(())
    }

    /// Validates environments (rule 4).
    fn validate_envs(&self) -> Result<(), ProtocolError> {
        for (name, env) in &self.env {
            if env.checkout.trim().is_empty() {
                return Err(ProtocolError::Config(format!("env.{name}.checkout must not be empty")));
            }
            if env.approval == Approval::Policy && env.guards.is_empty() {
                return Err(ProtocolError::Config(format!(
                    "env.{name}.approval = \"policy\" requires at least one guard"
                )));
            }
        }
        Ok(())
    }

    /// Validates named gate sets (rule 5) through the shared [`Gate::validate`] rule.
    fn validate_gate_sets(&self) -> Result<(), ProtocolError> {
        for (name, gates) in &self.gates {
            for (index, gate) in gates.iter().enumerate() {
                gate.validate()
                    .map_err(|err| ProtocolError::Config(format!("gates.{name}[{index}]: {err}")))?;
            }
        }
        Ok(())
    }

    /// Validates loop override names (rule 6).
    fn validate_loops(&self) -> Result<(), ProtocolError> {
        for name in self.loops.keys() {
            if !KNOWN_LOOPS.contains(&name.as_str()) {
                return Err(ProtocolError::Config(format!("unknown loop name: {name}")));
            }
        }
        Ok(())
    }

    /// Validates always-allowed globs (rule 7).
    fn validate_globs(&self) -> Result<(), ProtocolError> {
        for pattern in &self.project.always_allowed {
            globset::Glob::new(pattern)
                .map_err(|err| ProtocolError::Config(format!("project.always_allowed {pattern:?}: {err}")))?;
        }
        Ok(())
    }
}

/// The node-level configuration file, `~/.vowp/config.toml`.
///
/// It carries the node-wide subset of [`VowpConfig`]: it has no `[project]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeFile {
    /// Node-wide settings.
    #[serde(default)]
    pub node: NodeConfig,
    /// Node-wide role profiles.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Node-wide provider settings.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Node-wide harness overrides.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessConfig>,
    /// Node-wide capsule settings.
    #[serde(default)]
    pub capsule: CapsuleConfig,
}

impl NodeFile {
    /// Parses the node-level configuration file.
    pub fn from_toml(s: &str) -> Result<Self, ProtocolError> {
        toml::from_str(s).map_err(|err| ProtocolError::Config(err.to_string()))
    }
}

/// Rejects unknown keys inside any `[loops.<name>]` table.
fn reject_unknown_loop_keys(raw: &toml::Table) -> Result<(), ProtocolError> {
    let Some(loops) = raw.get("loops").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (name, value) in loops {
        let Some(entry) = value.as_table() else {
            continue;
        };
        for key in entry.keys() {
            if !LOOP_KEYS.contains(&key.as_str()) {
                return Err(ProtocolError::Config(format!("unknown key loops.{name}.{key}")));
            }
        }
    }
    Ok(())
}

/// Parses an interval such as `30m` into seconds.
fn every_seconds(spec: &str) -> Option<u64> {
    let (digits, unit) = spec.split_at(spec.len().checked_sub(1)?);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let count: u64 = digits.parse().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    count.checked_mul(multiplier)
}

/// Returns true for `UTC` or a fixed `±HH:MM` offset.
fn valid_tz(tz: &str) -> bool {
    if tz == "UTC" {
        return true;
    }
    let (Some(sign), Some(rest)) = (tz.get(..1), tz.get(1..)) else {
        return false;
    };
    if !matches!(sign, "+" | "-") {
        return false;
    }
    let mut parts = rest.split(':');
    let (Some(hours), Some(minutes), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if hours.len() != 2 || minutes.len() != 2 {
        return false;
    }
    if !hours.bytes().all(|byte| byte.is_ascii_digit()) || !minutes.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (hours.parse::<u8>(), minutes.parse::<u8>()) {
        (Ok(hours), Ok(minutes)) => hours <= 23 && minutes <= 59,
        _ => false,
    }
}
