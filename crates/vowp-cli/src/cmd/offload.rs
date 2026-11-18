//! `vowp offload`: the effective policy and the read-only offload status.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §7 and §12.1. Both commands only read:
//! they never send `AssignRun`, create a worktree or start an agent. The status view joins
//! the global node/assignment rows with each project's ready jobs and evaluates the same
//! pure `vowp_core::offload` policy the dispatcher will use. No absolute local path is
//! printed in human or JSON output.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use vowp_core::offload::{
    Decision, HomeFacts, JobRequest, JobRequirements, LimitFact, NodeCapabilities, NodeFacts, NodePower, PathFacts,
    Target, ThermalState, evaluate,
};
use vowp_protocol::{
    AgentCapabilities, JobId, JobKind, JobState, NodeCard, NodeName, NodePresence, NodeStatus, Placement, ProjectName,
    ResultTransport, RunId, SecretName, SessionId, Timestamp, VowpConfig, WaitingReason,
};
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState, AssignmentFilter};
use vowp_store::{JobFilter, Store};

use crate::cli::{OffloadCommand, OffloadPolicyArgs, OffloadStatusArgs};
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Runs one offload subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &OffloadCommand) -> Result<i32, CliError> {
    match command {
        OffloadCommand::Policy(args) => policy(ctx, io, args),
        OffloadCommand::Status(args) => status(ctx, io, args),
    }
}

/// One effective project policy row.
#[derive(Debug, Serialize)]
struct ProjectPolicyRow {
    project: ProjectName,
    placement: Placement,
    home_only: bool,
    allow_kinds: Vec<JobKind>,
    deny_kinds: Vec<JobKind>,
    allow_secrets: Vec<SecretName>,
}

/// One effective node policy row.
#[derive(Debug, Serialize)]
struct NodePolicyRow {
    node: NodeName,
    max_workers: Option<u32>,
    allow_battery: bool,
    labels: Vec<String>,
}

/// The effective `vowp offload policy` document.
#[derive(Debug, Serialize)]
struct EffectivePolicy {
    enabled: bool,
    default: Placement,
    max_remote_runs: u32,
    grace_secs: u64,
    result_transport: ResultTransport,
    pause_on_home_brake: bool,
    require_clean_base: bool,
    min_disk_bytes: u64,
    projects: Vec<ProjectPolicyRow>,
    nodes: Vec<NodePolicyRow>,
}

/// Runs `offload policy`.
fn policy(ctx: &Ctx, io: &mut dyn IoStreams, args: &OffloadPolicyArgs) -> Result<i32, CliError> {
    let config = ctx.config();
    let offload = &config.offload;

    let mut project_names: BTreeSet<ProjectName> = offload.projects.keys().cloned().collect();
    if ctx.project_selected() {
        project_names.insert(ctx.project().clone());
    }
    if args.project_explicit {
        project_names.clear();
        project_names.insert(ctx.project().clone());
    }
    let projects: Vec<ProjectPolicyRow> = project_names
        .into_iter()
        .map(|name| {
            let effective = offload.project_policy(&name);
            ProjectPolicyRow {
                placement: effective.placement.unwrap_or_else(|| offload.default.clone()),
                home_only: effective.home_only,
                allow_kinds: effective.allow_kinds,
                deny_kinds: effective.deny_kinds,
                allow_secrets: effective.allow_secrets,
                project: name,
            }
        })
        .collect();

    let mut node_names: BTreeSet<NodeName> = offload.nodes.keys().cloned().collect();
    if let Ok(store) = ctx.fleet_store() {
        let now = ctx.clock().now();
        for card in store.list_node_cards(now)? {
            node_names.insert(card.name);
        }
    }
    let nodes: Vec<NodePolicyRow> = node_names
        .into_iter()
        .map(|name| {
            let effective = offload.node_policy(&name);
            NodePolicyRow {
                node: name,
                max_workers: effective.max_workers,
                allow_battery: effective.allow_battery,
                labels: effective.labels,
            }
        })
        .collect();

    let document = EffectivePolicy {
        enabled: offload.enabled,
        default: offload.default.clone(),
        max_remote_runs: offload.max_remote_runs,
        grace_secs: offload.grace_secs,
        result_transport: offload.result_transport,
        pause_on_home_brake: offload.pause_on_home_brake,
        require_clean_base: offload.require_clean_base,
        min_disk_bytes: offload.min_disk_bytes,
        projects,
        nodes,
    };

    if ctx.json() {
        print_json(io, &document)?;
    } else {
        print_policy(io, &document)?;
    }
    Ok(0)
}

