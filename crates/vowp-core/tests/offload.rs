//! Offload policy evaluation: the §7.2 checks, in order, with caller-supplied facts.
//! Every fixture is a plain value; the test never opens `~/.vowp`, tmux, the network or a
//! harness. Spec: `docs/design/v0.4/local-orchestrator.md` §6.6, §6.7 and §7.1–§7.5.

use vowp_core::offload::{
    Decision, HomeFacts, JobRequest, JobRequirements, LimitFact, NodeCapabilities, NodeFacts, NodePower, PathFacts,
    Target, ThermalState, evaluate,
};
use vowp_protocol::config::{OffloadConfig, OffloadNodeConfig, OffloadProjectConfig};
use vowp_protocol::{
    AgentCapacity, JobKind, LimitKind, NodeFallback, NodeName, NodePresence, NodeStatus, Placement, ProjectName,
    SecretName, WaitingReason,
};

/// The project used by every fixture.
fn project() -> ProjectName {
    ProjectName::parse("vowp").expect("project slug")
}

/// A node name.
fn node(name: &str) -> NodeName {
    NodeName::parse(name).expect("node slug")
}

/// A secret name.
fn secret(name: &str) -> SecretName {
    SecretName::parse(name).expect("secret name")
}

/// An enabled policy with the documented defaults.
fn enabled_policy() -> OffloadConfig {
    OffloadConfig {
        enabled: true,
        ..OffloadConfig::default()
    }
}

/// A healthy node named `name`, mapped to `project()`.
fn healthy_node(name: &str) -> NodeFacts {
    NodeFacts {
        name: node(name),
        status: NodeStatus::Active,
        presence: NodePresence::Online,
        labels: vec!["linux".to_owned()],
        capabilities: NodeCapabilities {
            harnesses: vec!["opencode".to_owned()],
            secrets: vec![secret("BUILD_TOKEN")],
            projects: vec![project()],
        },
        capacity: Some(capacity(0, 4)),
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
        origin_matches: Some(true),
        base_fetchable: Some(true),
        clean_base: Some(true),
        owns_conflict: Some(false),
    }
}

/// A capacity snapshot with the given busy and maximum slots.
fn capacity(busy: u32, max: u32) -> AgentCapacity {
    AgentCapacity {
        cpu_cores: 8,
        load1: 0.1,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        mem_free_bytes: 8 * 1024 * 1024 * 1024,
        disk_free_bytes: 100 * 1024 * 1024 * 1024,
        busy_slots: busy,
        max_slots: max,
        on_battery: Some(false),
        thermal_state: Some("nominal".to_owned()),
    }
}

/// An idle home.
fn home() -> HomeFacts {
    HomeFacts::default()
}

/// Evaluates one pinned node job against one node and the default project policy.
fn evaluate_node(
    policy: &OffloadConfig,
    project_policy: &OffloadProjectConfig,
    kind: JobKind,
    facts: &NodeFacts,
) -> Decision {
    let request = JobRequest::new(
        kind,
        Some(Placement::Node {
            node: facts.name.clone(),
            fallback: None,
        }),
    );
    evaluate(
        policy,
        &project(),
        project_policy,
        &request,
        &home(),
        std::slice::from_ref(facts),
    )
}

#[test]
fn placement_home_node_and_any_choose_the_documented_targets() {
    let policy = enabled_policy();
    let project_policy = OffloadProjectConfig::default();
    let facts = healthy_node("mac-studio");

    let home_only = JobRequest::new(JobKind::Build, Some(Placement::Home));
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &home_only,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    let pinned = JobRequest::new(
        JobKind::Build,
        Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
    );
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &pinned,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Node(node("mac-studio"))]
        }
    );

    let any = JobRequest::new(JobKind::Build, Some(Placement::Any));
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &any,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home, Target::Node(node("mac-studio"))]
        }
    );

    // `node:<name>+home` uses the node when it is eligible and the home when it is not.
    let fallback = JobRequest::new(
        JobKind::Build,
        Some(Placement::Node {
            node: node("mac-studio"),
            fallback: Some(NodeFallback::Home),
        }),
    );
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &fallback,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Node(node("mac-studio"))]
        }
    );
    let mut offline = facts;
    offline.presence = NodePresence::Offline;
    let decision = evaluate(&policy, &project(), &project_policy, &fallback, &home(), &[offline]);
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    // A project default of `any` behaves the same when the job has no placement.
    let project_any = OffloadProjectConfig {
        placement: Some(Placement::Any),
        ..OffloadProjectConfig::default()
    };
    let no_placement = JobRequest::new(JobKind::Build, None);
    let decision = evaluate(
        &policy,
        &project(),
        &project_any,
        &no_placement,
        &home(),
        &[healthy_node("mac-studio")],
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home, Target::Node(node("mac-studio"))]
        }
    );
}

