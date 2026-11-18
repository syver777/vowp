//! The §7.2 eligibility evaluator.
//!
//! The evaluator is a pure function of the effective policy and caller-supplied facts.
//! It never reads a store, a clock or the filesystem: a fact the caller cannot supply is
//! a refusal, never an implicit pass. A job-level policy refusal (`project_home_only`,
//! `kind_not_allowed`, `home_only_secret`) removes every node target, because no node can
//! satisfy it; for `any` the home stays a target, so the refusal becomes a waiting reason
//! only when the home is unavailable too. A transient node fact is a `WaitingReason` only
//! for a pinned placement, since `any` still has the home.

use std::fmt;

use vowp_protocol::config::{OffloadConfig, OffloadProjectConfig};
use vowp_protocol::{
    AgentCapacity, JobKind, LimitKind, NodeFallback, NodeName, NodePresence, NodeStatus, Placement, ProjectName,
    ResultTransport, SecretName, WaitingReason,
};

/// What the home knows about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeFacts {
    /// Whether the home capacity brake is currently active.
    pub brake_active: bool,
    /// Number of remote assignments that occupy a fleet slot.
    pub active_remote_runs: u32,
}

/// Names-only capabilities a node advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeCapabilities {
    /// Harness names the node can run.
    pub harnesses: Vec<String>,
    /// Secret names the node can resolve locally; never values.
    pub secrets: Vec<SecretName>,
    /// Projects mapped on the node.
    pub projects: Vec<ProjectName>,
}

/// The last path map a node reported and what the caller could verify about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFacts {
    /// A path map row exists for the project.
    pub map_present: bool,
    /// The local project root exists.
    pub local_root_present: bool,
    /// The worktree root exists.
    pub worktree_root_present: bool,
    /// The local and worktree roots are writable.
    pub writable: bool,
}

/// One provider limit active on a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitFact {
    /// Provider key the limit applies to.
    pub provider: String,
    /// Limit class.
    pub kind: LimitKind,
    /// Redacted detail; never a secret value.
    pub detail: String,
}

/// Thermal state reported by the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalState {
    /// Normal operating temperature.
    Nominal,
    /// Warm but usable.
    Fair,
    /// Thermally limited; new runs are refused unless `allow_battery`.
    Serious,
    /// Critically limited; new runs are refused unless `allow_battery`.
    Critical,
}

impl ThermalState {
    /// Parses a node-reported thermal name; an unknown name is treated as nominal.
    pub fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("fair") => Self::Fair,
            Some("serious") => Self::Serious,
            Some("critical") => Self::Critical,
            _ => Self::Nominal,
        }
    }
}

/// The node's battery and thermal fact. `None` in [`NodeFacts::power`] is a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePower {
    /// Whether the node is running on battery.
    pub on_battery: bool,
    /// Reported thermal state.
    pub thermal_state: ThermalState,
}

/// Everything the policy needs to know about one candidate node.
///
/// Optional facts are deliberate: `None` means the home does not know and the evaluator
/// refuses rather than assuming the best case. `capacity` and `path` are likewise
/// required before a node can be chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeFacts {
    /// Node name.
    pub name: NodeName,
    /// Enrollment state.
    pub status: NodeStatus,
    /// Computed presence.
    pub presence: NodePresence,
    /// Enrolled labels.
    pub labels: Vec<String>,
    /// Names-only capabilities.
    pub capabilities: NodeCapabilities,
    /// Latest capacity snapshot; its battery fields are display data, not policy facts.
    pub capacity: Option<AgentCapacity>,
    /// Battery and thermal state; `None` means the home does not know.
    pub power: Option<NodePower>,
    /// Active provider limits.
    pub limits: Vec<LimitFact>,
    /// Path map facts for the job's project.
    pub path: Option<PathFacts>,
    /// Whether the node's origin matches the project remote.
    ///
    /// Only a `push` assignment requires it. A `bundle` assignment returns its commits over
    /// the agent channel and never reads or uses a git remote on the node.
    pub origin_matches: Option<bool>,
    /// Whether the job's base commit is fetchable on the node.
    pub base_fetchable: Option<bool>,
    /// Whether the node's base checkout is clean.
    pub clean_base: Option<bool>,
    /// Whether the job's `owns` overlap a running job on the node.
    pub owns_conflict: Option<bool>,
}