/// Prints the effective policy as two aligned tables.
fn print_policy(io: &mut dyn IoStreams, document: &EffectivePolicy) -> Result<(), CliError> {
    let state = if document.enabled { "enabled" } else { "disabled" };
    writeln!(io.out(), "offload: {state}")?;
    writeln!(io.out(), "  default: {}", document.default)?;
    writeln!(io.out(), "  max_remote_runs: {}", document.max_remote_runs)?;
    writeln!(io.out(), "  grace_secs: {}", document.grace_secs)?;
    writeln!(
        io.out(),
        "  result_transport: {}",
        transport_label(document.result_transport)
    )?;
    writeln!(
        io.out(),
        "  pause_on_home_brake: {}",
        yes_no(document.pause_on_home_brake)
    )?;
    writeln!(
        io.out(),
        "  require_clean_base: {}",
        yes_no(document.require_clean_base)
    )?;
    writeln!(io.out(), "  min_disk_bytes: {}", document.min_disk_bytes)?;
    if document.projects.is_empty() {
        writeln!(io.out(), "projects: none")?;
    } else {
        let mut table = Table::new(&[
            "PROJECT",
            "PLACEMENT",
            "HOME_ONLY",
            "ALLOW_KINDS",
            "DENY_KINDS",
            "ALLOW_SECRETS",
        ]);
        for row in &document.projects {
            table.row(vec![
                row.project.to_string(),
                row.placement.to_string(),
                yes_no(row.home_only),
                kinds_label(&row.allow_kinds),
                kinds_label(&row.deny_kinds),
                secrets_label(&row.allow_secrets),
            ]);
        }
        writeln!(io.out(), "{}", table.render(false))?;
    }
    if document.nodes.is_empty() {
        writeln!(io.out(), "nodes: none")?;
    } else {
        let mut table = Table::new(&["NODE", "MAX_WORKERS", "ALLOW_BATTERY", "LABELS"]);
        for row in &document.nodes {
            table.row(vec![
                row.node.to_string(),
                row.max_workers
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                yes_no(row.allow_battery),
                if row.labels.is_empty() {
                    "-".to_owned()
                } else {
                    row.labels.join(",")
                },
            ]);
        }
        writeln!(io.out(), "{}", table.render(false))?;
    }
    Ok(())
}

/// One node row in `offload status`; never a path or secret value.
#[derive(Debug, Serialize)]
struct NodeStatusRow {
    name: NodeName,
    status: NodeStatus,
    presence: NodePresence,
    mapped: Vec<ProjectName>,
    slots_busy: Option<u32>,
    slots_max: Option<u32>,
    disk_free_bytes: Option<u64>,
    on_battery: Option<bool>,
    thermal_state: Option<String>,
    last_error: Option<String>,
}

/// One live assignment row in `offload status`; the token-free payload is not echoed.
#[derive(Debug, Serialize)]
struct AssignmentRow {
    id: String,
    run_id: RunId,
    session_id: SessionId,
    project: ProjectName,
    node: NodeName,
    epoch: u64,
    state: AgentAssignmentState,
    created_at: Timestamp,
    expires_at: Option<Timestamp>,
    detail: String,
}

/// One ready job that is not home-placed, with its evaluated reason.
#[derive(Debug, Serialize)]
struct WaitingRow {
    project: ProjectName,
    job: JobId,
    kind: JobKind,
    placement: Placement,
    waiting_reason: Option<WaitingReason>,
    detail: String,
    targets: Vec<String>,
}

/// The `vowp offload status` document.
#[derive(Debug, Serialize)]
struct OffloadStatusOutput {
    generated_at: Timestamp,
    nodes: Vec<NodeStatusRow>,
    assignments: Vec<AssignmentRow>,
    ready: Vec<WaitingRow>,
}