#[test]
fn home_only_project_and_denied_kind_are_refused() {
    let policy = enabled_policy();
    let facts = healthy_node("mac-studio");

    let home_only = OffloadProjectConfig {
        home_only: true,
        ..OffloadProjectConfig::default()
    };
    let decision = evaluate_node(&policy, &home_only, JobKind::Build, &facts);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::ProjectHomeOnly),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("home_only"));

    // A denied kind is refused before any node work happens.
    let default_policy = OffloadProjectConfig::default();
    let decision = evaluate_node(&policy, &default_policy, JobKind::Merge, &facts);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::KindNotAllowed),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("merge"));

    // Home placement still runs the same job: the reason is about offload, not failure.
    let home_request = JobRequest::new(JobKind::Merge, Some(Placement::Home));
    let decision = evaluate(&policy, &project(), &default_policy, &home_request, &home(), &[facts]);
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    // `any` still has the home: a policy refusal removes the node, never the home.
    let any_home_only = JobRequest::new(JobKind::Build, Some(Placement::Any));
    let decision = evaluate(
        &policy,
        &project(),
        &home_only,
        &any_home_only,
        &home(),
        &[healthy_node("mac-studio")],
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    let any_denied = JobRequest::new(JobKind::Merge, Some(Placement::Any));
    let decision = evaluate(
        &policy,
        &project(),
        &default_policy,
        &any_denied,
        &home(),
        &[healthy_node("mac-studio")],
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    // With no home target left, the job-level refusal is the reason the job waits.
    let braked = HomeFacts {
        brake_active: true,
        active_remote_runs: 0,
    };
    let mut paused = policy.clone();
    paused.pause_on_home_brake = true;
    let decision = evaluate(
        &paused,
        &project(),
        &home_only,
        &any_home_only,
        &braked,
        &[healthy_node("mac-studio")],
    );
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::ProjectHomeOnly),
        "{decision:?}"
    );
}

#[test]
fn home_only_secret_is_refused_and_a_node_secret_that_is_declared_is_allowed() {
    let policy = enabled_policy();
    let facts = healthy_node("mac-studio");
    let project_policy = OffloadProjectConfig {
        allow_secrets: vec![secret("BUILD_TOKEN")],
        ..OffloadProjectConfig::default()
    };

    let allowed = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
        requires: JobRequirements {
            secrets: vec![secret("BUILD_TOKEN")],
            ..JobRequirements::default()
        },
    };
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &allowed,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Node(node("mac-studio"))]
        }
    );

    let home_only = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
        requires: JobRequirements {
            secrets: vec![secret("BUILD_TOKEN"), secret("PROD_DEPLOY_KEY")],
            ..JobRequirements::default()
        },
    };
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &home_only,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::HomeOnlySecret),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("PROD_DEPLOY_KEY"));

    // `any` keeps the home target when the secret is home-only.
    let any_home_only = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Any),
        requires: JobRequirements {
            secrets: vec![secret("BUILD_TOKEN"), secret("PROD_DEPLOY_KEY")],
            ..JobRequirements::default()
        },
    };
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &any_home_only,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision,
        Decision::Eligible {
            targets: vec![Target::Home]
        }
    );

    // A project-allowed secret the node does not declare is `missing_secret`.
    let mut undeclared = facts;
    undeclared.capabilities.secrets.clear();
    let decision = evaluate(&policy, &project(), &project_policy, &allowed, &home(), &[undeclared]);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::MissingSecret),
        "{decision:?}"
    );
}