/// Job-side requirements the policy evaluates. Secret values never appear here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobRequirements {
    /// Labels the job requires the node to have enrolled.
    pub labels: Vec<String>,
    /// Harness names the job requires.
    pub harnesses: Vec<String>,
    /// Secret names the job needs the node to resolve locally.
    pub secrets: Vec<SecretName>,
    /// Provider key of the job's executor, when one is chosen.
    pub provider: Option<String>,
}

/// One job's kind, placement and requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRequest {
    /// What the job does.
    pub kind: JobKind,
    /// The per-job side-table placement; `None` uses the project default.
    pub placement: Option<Placement>,
    /// Labels, harnesses, secrets and provider the job needs.
    pub requires: JobRequirements,
}

impl JobRequest {
    /// Builds a request with no extra requirements.
    pub fn new(kind: JobKind, placement: Option<Placement>) -> Self {
        Self {
            kind,
            placement,
            requires: JobRequirements::default(),
        }
    }
}

/// Where an eligible job may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The home.
    Home,
    /// The named node.
    Node(NodeName),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Home => f.write_str("home"),
            Self::Node(node) => write!(f, "node:{node}"),
        }
    }
}

/// The evaluator's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The job may run on these targets.
    Eligible {
        /// Home and/or eligible nodes, with the home first when it is a target.
        targets: Vec<Target>,
    },
    /// The job stays `ready` with this first reason.
    Waiting {
        /// The §7.5 waiting-reason vocabulary value.
        reason: WaitingReason,
        /// Human-readable, path-free detail.
        detail: String,
        /// The node the reason is about, when it is node-specific.
        node: Option<NodeName>,
    },
}

impl Decision {
    /// Returns whether the job may run on at least one target.
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible { .. })
    }

    /// Returns whether the job is waiting for a reason.
    pub fn is_waiting(&self) -> bool {
        matches!(self, Self::Waiting { .. })
    }

    /// Returns the eligible targets; empty for a waiting decision.
    pub fn targets(&self) -> &[Target] {
        match self {
            Self::Eligible { targets } => targets,
            Self::Waiting { .. } => &[],
        }
    }

    /// Returns the waiting reason, when the job is waiting.
    pub fn waiting_reason(&self) -> Option<WaitingReason> {
        match self {
            Self::Eligible { .. } => None,
            Self::Waiting { reason, .. } => Some(*reason),
        }
    }

    /// Returns the human-readable detail, when the job is waiting.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Eligible { .. } => None,
            Self::Waiting { detail, .. } => Some(detail),
        }
    }

    /// Returns the node the waiting reason is about, when it is node-specific.
    pub fn node(&self) -> Option<&NodeName> {
        match self {
            Self::Eligible { .. } => None,
            Self::Waiting { node, .. } => node.as_ref(),
        }
    }
}

/// Evaluates §7.2 checks 1–11 in order.
///
/// `project_policy` is the effective `OffloadProjectConfig` (defaults already filled) for
/// `project`; `fleet` supplies the fleet settings and per-node overrides. The returned
/// targets are candidates, not a dispatch decision: the dispatcher still picks among them.
pub fn evaluate(
    fleet: &OffloadConfig,
    project: &ProjectName,
    project_policy: &OffloadProjectConfig,
    job: &JobRequest,
    home: &HomeFacts,
    nodes: &[NodeFacts],
) -> Decision {
    let placement = job
        .placement
        .clone()
        .or_else(|| project_policy.placement.clone())
        .unwrap_or_else(|| fleet.default.clone());

    match placement {
        Placement::Home => match home_target(fleet, home) {
            Ok(target) => Decision::Eligible { targets: vec![target] },
            Err((reason, detail)) => Decision::Waiting {
                reason,
                detail,
                node: None,
            },
        },
        Placement::Any => evaluate_any(fleet, project, project_policy, job, home, nodes),
        Placement::Node { node, fallback } => {
            evaluate_node(fleet, project, project_policy, job, home, nodes, &node, fallback)
        }
    }
}