/// Runs `offload status`.
fn status(ctx: &Ctx, io: &mut dyn IoStreams, _args: &OffloadStatusArgs) -> Result<i32, CliError> {
    let global = ctx.fleet_store()?;
    let now = ctx.clock().now();
    let cards = global.list_node_cards(now)?;

    let active_states = [
        AgentAssignmentState::Queued,
        AgentAssignmentState::Sent,
        AgentAssignmentState::Accepted,
        AgentAssignmentState::Running,
        AgentAssignmentState::Disconnected,
    ];
    let assignments = global.list_assignments(&AssignmentFilter {
        states: active_states.to_vec(),
        ..AssignmentFilter::default()
    })?;
    let active_remote_runs = u32::try_from(assignments.len()).unwrap_or(u32::MAX);

    let nodes: Vec<NodeStatusRow> = cards.iter().map(node_status_row).collect();
    let home_facts = HomeFacts {
        brake_active: snapshot_brake_active(ctx),
        active_remote_runs,
    };

    let mut ready = Vec::new();
    for entry in global.list_project_entries()? {
        if entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        let Ok(project_store) = Store::open_read_only(ctx.paths(), Some(&entry.name)) else {
            continue;
        };
        let Ok(config) = project_config(ctx, &entry.name, &entry.path) else {
            continue;
        };
        let project_policy = config.offload.project_policy(&entry.name);
        let jobs = project_store.list_jobs(&JobFilter {
            project: Some(entry.name.clone()),
            states: vec![JobState::Ready],
            ..JobFilter::default()
        })?;
        for job in jobs {
            let job_placement = project_store.job_placement(&job.id)?;
            let placement = job_placement
                .clone()
                .or_else(|| project_policy.placement.clone())
                .unwrap_or_else(|| config.offload.default.clone());
            if placement == Placement::Home {
                continue;
            }
            let facts = node_facts(&global, &cards, &entry.name, now)?;
            let request = JobRequest {
                kind: job.kind,
                placement: job_placement,
                requires: JobRequirements::default(),
            };
            let decision = evaluate(
                &config.offload,
                &entry.name,
                &project_policy,
                &request,
                &home_facts,
                &facts,
            );
            ready.push(waiting_row(&entry.name, &job.id, job.kind, placement, &decision));
        }
    }
    ready.sort_by(|left, right| left.project.cmp(&right.project).then_with(|| left.job.cmp(&right.job)));

    let document = OffloadStatusOutput {
        generated_at: now,
        nodes,
        assignments: assignments.iter().map(assignment_row).collect(),
        ready,
    };
    if ctx.json() {
        print_json(io, &document)?;
    } else {
        print_status(io, &document, ctx.tz_offset_minutes())?;
    }
    Ok(0)
}

/// Prints the status document as three aligned tables.
fn print_status(
    io: &mut dyn IoStreams,
    document: &OffloadStatusOutput,
    tz_offset_minutes: i32,
) -> Result<(), CliError> {
    if document.nodes.is_empty() {
        writeln!(io.out(), "nodes: none")?;
    } else {
        let mut table = Table::new(&[
            "NAME",
            "STATUS",
            "PRESENCE",
            "MAPPED",
            "SLOTS",
            "DISK",
            "BATTERY",
            "LAST_ERROR",
        ]);
        for row in &document.nodes {
            let slots = match (row.slots_busy, row.slots_max) {
                (Some(busy), Some(max)) => format!("{busy}/{max}"),
                _ => "-".to_owned(),
            };
            let disk = row.disk_free_bytes.map(format_bytes).unwrap_or_else(|| "-".to_owned());
            let battery = match row.on_battery {
                Some(true) => "battery",
                Some(false) => "ac",
                None => "-",
            };
            let battery = match row.thermal_state.as_deref() {
                Some(thermal) if thermal != "nominal" && !thermal.is_empty() => {
                    format!("{battery} ({thermal})")
                }
                _ => battery.to_owned(),
            };
            table.row(vec![
                row.name.to_string(),
                format!("{:?}", row.status).to_lowercase(),
                format!("{:?}", row.presence).to_lowercase(),
                if row.mapped.is_empty() {
                    "-".to_owned()
                } else {
                    row.mapped.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
                },
                slots,
                disk,
                battery,
                row.last_error.clone().unwrap_or_else(|| "-".to_owned()),
            ]);
        }
        writeln!(io.out(), "{}", table.render(false))?;
    }

    if document.assignments.is_empty() {
        writeln!(io.out(), "assignments: none")?;
    } else {
        let mut table = Table::new(&["ID", "PROJECT", "RUN", "NODE", "STATE", "CREATED", "DETAIL"]);
        for row in &document.assignments {
            table.row(vec![
                row.id.clone(),
                row.project.to_string(),
                row.run_id.to_string(),
                row.node.to_string(),
                row.state.as_str().to_owned(),
                local_time_at(row.created_at, document.generated_at, tz_offset_minutes),
                row.detail.clone(),
            ]);
        }
        writeln!(io.out(), "{}", table.render(false))?;
    }

    if document.ready.is_empty() {
        writeln!(io.out(), "ready: none offloaded")?;
    } else {
        let mut table = Table::new(&["PROJECT", "JOB", "KIND", "PLACEMENT", "REASON", "DETAIL"]);
        for row in &document.ready {
            table.row(vec![
                row.project.to_string(),
                row.job.to_string(),
                kind_label(row.kind).to_owned(),
                row.placement.to_string(),
                row.waiting_reason.map(reason_label).unwrap_or("eligible").to_owned(),
                row.detail.clone(),
            ]);
        }
        writeln!(io.out(), "{}", table.render(false))?;
    }
    Ok(())
}