#[test]
fn missing_label_capability_or_harness_reports_the_documented_reason() {
    let policy = enabled_policy();
    let project_policy = OffloadProjectConfig::default();
    let facts = healthy_node("mac-studio");

    // An owner-required label from `[offload.nodes.<name>]`.
    let mut policy_with_label = policy.clone();
    policy_with_label.nodes.insert(
        node("mac-studio"),
        OffloadNodeConfig {
            labels: vec!["xcode".to_owned()],
            ..OffloadNodeConfig::default()
        },
    );
    let decision = evaluate_node(&policy_with_label, &project_policy, JobKind::Build, &facts);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::NoEligibleNode),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("label xcode"));

    // A job-required harness the node does not advertise.
    let required_harness = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
        requires: JobRequirements {
            harnesses: vec!["xcodebuild".to_owned()],
            ..JobRequirements::default()
        },
    };
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &required_harness,
        &home(),
        std::slice::from_ref(&facts),
    );
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::NoEligibleNode),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("harness xcodebuild"));

    // The project must be a declared capability (mapped project).
    let mut unmapped = facts;
    unmapped.capabilities.projects.clear();
    let decision = evaluate_node(&policy, &project_policy, JobKind::Build, &unmapped);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::PathUnmapped),
        "{decision:?}"
    );
}

#[test]
fn offline_revoked_busy_battery_disk_low_and_limit_report_the_documented_reason() {
    let policy = enabled_policy();
    let project_policy = OffloadProjectConfig::default();
    let facts = healthy_node("mac-studio");

    let mut offline = facts.clone();
    offline.presence = NodePresence::Offline;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &offline).waiting_reason(),
        Some(WaitingReason::NodeOffline)
    );

    let mut revoked = facts.clone();
    revoked.status = NodeStatus::Revoked;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &revoked).waiting_reason(),
        Some(WaitingReason::NodeRevoked)
    );

    let mut busy = facts.clone();
    busy.capacity = Some(capacity(4, 4));
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &busy).waiting_reason(),
        Some(WaitingReason::NodeBusy)
    );

    let mut battery = facts.clone();
    battery.power = Some(NodePower {
        on_battery: true,
        thermal_state: ThermalState::Nominal,
    });
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &battery).waiting_reason(),
        Some(WaitingReason::NodeBattery)
    );

    let mut thermal = facts.clone();
    thermal.power = Some(NodePower {
        on_battery: false,
        thermal_state: ThermalState::Critical,
    });
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &thermal).waiting_reason(),
        Some(WaitingReason::NodeBattery)
    );

    // A missing battery/thermal fact is a refusal, never a pass.
    let mut unknown_power = facts.clone();
    unknown_power.power = None;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &unknown_power).waiting_reason(),
        Some(WaitingReason::NodeBattery)
    );

    let mut disk_low = facts.clone();
    disk_low.capacity = Some(AgentCapacity {
        disk_free_bytes: 1024,
        ..capacity(0, 4)
    });
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &disk_low).waiting_reason(),
        Some(WaitingReason::NodeDiskLow)
    );

    let mut limited = facts.clone();
    limited.limits.push(LimitFact {
        provider: "opencode".to_owned(),
        kind: LimitKind::Credits,
        detail: "credits exhausted".to_owned(),
    });
    let with_provider = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
        requires: JobRequirements {
            provider: Some("opencode".to_owned()),
            ..JobRequirements::default()
        },
    };
    let decision = evaluate(
        &policy,
        &project(),
        &project_policy,
        &with_provider,
        &home(),
        &[limited.clone()],
    );
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::NodeLimit),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("credits exhausted"));

    // A limit for another provider does not block this job.
    let other = JobRequest {
        kind: JobKind::Build,
        placement: Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
        requires: JobRequirements {
            provider: Some("claude-code".to_owned()),
            ..JobRequirements::default()
        },
    };
    assert!(evaluate(&policy, &project(), &project_policy, &other, &home(), &[limited]).is_eligible());
}