/// Evaluates an `any` placement: the home plus every eligible node.
fn evaluate_any(
    fleet: &OffloadConfig,
    project: &ProjectName,
    project_policy: &OffloadProjectConfig,
    job: &JobRequest,
    home: &HomeFacts,
    nodes: &[NodeFacts],
) -> Decision {
    // A job-level refusal removes every node target, but the home still runs the job: a
    // `home_only` project, a denied kind and a home-only secret are reasons to stay home,
    // not reasons to park the job.
    let refusal = job_level_refusal(project, project_policy, job);

    let mut targets = Vec::new();
    let mut node_reasons = Vec::new();
    if refusal.is_none() {
        for facts in nodes {
            match check_node(fleet, project, job, home, facts) {
                Ok(()) => targets.push(Target::Node(facts.name.clone())),
                Err(reason) => node_reasons.push((facts.name.clone(), reason)),
            }
        }
    }
    let home_reason = match home_target(fleet, home) {
        Ok(target) => {
            targets.insert(0, target);
            None
        }
        Err(reason) => Some(reason),
    };

    if targets.is_empty() {
        if let Some((reason, detail)) = refusal {
            return Decision::Waiting {
                reason,
                detail,
                node: None,
            };
        }
        if let Some((node, (reason, detail))) = node_reasons.into_iter().next() {
            return Decision::Waiting {
                reason,
                detail,
                node: Some(node),
            };
        }
        let (reason, detail) = home_reason.unwrap_or_else(|| {
            (
                WaitingReason::NoEligibleNode,
                format!("no node satisfies the placement for project {project}"),
            )
        });
        return Decision::Waiting {
            reason,
            detail,
            node: None,
        };
    }
    Decision::Eligible { targets }
}

/// Evaluates a pinned `node:<name>` placement, including its explicit home fallback.
#[allow(clippy::too_many_arguments)]
fn evaluate_node(
    fleet: &OffloadConfig,
    project: &ProjectName,
    project_policy: &OffloadProjectConfig,
    job: &JobRequest,
    home: &HomeFacts,
    nodes: &[NodeFacts],
    node: &NodeName,
    fallback: Option<NodeFallback>,
) -> Decision {
    if let Some((reason, detail)) = job_level_refusal(project, project_policy, job) {
        return match fallback {
            Some(NodeFallback::Home) => fallback_home(fleet, home),
            None => Decision::Waiting {
                reason,
                detail,
                node: Some(node.clone()),
            },
        };
    }
    let Some(facts) = nodes.iter().find(|facts| &facts.name == node) else {
        return match fallback {
            Some(NodeFallback::Home) => fallback_home(fleet, home),
            None => Decision::Waiting {
                reason: WaitingReason::NoEligibleNode,
                detail: format!("node {node} is not enrolled"),
                node: Some(node.clone()),
            },
        };
    };
    match check_node(fleet, project, job, home, facts) {
        Ok(()) => Decision::Eligible {
            targets: vec![Target::Node(facts.name.clone())],
        },
        Err((reason, detail)) => match fallback {
            Some(NodeFallback::Home) => fallback_home(fleet, home),
            None => Decision::Waiting {
                reason,
                detail,
                node: Some(node.clone()),
            },
        },
    }
}

/// Returns the home as the explicit fallback target.
fn fallback_home(fleet: &OffloadConfig, home: &HomeFacts) -> Decision {
    match home_target(fleet, home) {
        Ok(target) => Decision::Eligible { targets: vec![target] },
        Err((reason, detail)) => Decision::Waiting {
            reason,
            detail,
            node: None,
        },
    }
}

/// A job-level requirement that no node can satisfy.
fn job_level_refusal(
    project: &ProjectName,
    policy: &OffloadProjectConfig,
    job: &JobRequest,
) -> Option<(WaitingReason, String)> {
    if policy.home_only {
        return Some((
            WaitingReason::ProjectHomeOnly,
            format!("project {project} is marked home_only"),
        ));
    }
    if !kind_allowed(policy, job.kind) {
        return Some((
            WaitingReason::KindNotAllowed,
            format!("job kind {} is not allowed to leave the home", kind_name(job.kind)),
        ));
    }
    if let Some(secret) = job
        .requires
        .secrets
        .iter()
        .find(|name| !policy.allow_secrets.contains(name))
    {
        return Some((
            WaitingReason::HomeOnlySecret,
            format!("secret {secret} is home-only for project {project}"),
        ));
    }
    None
}