/// Builds the per-node fact set for one project.
fn node_facts(
    store: &Store,
    cards: &[NodeCard],
    project: &ProjectName,
    now: Timestamp,
) -> Result<Vec<NodeFacts>, CliError> {
    let mut facts = Vec::with_capacity(cards.len());
    for card in cards {
        let node = card.name.clone();
        let raw = store.require_node(&node)?;
        let capabilities = serde_json::from_value::<AgentCapabilities>(raw.capabilities.clone()).ok();
        let mut mapped = card.mapped.clone();
        let mut harnesses = Vec::new();
        let mut secrets = Vec::new();
        if let Some(capabilities) = capabilities {
            harnesses = capabilities.harnesses.keys().cloned().collect();
            secrets = capabilities.secret_names;
            for project in capabilities.projects {
                if !mapped.contains(&project) {
                    mapped.push(project);
                }
            }
        }
        let limits = store
            .node_limits(&node)?
            .into_iter()
            .filter(|limit| limit.resets_at.is_none_or(|resets| resets > now))
            .map(|limit| LimitFact {
                provider: limit.provider,
                kind: limit.kind,
                detail: limit.detail,
            })
            .collect();
        facts.push(NodeFacts {
            name: node.clone(),
            status: card.status,
            presence: card.presence,
            labels: card.labels.clone(),
            capabilities: NodeCapabilities {
                harnesses,
                secrets,
                projects: mapped,
            },
            capacity: card.capacity.clone(),
            power: card.capacity.as_ref().map(|capacity| NodePower {
                on_battery: capacity.on_battery.unwrap_or(false),
                thermal_state: ThermalState::from_name(capacity.thermal_state.as_deref()),
            }),
            limits,
            path: path_facts(store, &node, project, raw.is_local)?,
            origin_matches: None,
            base_fetchable: None,
            clean_base: None,
            owns_conflict: Some(false),
        });
    }
    Ok(facts)
}

/// Returns path facts for the last reported map, verifying a local node's filesystem only.
fn path_facts(
    store: &Store,
    node: &NodeName,
    project: &ProjectName,
    is_local: bool,
) -> Result<Option<PathFacts>, CliError> {
    let Some(map) = store
        .list_node_paths(node)?
        .into_iter()
        .find(|path| &path.project == project)
    else {
        return Ok(None);
    };
    if !is_local {
        // A remote map is the agent's report; its roots live on the node and cannot be
        // inspected from the home without a round trip. Presence is the reported fact.
        return Ok(Some(PathFacts {
            map_present: true,
            local_root_present: true,
            worktree_root_present: true,
            writable: true,
        }));
    }
    let local_root = Path::new(&map.local_root);
    let worktree_root = Path::new(&map.worktree_root);
    Ok(Some(PathFacts {
        map_present: true,
        local_root_present: local_root.is_dir(),
        worktree_root_present: worktree_root.is_dir(),
        writable: is_writable(local_root) && is_writable(worktree_root),
    }))
}

/// Returns whether `path` exists and is not marked read-only.
fn is_writable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.permissions().readonly())
        .unwrap_or(false)
}

/// Reads the active home brake from the derived snapshot, when it exists.
fn snapshot_brake_active(ctx: &Ctx) -> bool {
    vowp_core::read_snapshot(ctx.paths())
        .map(|snapshot| snapshot.capacity.brake.active)
        .unwrap_or(false)
}