#[test]
fn node_at_its_worker_cap_reports_node_busy() {
    let mut policy = enabled_policy();
    policy.nodes.insert(
        node("mac-studio"),
        OffloadNodeConfig {
            max_workers: Some(1),
            ..OffloadNodeConfig::default()
        },
    );
    let project_policy = OffloadProjectConfig::default();
    let mut facts = healthy_node("mac-studio");
    facts.capacity = Some(capacity(1, 8));

    let decision = evaluate_node(&policy, &project_policy, JobKind::Build, &facts);
    assert_eq!(decision.waiting_reason(), Some(WaitingReason::NodeBusy), "{decision:?}");
    assert!(decision.detail().expect("detail").contains("worker cap (1/1)"));

    // The fleet cap also reports node_busy, never no_eligible_node.
    let mut fleet_full = HomeFacts {
        brake_active: false,
        active_remote_runs: 2,
    };
    facts.capacity = Some(capacity(0, 4));
    let decision = evaluate_node_with_home(&policy, &project_policy, JobKind::Build, &facts, &fleet_full);
    assert_eq!(decision.waiting_reason(), Some(WaitingReason::NodeBusy), "{decision:?}");
    assert!(decision.detail().expect("detail").contains("max_remote_runs"));

    fleet_full.active_remote_runs = 1;
    let decision = evaluate_node_with_home(&policy, &project_policy, JobKind::Build, &facts, &fleet_full);
    assert!(decision.is_eligible(), "{decision:?}");
}

/// Evaluates a pinned job with an explicit home fact set.
fn evaluate_node_with_home(
    policy: &OffloadConfig,
    project_policy: &OffloadProjectConfig,
    kind: JobKind,
    facts: &NodeFacts,
    home: &HomeFacts,
) -> Decision {
    let request = JobRequest::new(
        kind,
        Some(Placement::Node {
            node: facts.name.clone(),
            fallback: None,
        }),
    );
    evaluate(
        policy,
        &project(),
        project_policy,
        &request,
        home,
        std::slice::from_ref(facts),
    )
}

#[test]
fn no_eligible_node_is_a_waiting_reason_not_a_failure() {
    let policy = enabled_policy();
    let project_policy = OffloadProjectConfig::default();

    // The pinned node is not enrolled at all.
    let absent = JobRequest::new(
        JobKind::Build,
        Some(Placement::Node {
            node: node("mac-studio"),
            fallback: None,
        }),
    );
    let decision = evaluate(&policy, &project(), &project_policy, &absent, &home(), &[]);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::NoEligibleNode),
        "{decision:?}"
    );
    assert!(decision.detail().expect("detail").contains("not enrolled"));
    assert!(decision.targets().is_empty());

    // A node missing a path root is path_missing, still a waiting reason.
    let mut facts = healthy_node("mac-studio");
    facts.path = Some(PathFacts {
        map_present: true,
        local_root_present: false,
        worktree_root_present: true,
        writable: true,
    });
    let decision = evaluate_node(&policy, &project_policy, JobKind::Build, &facts);
    assert_eq!(
        decision.waiting_reason(),
        Some(WaitingReason::PathMissing),
        "{decision:?}"
    );
    assert!(decision.is_waiting());
}

#[test]
fn missing_origin_or_base_facts_are_refusals() {
    let policy = enabled_policy();
    let project_policy = OffloadProjectConfig::default();

    let mut facts = healthy_node("mac-studio");
    facts.origin_matches = None;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &facts).waiting_reason(),
        Some(WaitingReason::RemoteMismatch)
    );

    let mut facts = healthy_node("mac-studio");
    facts.base_fetchable = None;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &facts).waiting_reason(),
        Some(WaitingReason::NoEligibleNode)
    );

    let mut facts = healthy_node("mac-studio");
    facts.clean_base = None;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &facts).waiting_reason(),
        Some(WaitingReason::NoEligibleNode)
    );

    let mut facts = healthy_node("mac-studio");
    facts.owns_conflict = None;
    assert_eq!(
        evaluate_node(&policy, &project_policy, JobKind::Build, &facts).waiting_reason(),
        Some(WaitingReason::NoEligibleNode)
    );
}