/// Returns whether the project policy allows `kind` on a node.
fn kind_allowed(policy: &OffloadProjectConfig, kind: JobKind) -> bool {
    if policy.deny_kinds.contains(&kind) {
        return false;
    }
    policy.allow_kinds.contains(&kind)
}

/// Checks one node in §7.2 order. The first failure wins.
fn check_node(
    fleet: &OffloadConfig,
    project: &ProjectName,
    job: &JobRequest,
    home: &HomeFacts,
    facts: &NodeFacts,
) -> Result<(), (WaitingReason, String)> {
    let node = &facts.name;
    let node_policy = fleet.node_policy(node);

    // 1. Offload is enabled; the node is active, not revoked, and online.
    if !fleet.enabled {
        return Err((WaitingReason::NoEligibleNode, "offload is disabled".to_owned()));
    }
    match facts.status {
        NodeStatus::Active => {}
        NodeStatus::Pending => {
            return Err((WaitingReason::NodeOffline, format!("node {node} is pending enrollment")));
        }
        NodeStatus::Revoked => {
            return Err((WaitingReason::NodeRevoked, format!("node {node} is revoked")));
        }
    }
    match facts.presence {
        NodePresence::Online => {}
        NodePresence::Degraded => {
            return Err((
                WaitingReason::NodeOffline,
                format!("node {node} is degraded (last heartbeat 30–90 seconds ago)"),
            ));
        }
        NodePresence::Offline => {
            return Err((WaitingReason::NodeOffline, format!("node {node} is offline")));
        }
        NodePresence::Revoked => {
            return Err((WaitingReason::NodeRevoked, format!("node {node} is revoked")));
        }
    }

    // 2. The placement matches; the caller filters and the pinned path checks the name.

    // 3. The project opted in and is not home_only; `job_level_refusal` already ran.

    // 4. The job kind is allowed; `job_level_refusal` already ran.

    // 5. Required labels and capabilities are present.
    for label in node_policy.labels.iter().chain(job.requires.labels.iter()) {
        if !facts.labels.contains(label) {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("node {node} does not have the required label {label}"),
            ));
        }
    }
    for harness in &job.requires.harnesses {
        if !facts.capabilities.harnesses.contains(harness) {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("node {node} does not have the required harness {harness}"),
            ));
        }
    }

    // 6. No required secret is home-only; the node declares the names it can resolve.
    for secret in &job.requires.secrets {
        if !facts.capabilities.secrets.contains(secret) {
            return Err((
                WaitingReason::MissingSecret,
                format!("node {node} does not declare the secret {secret}"),
            ));
        }
    }

    // 7. The path map exists and is valid; roots exist, are writable and disk is above the reserve.
    if !facts.capabilities.projects.contains(project) {
        return Err((
            WaitingReason::PathUnmapped,
            format!("project {project} is not mapped on node {node}"),
        ));
    }
    let Some(path) = &facts.path else {
        return Err((
            WaitingReason::PathUnmapped,
            format!("node {node} has no path map for project {project}"),
        ));
    };
    if !path.map_present {
        return Err((
            WaitingReason::PathUnmapped,
            format!("node {node} has no path map for project {project}"),
        ));
    }
    if !path.local_root_present {
        return Err((WaitingReason::PathMissing, format!("node {node} local root is missing")));
    }
    if !path.worktree_root_present {
        return Err((
            WaitingReason::PathMissing,
            format!("node {node} worktree root is missing"),
        ));
    }
    if !path.writable {
        return Err((
            WaitingReason::PathMissing,
            format!("node {node} project or worktree root is not writable"),
        ));
    }
    let Some(capacity) = &facts.capacity else {
        return Err((
            WaitingReason::NodeDiskLow,
            format!("node {node} has not reported free disk space"),
        ));
    };
    if capacity.disk_free_bytes < fleet.min_disk_bytes {
        return Err((
            WaitingReason::NodeDiskLow,
            format!(
                "node {node} has {} bytes free, below the {} byte reserve",
                capacity.disk_free_bytes, fleet.min_disk_bytes
            ),
        ));
    }

    // 8. The node has a free slot and is not battery/thermal limited; fleet cap applies.
    let max_slots = node_policy.max_workers.unwrap_or(capacity.max_slots);
    if max_slots == 0 {
        return Err((WaitingReason::NodeBusy, format!("node {node} has no worker slots")));
    }
    if capacity.busy_slots >= max_slots {
        return Err((
            WaitingReason::NodeBusy,
            format!("node {node} is at its worker cap ({}/{max_slots})", capacity.busy_slots),
        ));
    }
    if home.active_remote_runs >= fleet.max_remote_runs {
        return Err((
            WaitingReason::NodeBusy,
            format!(
                "fleet max_remote_runs = {} is reached ({} active)",
                fleet.max_remote_runs, home.active_remote_runs
            ),
        ));
    }
    let Some(power) = &facts.power else {
        return Err((
            WaitingReason::NodeBattery,
            format!("node {node} has not reported battery and thermal state"),
        ));
    };
    if !node_policy.allow_battery {
        if power.on_battery {
            return Err((
                WaitingReason::NodeBattery,
                format!("node {node} is on battery and allow_battery = false"),
            ));
        }
        if matches!(power.thermal_state, ThermalState::Serious | ThermalState::Critical) {
            return Err((
                WaitingReason::NodeBattery,
                format!("node {node} is thermally limited and allow_battery = false"),
            ));
        }
    }

    // 9. The node's provider limit does not block the executor.
    if let Some(provider) = job.requires.provider.as_deref() {
        if let Some(limit) = facts.limits.iter().find(|limit| limit.provider == provider) {
            return Err((WaitingReason::NodeLimit, limit_detail(node, limit)));
        }
    } else if let Some(limit) = facts.limits.first() {
        return Err((
            WaitingReason::NodeLimit,
            format!(
                "node {node} has an active {} limit and the job's provider is not known",
                limit.provider
            ),
        ));
    }

    // 10. No `owns` overlap with a running job on the node.
    match facts.owns_conflict {
        Some(false) => {}
        Some(true) => {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("job owns paths that a running job holds on node {node}"),
            ));
        }
        None => {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("ownership conflict state is not known on node {node}"),
            ));
        }
    }

    // 11. Origin matches (push only), the base commit is fetchable and the base is clean.
    // A bundle assignment needs read access to the base, not a push remote: the push-remote
    // match is not a bundle eligibility fact.
    if fleet.result_transport == ResultTransport::Push {
        match facts.origin_matches {
            Some(true) => {}
            Some(false) => {
                return Err((
                    WaitingReason::RemoteMismatch,
                    format!("node {node} origin does not match the project remote"),
                ));
            }
            None => {
                return Err((
                    WaitingReason::RemoteMismatch,
                    format!("node {node} origin match is not known"),
                ));
            }
        }
    }
    match facts.base_fetchable {
        Some(true) => {}
        Some(false) => {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("the base commit is not fetchable on node {node}"),
            ));
        }
        None => {
            return Err((
                WaitingReason::NoEligibleNode,
                format!("base commit fetchability is not known on node {node}"),
            ));
        }
    }
    if fleet.require_clean_base {
        match facts.clean_base {
            Some(true) => {}
            Some(false) => {
                return Err((
                    WaitingReason::NoEligibleNode,
                    format!("node {node} base checkout is not clean"),
                ));
            }
            None => {
                return Err((
                    WaitingReason::NoEligibleNode,
                    format!("base checkout cleanliness is not known on node {node}"),
                ));
            }
        }
    }
    Ok(())
}