/// Loads one project's config, preferring the already parsed selected project.
fn project_config(ctx: &Ctx, name: &ProjectName, path: &str) -> Result<VowpConfig, CliError> {
    if name == ctx.project() {
        return Ok(ctx.config().clone());
    }
    let text = std::fs::read_to_string(Path::new(path).join("vowp.toml"))?;
    Ok(VowpConfig::from_toml(&text)?)
}

/// Converts one card into its status row.
fn node_status_row(card: &NodeCard) -> NodeStatusRow {
    NodeStatusRow {
        name: card.name.clone(),
        status: card.status,
        presence: card.presence,
        mapped: card.mapped.clone(),
        slots_busy: card.capacity.as_ref().map(|capacity| capacity.busy_slots),
        slots_max: card.capacity.as_ref().map(|capacity| capacity.max_slots),
        disk_free_bytes: card.capacity.as_ref().map(|capacity| capacity.disk_free_bytes),
        on_battery: card.capacity.as_ref().and_then(|capacity| capacity.on_battery),
        thermal_state: card
            .capacity
            .as_ref()
            .and_then(|capacity| capacity.thermal_state.clone()),
        last_error: card.last_error.clone(),
    }
}

/// Converts one assignment into its public row.
fn assignment_row(assignment: &AgentAssignment) -> AssignmentRow {
    AssignmentRow {
        id: assignment.id.clone(),
        run_id: assignment.run_id.clone(),
        session_id: assignment.session_id.clone(),
        project: assignment.project.clone(),
        node: assignment.node.clone(),
        epoch: assignment.epoch,
        state: assignment.state,
        created_at: assignment.created_at,
        expires_at: assignment.expires_at,
        detail: assignment.detail.clone(),
    }
}

/// Builds one ready-job row from the evaluator's answer.
fn waiting_row(
    project: &ProjectName,
    job: &JobId,
    kind: JobKind,
    placement: Placement,
    decision: &Decision,
) -> WaitingRow {
    let targets: Vec<String> = decision.targets().iter().map(target_label).collect();
    let detail = match decision {
        Decision::Eligible { .. } => format!("eligible: {}", targets.join(", ")),
        Decision::Waiting { detail, .. } => detail.clone(),
    };
    WaitingRow {
        project: project.clone(),
        job: job.clone(),
        kind,
        placement,
        waiting_reason: decision.waiting_reason(),
        detail,
        targets,
    }
}

/// Formats one target without a local path.
fn target_label(target: &Target) -> String {
    match target {
        Target::Home => "home".to_owned(),
        Target::Node(node) => format!("node:{node}"),
    }
}

/// Formats a byte count in GiB with one decimal place.
fn format_bytes(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Returns `yes` or `no`.
fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_owned()
}

/// Returns the snake_case wire label of a result transport.
fn transport_label(transport: ResultTransport) -> &'static str {
    match transport {
        ResultTransport::Push => "push",
        ResultTransport::Bundle => "bundle",
    }
}

/// Joins job kinds for a table cell.
fn kinds_label(kinds: &[JobKind]) -> String {
    if kinds.is_empty() {
        "-".to_owned()
    } else {
        kinds.iter().map(|kind| kind_label(*kind)).collect::<Vec<_>>().join(",")
    }
}

/// Joins secret names for a table cell.
fn secrets_label(names: &[SecretName]) -> String {
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
    }
}

/// Returns the snake_case label of a job kind.
fn kind_label(kind: JobKind) -> &'static str {
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

/// Returns the §7.5 wire label of a waiting reason.
fn reason_label(reason: WaitingReason) -> &'static str {
    match reason {
        WaitingReason::NodeOffline => "node_offline",
        WaitingReason::NodeRevoked => "node_revoked",
        WaitingReason::NodeBusy => "node_busy",
        WaitingReason::NodeBattery => "node_battery",
        WaitingReason::NodeDiskLow => "node_disk_low",
        WaitingReason::NodeLimit => "node_limit",
        WaitingReason::PathUnmapped => "path_unmapped",
        WaitingReason::PathMissing => "path_missing",
        WaitingReason::MissingSecret => "missing_secret",
        WaitingReason::HomeOnlySecret => "home_only_secret",
        WaitingReason::KindNotAllowed => "kind_not_allowed",
        WaitingReason::ProjectHomeOnly => "project_home_only",
        WaitingReason::RemoteMismatch => "remote_mismatch",
        WaitingReason::NoEligibleNode => "no_eligible_node",
    }
}