/// Returns the home target, or the brake refusal.
fn home_target(fleet: &OffloadConfig, home: &HomeFacts) -> Result<Target, (WaitingReason, String)> {
    if fleet.pause_on_home_brake && home.brake_active {
        return Err((
            WaitingReason::NoEligibleNode,
            "the home capacity brake is active and pause_on_home_brake = true".to_owned(),
        ));
    }
    Ok(Target::Home)
}

/// Formats one provider-limit detail without a secret value.
fn limit_detail(node: &NodeName, limit: &LimitFact) -> String {
    if limit.detail.is_empty() {
        format!("node {node} provider {} is limited ({:?})", limit.provider, limit.kind)
    } else {
        format!(
            "node {node} provider {} is limited ({:?}): {}",
            limit.provider, limit.kind, limit.detail
        )
    }
}

/// Returns the snake_case name of a job kind.
fn kind_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Plan => "plan",
        JobKind::Spec => "spec",
        JobKind::Build => "build",
        JobKind::Test => "test",
        JobKind::Verify => "verify",
        JobKind::Fix => "fix",
        JobKind::Merge => "merge",
        JobKind::Docs => "docs",
        JobKind::Research => "research",
        JobKind::Promote => "promote",
        JobKind::Release => "release",
        JobKind::Decision => "decision",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vowp_protocol::AgentCapacity;

    fn project() -> ProjectName {
        ProjectName::parse("alpha").expect("project")
    }

    fn node() -> NodeName {
        NodeName::parse("mac-studio").expect("node")
    }

    fn fact() -> NodeFacts {
        NodeFacts {
            name: node(),
            status: NodeStatus::Active,
            presence: NodePresence::Online,
            labels: Vec::new(),
            capabilities: NodeCapabilities {
                harnesses: vec!["opencode".to_owned()],
                secrets: Vec::new(),
                projects: vec![project()],
            },
            capacity: Some(AgentCapacity {
                cpu_cores: 8,
                load1: 0.1,
                mem_total_bytes: 16 * 1024 * 1024 * 1024,
                mem_free_bytes: 8 * 1024 * 1024 * 1024,
                disk_free_bytes: 1024 * 1024 * 1024 * 1024,
                busy_slots: 0,
                max_slots: 2,
                on_battery: Some(false),
                thermal_state: Some("nominal".to_owned()),
            }),
            power: Some(NodePower {
                on_battery: false,
                thermal_state: ThermalState::Nominal,
            }),
            limits: Vec::new(),
            path: Some(PathFacts {
                map_present: true,
                local_root_present: true,
                worktree_root_present: true,
                writable: true,
            }),
            // A node with no push access: it can read the base but its origin does not
            // match the project remote.
            origin_matches: Some(false),
            base_fetchable: Some(true),
            clean_base: Some(true),
            owns_conflict: Some(false),
        }
    }

    fn request() -> JobRequest {
        JobRequest {
            kind: JobKind::Build,
            placement: Some(Placement::Node {
                node: node(),
                fallback: None,
            }),
            requires: JobRequirements {
                harnesses: vec!["opencode".to_owned()],
                ..JobRequirements::default()
            },
        }
    }

    #[test]
    fn bundle_eligibility_does_not_require_a_push_remote() {
        let fleet = OffloadConfig {
            enabled: true,
            result_transport: ResultTransport::Bundle,
            ..OffloadConfig::default()
        };
        let decision = evaluate(
            &fleet,
            &project(),
            &OffloadProjectConfig::default(),
            &request(),
            &HomeFacts::default(),
            &[fact()],
        );
        assert!(
            decision.is_eligible(),
            "bundle is eligible without a push remote: {decision:?}"
        );

        let fleet = OffloadConfig {
            enabled: true,
            result_transport: ResultTransport::Push,
            ..OffloadConfig::default()
        };
        let decision = evaluate(
            &fleet,
            &project(),
            &OffloadProjectConfig::default(),
            &request(),
            &HomeFacts::default(),
            &[fact()],
        );
        assert_eq!(
            decision.waiting_reason(),
            Some(WaitingReason::RemoteMismatch),
            "push still requires the remote match"
        );
    }

    #[test]
    fn bundle_eligibility_still_requires_a_fetchable_base() {
        let fleet = OffloadConfig {
            enabled: true,
            result_transport: ResultTransport::Bundle,
            ..OffloadConfig::default()
        };
        let mut facts = fact();
        facts.base_fetchable = Some(false);
        let decision = evaluate(
            &fleet,
            &project(),
            &OffloadProjectConfig::default(),
            &request(),
            &HomeFacts::default(),
            &[facts],
        );
        assert!(
            decision.detail().is_some_and(|detail| detail.contains("not fetchable")),
            "a bundle node still needs read access to the base: {decision:?}"
        );
    }
}
