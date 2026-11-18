//! The home-side offload dispatcher: queue, claim, send and apply agent frames.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §6.6, §7.1–§7.5, §8.3–§8.4 and
//! §9. The dispatcher is the home half of the run lifecycle. It turns a ready, eligible
//! job into a durable `agent_assignments` row, hands `AssignRun` frames to the gateway
//! when the node has a free slot, and keeps the assignment and job rows in step with the
//! agent's `RunAccepted`, `RunRejected`, `RunState` and `Resume` frames. It never creates
//! the node worktree, starts a harness or pushes a branch; those belong to the agent
//! executor.
//!
//! A run lease is separate from the chat surface lease. The home mints a short-lived
//! token when it sends or resumes `AssignRun`, stores only its SHA-256 in `leases`, and
//! never writes the token into the durable payload, an error or a log. A rejected job
//! stays `ready` with a `waiting:` note: the waiting reason and detail are part of the
//! read model, not a failure.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use vowp_protocol::{
    Actor, AgentFrame, AssignmentPayload, EventBody, EventSource, Executor, ExecutorRef, HarnessKind, Job, JobId,
    JobState, Lease, NodeCard, NodeName, PathRef, Placement, ProjectName, RemoteRunState, ResultTransport, RunId,
    RunOutcome, RunPurpose, RunRejectReason, RunResume, RunResumeResult, Session, SessionCreated, SessionId,
    SessionRole, SessionState, SurfaceKind, SurfaceResume, SurfaceResumeResult, SurfaceState, Timestamp, VowpConfig,
    WaitingReason,
};
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState, AssignmentFilter};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{JobFilter, RunFinish, StateChange, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::offload::{
    Decision, HomeFacts, JobRequest, JobRequirements, LimitFact, NodeCapabilities, NodeFacts, NodePower, PathFacts,
    Target, ThermalState, evaluate,
};
use crate::session::remote::{BeginTurn, begin_turn};

/// How long a run lease minted for one `AssignRun` stays current.
///
/// The agent protocol has no separate run-lease heartbeat in this slice, so the lease is
/// short enough that an abandoned dispatch is fenced quickly and long enough that a normal
/// reconnect can reuse it. A duplicate `AssignRun` or a `Resume` re-mints the token.
pub const RUN_LEASE_TTL: Duration = Duration::from_secs(600);

/// Per-run spool cap from §6.5 (512 MiB).
pub const SPOOL_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// How long a rejected assignment suppresses an immediate re-dispatch of the same job.
pub const REJECT_RETRY: Duration = Duration::from_secs(60);

/// The `jobs.note` prefix the dispatcher owns, so it never overwrites another loop's note.
pub const WAITING_PREFIX: &str = "waiting: offload ";

/// One queued or claimed assignment and its wire frame.
#[derive(Debug, Clone)]
pub struct ClaimedAssignment {
    /// The durable row after the claim or resume.
    pub assignment: AgentAssignment,
    /// The `AssignRun` frame to send on the node's live connection.
    pub frame: AgentFrame,
}

/// One ready job that stays home because the policy found no node target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingJob {
    /// Project the job belongs to.
    pub project: ProjectName,
    /// Ready job that is waiting.
    pub job: JobId,
    /// First §7.5 reason.
    pub reason: WaitingReason,
    /// Human-readable, path-free detail.
    pub detail: String,
    /// Node the reason is about, when it is node-specific.
    pub node: Option<NodeName>,
    /// Effective placement that produced the decision.
    pub placement: Placement,
}

/// What one enqueue pass did.
#[derive(Debug, Clone, Default)]
pub struct EnqueueReport {
    /// Assignments inserted as `queued`.
    pub enqueued: Vec<AgentAssignment>,
    /// Ready jobs left home with a waiting reason.
    pub waiting: Vec<WaitingJob>,
    /// Ready jobs whose placement is `home`.
    pub home: usize,
}

/// What one resume frame produced.
#[derive(Debug, Clone)]
pub struct ResumeOutcome {
    /// The `ResumeOk` frame to send first.
    pub resume_ok: AgentFrame,
    /// Fresh `AssignRun` frames for accepted runs.
    pub assignments: Vec<ClaimedAssignment>,
    /// Runs the agent still holds but the home fenced; send `RunLost` for each.
    pub lost: Vec<(RunId, String)>,
}

/// Scans the ready jobs of every registered project and queues each eligible one.
///
/// The effective placement is the job side-table value, then the project policy, then the
/// fleet default. `home` is skipped: that job is the L2 loop's work. A job with a node
/// target must not already have a live assignment (or a fresh rejection), so a heartbeat
/// cannot create a second queue row for the same job, run or epoch. A job that is not
/// eligible stays `ready`; its reason and detail are written to the job note for the read
/// model and returned in [`EnqueueReport::waiting`].
pub fn enqueue_ready_jobs(deps: &Deps, store: &Store, now: Timestamp) -> Result<EnqueueReport, CoreError> {
    let config = deps.config_now();
    let paths = store
        .paths()
        .ok_or_else(|| CoreError::Invalid("offload dispatch needs a file-backed store".to_owned()))?;
    let cards = store.list_node_cards(now)?;
    let mut active_runs = active_assignment_count(store)?;
    let mut report = EnqueueReport::default();

    for entry in store.list_project_entries()? {
        if entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        let project = store.require_project(&entry.name)?;
        let project_config = project_config(&config, &project);
        // A gateway on a multi-project home runs with the node-level config, so
        // `offload.enabled` is a per-project decision here. A disabled project's ready
        // jobs are the L2 loop's work, not a waiting reason.
        if !project_config.offload.enabled {
            continue;
        }
        let project_policy = project_config.offload.project_policy(&entry.name);
        let project_store = Store::open_with_project(paths, &entry.name)?.with_clock(deps.clock.clone());
        let jobs = project_store.list_jobs(&JobFilter {
            project: Some(entry.name.clone()),
            states: vec![JobState::Ready],
            ..JobFilter::default()
        })?;
        if jobs.is_empty() {
            continue;
        }
        let mut has_offload_candidate = false;
        for job in &jobs {
            if effective_placement(&project_store, job, &project_config)? != Placement::Home {
                has_offload_candidate = true;
                break;
            }
        }
        if !has_offload_candidate {
            report.home += jobs.len();
            continue;
        }

        // The base commit is a home fact: the node starts its detached worktree from it.
        // Without a resolvable HEAD there is nothing durable to send, so the jobs wait.
        let base_commit = match deps.git.head_sha(Path::new(&entry.path), "HEAD") {
            Ok(commit) if !commit.is_empty() => commit,
            _ => {
                for job in &jobs {
                    let placement = effective_placement(&project_store, job, &project_config)?;
                    if placement == Placement::Home {
                        report.home += 1;
                        continue;
                    }
                    let detail = format!("the project checkout for {} has no HEAD commit", entry.name);
                    record_waiting(&project_store, job, WaitingReason::NoEligibleNode, &detail)?;
                    report.waiting.push(WaitingJob {
                        project: entry.name.clone(),
                        job: job.id.clone(),
                        reason: WaitingReason::NoEligibleNode,
                        detail,
                        node: None,
                        placement,
                    });
                }
                continue;
            }
        };
        let remote_url = entry.remote_url.as_deref().map(strip_userinfo);
        let facts = cards
            .iter()
            .map(|card| node_facts(deps, store, card, &entry.name, &base_commit, remote_url.as_deref()))
            .collect::<Result<Vec<_>, _>>()?;
        let brake_active = home_brake_active(deps);

        for job in &jobs {
            // The fleet cap is checked against the count after every insert earlier in this
            // pass, so a batch of ready jobs never all sees the pre-pass count.
            let home = HomeFacts {
                brake_active,
                active_remote_runs: active_runs,
            };
            let job_placement = project_store.job_placement(&job.id)?;
            let placement = effective_placement(&project_store, job, &project_config)?;
            if placement == Placement::Home {
                report.home += 1;
                continue;
            }
            if has_live_assignment(store, &job.id)? || is_in_rejection_cooldown(store, &job.id, now)? {
                continue;
            }
            let Some(profile) = project_config.profiles.get(&job.profile) else {
                let detail = format!("profile {} is not configured", job.profile);
                record_waiting(&project_store, job, WaitingReason::NoEligibleNode, &detail)?;
                report.waiting.push(WaitingJob {
                    project: entry.name.clone(),
                    job: job.id.clone(),
                    reason: WaitingReason::NoEligibleNode,
                    detail,
                    node: None,
                    placement,
                });
                continue;
            };
            let Some((_rung, executor)) = profile.pick(&HashMap::new(), now) else {
                let detail = format!("profile {} has no available executor", job.profile);
                record_waiting(&project_store, job, WaitingReason::NoEligibleNode, &detail)?;
                report.waiting.push(WaitingJob {
                    project: entry.name.clone(),
                    job: job.id.clone(),
                    reason: WaitingReason::NoEligibleNode,
                    detail,
                    node: None,
                    placement,
                });
                continue;
            };
            let executor = executor.clone();
            let env_names = harness_env_names(&project_config, &executor);
            let secret_names = env_names
                .iter()
                .filter_map(|name| vowp_protocol::SecretName::parse(name).ok())
                .collect::<Vec<_>>();
            let request = JobRequest {
                kind: job.kind,
                placement: job_placement,
                requires: JobRequirements {
                    labels: Vec::new(),
                    harnesses: vec![executor.harness.as_str().to_owned()],
                    secrets: secret_names.clone(),
                    provider: Some(executor.provider()),
                },
            };
            let decision = evaluate(
                &project_config.offload,
                &entry.name,
                &project_policy,
                &request,
                &home,
                &facts,
            );
            if let Decision::Waiting { reason, detail, node } = &decision {
                record_waiting(&project_store, job, *reason, detail)?;
                report.waiting.push(WaitingJob {
                    project: entry.name.clone(),
                    job: job.id.clone(),
                    reason: *reason,
                    detail: detail.clone(),
                    node: node.clone(),
                    placement,
                });
                continue;
            }
            let candidates = decision
                .targets()
                .iter()
                .filter_map(|target| match target {
                    Target::Node(node) => Some(node.clone()),
                    Target::Home => None,
                })
                .collect::<Vec<_>>();
            let Some(target) = choose_target(profile.node.as_ref(), &candidates) else {
                // `any` keeps the home as a target; the L2 loop runs the job there.
                report.home += 1;
                continue;
            };
            let purpose = crate::loops::dispatch::purpose_for_dispatch(&project_store, job)
                .unwrap_or_else(|_| crate::loops::l2_worker::purpose_for(job));
            let prompt = seed_prompt(job);
            let (session_id, run_id) =
                match open_agent_run(deps, store, &project_store, job, &target, &executor, purpose, &prompt) {
                    Ok(ids) => ids,
                    Err(error) => {
                        let detail = format!("could not open the remote run: {:?}", error.kind());
                        record_waiting(&project_store, job, WaitingReason::NoEligibleNode, &detail)?;
                        report.waiting.push(WaitingJob {
                            project: entry.name.clone(),
                            job: job.id.clone(),
                            reason: WaitingReason::NoEligibleNode,
                            detail,
                            node: Some(target),
                            placement,
                        });
                        continue;
                    }
                };
            let result_transport = project_config.offload.result_transport;
            let payload = AssignmentPayload {
                assignment_id: format!("assignment-{run_id}"),
                run_id: run_id.clone(),
                session_id: session_id.clone(),
                project: entry.name.clone(),
                job: job.id.clone(),
                executor: executor.clone(),
                purpose,
                prompt,
                cwd_ref: PathRef::Worktree {
                    project: entry.name.clone(),
                    job: job.id.clone(),
                },
                base_commit: base_commit.clone(),
                remote: project_config.project.remote.clone(),
                // A bundle assignment never pushes: it carries no remote URL, so the node
                // neither needs nor uses a git credential. A push assignment keeps the
                // stripped project URL the agent compares with its own push URL.
                remote_url: match result_transport {
                    ResultTransport::Push => remote_url.clone().unwrap_or_default(),
                    ResultTransport::Bundle => String::new(),
                },
                result_transport,
                env_names,
                secret_names,
                epoch: 1,
                deadline: None,
                spool_cap_bytes: SPOOL_CAP_BYTES,
            };
            let assignment = AgentAssignment {
                id: payload.assignment_id.clone(),
                run_id: run_id.clone(),
                session_id: session_id.clone(),
                project: entry.name.clone(),
                node: target,
                epoch: payload.epoch,
                state: AgentAssignmentState::Queued,
                payload,
                created_at: now,
                claimed_at: None,
                expires_at: None,
                detail: String::new(),
            };
            match store.insert_assignment(&assignment) {
                Ok(()) => {
                    clear_waiting(&project_store, &job.id)?;
                    active_runs = active_runs.saturating_add(1);
                    report.enqueued.push(assignment);
                }
                Err(vowp_store::StoreError::Conflict(_)) => {
                    // Another pass won the race. The existing row is authoritative; the
                    // run opened above is closed without a second queue entry.
                    finish_open_run(
                        deps,
                        &project_store,
                        &run_id,
                        RunOutcome::Error,
                        "a duplicate assignment was refused",
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(report)
}

/// Claims up to the node's free worker/verifier slots and returns their `AssignRun` frames.
///
/// The slot limit is `[offload.nodes.<name>].max_workers` when set, then the node's reported
/// `max_slots`; a caller may pass an explicit cap. The claim is also bounded by the fleet-wide
/// `offload.max_remote_runs` minus the assignments that already occupy a run slot on every
/// node. A row that was selected but whose lease cannot be issued is returned to `queued` with
/// a retryable detail; it is never dropped.
pub fn claim_for_node(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    now: Timestamp,
) -> Result<Vec<ClaimedAssignment>, CoreError> {
    claim_for_node_with_slots(deps, store, node, None, now)
}

/// Claims for `node` with an explicit slot cap.
pub fn claim_for_node_with_slots(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    max_slots: Option<u32>,
    now: Timestamp,
) -> Result<Vec<ClaimedAssignment>, CoreError> {
    let Some(card) = store.node_card(node, now)? else {
        return Ok(Vec::new());
    };
    let config = deps.config_now();
    let configured = config.offload.node_policy(node).max_workers;
    let max_remote_runs = config.offload.max_remote_runs;
    let max_slots = max_slots
        .or(configured)
        .or_else(|| card.capacity.as_ref().map(|capacity| capacity.max_slots))
        .unwrap_or(0);
    let occupied = store
        .list_assignments(&AssignmentFilter {
            node: Some(node.clone()),
            states: dispatch_states().to_vec(),
            ..AssignmentFilter::default()
        })?
        .len();
    let node_free = max_slots.saturating_sub(u32::try_from(occupied).unwrap_or(u32::MAX));
    let fleet_free = max_remote_runs.saturating_sub(active_assignment_count(store)?);
    let free = node_free.min(fleet_free);
    if free == 0 {
        return Ok(Vec::new());
    }
    let claimed = store.claim_assignments(node, free, now, now.plus(RUN_LEASE_TTL))?;
    let mut frames = Vec::with_capacity(claimed.len());
    for assignment in claimed {
        match issue_run_lease(store, &assignment, now) {
            Ok((token, expires_at)) => frames.push(ClaimedAssignment {
                frame: assign_frame(&assignment, token, expires_at),
                assignment,
            }),
            Err(_) => {
                store.advance_assignment(
                    &assignment.id,
                    AgentAssignmentState::Queued,
                    None,
                    "the run lease could not be issued; the assignment stays queued",
                )?;
            }
        }
    }
    Ok(frames)
}

/// Returns claimed rows whose `AssignRun` could not be written to the queue.
///
/// A transport failure never drops a durable row: the unsent assignments go back to
/// `queued`, their just-minted lease is released, and the next connection can claim them.
pub fn return_claimed_to_queue(
    store: &Store,
    assignments: &[ClaimedAssignment],
    detail: &str,
) -> Result<(), CoreError> {
    for claimed in assignments {
        if claimed.assignment.state == AgentAssignmentState::Sent {
            store.advance_assignment(&claimed.assignment.id, AgentAssignmentState::Queued, None, detail)?;
            let _ = release_run_lease(store, &claimed.assignment.session_id);
        }
    }
    Ok(())
}

/// Applies a `RunAccepted` frame: the assignment moves to `accepted` and the job runs.
pub fn accept(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    assignment_id: &str,
    run_id: &RunId,
    epoch: u64,
    now: Timestamp,
) -> Result<AgentAssignment, CoreError> {
    let assignment = find_assignment(store, node, run_id, epoch)?;
    if assignment.id != assignment_id {
        return Err(CoreError::Invalid(format!(
            "assignment id {assignment_id} does not match run {run_id} epoch {epoch}"
        )));
    }
    // A reconnect re-sends `AssignRun` for the same epoch and the agent re-accepts the
    // run it already holds. Repeats are idempotent, and a `disconnected` row is that
    // same live run after a transport drop, so it returns to `running` rather than
    // failing a lifecycle edge the durable state does not allow. Any other state means
    // the home already fenced or finished this run.
    let next = match assignment.state {
        AgentAssignmentState::Sent => AgentAssignmentState::Accepted,
        AgentAssignmentState::Accepted | AgentAssignmentState::Running => assignment.state,
        AgentAssignmentState::Disconnected => AgentAssignmentState::Running,
        state => {
            return Err(CoreError::Refused(format!(
                "run {run_id} epoch {epoch} is {} and cannot be accepted",
                state.as_str()
            )));
        }
    };
    let detail = if next == assignment.state {
        assignment.detail.clone()
    } else if assignment.state == AgentAssignmentState::Disconnected {
        "agent re-accepted the assignment after a reconnect".to_owned()
    } else {
        "agent accepted the assignment".to_owned()
    };
    let assignment = store.advance_assignment(&assignment.id, next, Some(now), &detail)?;
    let assignment = store.renew_assignment(&assignment.id, now, now.plus(RUN_LEASE_TTL), &assignment.detail)?;
    let project_store = project_store(deps, store, &assignment.project)?;
    clear_waiting(&project_store, &assignment.payload.job)?;
    project_store.set_job_state(
        &assignment.payload.job,
        JobState::Running,
        &StateChange {
            note: None,
            parked_reason: None,
        },
    )?;
    Ok(assignment)
}

/// Applies a `RunRejected` frame: the job stays ready with the refusal reason.
#[allow(clippy::too_many_arguments)]
pub fn reject(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    assignment_id: &str,
    run_id: &RunId,
    epoch: u64,
    reason: RunRejectReason,
    detail: &str,
    now: Timestamp,
) -> Result<AgentAssignment, CoreError> {
    let assignment = find_assignment(store, node, run_id, epoch)?;
    if assignment.id != assignment_id {
        return Err(CoreError::Invalid(format!(
            "assignment id {assignment_id} does not match run {run_id} epoch {epoch}"
        )));
    }
    let detail = scrub(deps, detail);
    let event = format!("rejected {}: {detail}", reject_reason_name(reason));
    let assignment = store.advance_assignment(&assignment.id, AgentAssignmentState::Rejected, Some(now), &event)?;
    let project_store = project_store(deps, store, &assignment.project)?;
    finish_open_run(
        deps,
        &project_store,
        &assignment.run_id,
        RunOutcome::Error,
        &format!("the agent rejected the assignment: {event}"),
    )?;
    release_run_lease(store, &assignment.session_id)?;
    let job = project_store.require_job(&assignment.payload.job)?;
    if job.state == JobState::Running {
        project_store.set_job_state(
            &assignment.payload.job,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )?;
    }
    if !job.state.is_terminal() && job.state != JobState::Parked {
        record_waiting_note(
            &project_store,
            &assignment.payload.job,
            &format!("rejected {}: {detail}", reject_reason_name(reason)),
        )?;
    }
    Ok(store.set_assignment_expiry(&assignment.id, now.plus(REJECT_RETRY), &assignment.detail)?)
}

/// Applies a `RunState` frame without changing the payload or epoch.
#[allow(clippy::too_many_arguments)]
pub fn update_state(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    run_id: &RunId,
    epoch: u64,
    state: RemoteRunState,
    spool_bytes: u64,
    head_commit: Option<&str>,
    detail: &str,
    now: Timestamp,
) -> Result<AgentAssignment, CoreError> {
    let assignment = find_assignment(store, node, run_id, epoch)?;
    let detail = scrub(deps, detail);
    let head = head_commit
        .map(|head| format!(" head {}", short_hash(head)))
        .unwrap_or_default();
    let event = format!("{state:?} spool {spool_bytes} bytes{head}: {detail}").to_lowercase();
    let next = match state {
        RemoteRunState::Running | RemoteRunState::Finishing => AgentAssignmentState::Running,
        RemoteRunState::Disconnected => AgentAssignmentState::Disconnected,
    };
    let assignment = store.advance_assignment(&assignment.id, next, Some(now), &event)?;
    let assignment = store.renew_assignment(&assignment.id, now, now.plus(RUN_LEASE_TTL), &assignment.detail)?;
    // Keep the full reported head and spool size durable even if this connection drops
    // before the terminal summary arrives.
    super::result::record_reported(store, &assignment, head_commit, spool_bytes)?;
    let project_store = project_store(deps, store, &assignment.project)?;
    if next == AgentAssignmentState::Running {
        clear_waiting(&project_store, &assignment.payload.job)?;
        project_store.set_job_state(
            &assignment.payload.job,
            JobState::Running,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )?;
    }
    Ok(assignment)
}

/// Fences one run and marks its assignment `lost`.
///
/// An expired lease, a `RunLost` decision from the home, or a revoked node comes here. The
/// open run is closed with `outcome=error`, its lease is released, and the job stays ready
/// for a retry with a waiting note.
pub fn lose(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    run_id: &RunId,
    epoch: u64,
    reason: &str,
    now: Timestamp,
) -> Result<AgentAssignment, CoreError> {
    let assignment = find_assignment(store, node, run_id, epoch)?;
    if assignment.state == AgentAssignmentState::Lost {
        return Ok(assignment);
    }
    let reason = scrub(deps, reason);
    let assignment = store.advance_assignment(
        &assignment.id,
        AgentAssignmentState::Lost,
        Some(now),
        &format!("fenced: {reason}"),
    )?;
    let project_store = project_store(deps, store, &assignment.project)?;
    finish_open_run(
        deps,
        &project_store,
        &assignment.run_id,
        RunOutcome::Error,
        &format!("remote run lost: {reason}"),
    )?;
    release_run_lease(store, &assignment.session_id)?;
    let job = project_store.require_job(&assignment.payload.job)?;
    if job.state == JobState::Running {
        project_store.set_job_state(
            &assignment.payload.job,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )?;
    }
    if !job.state.is_terminal() && job.state != JobState::Parked {
        record_waiting_note(&project_store, &assignment.payload.job, &format!("run lost: {reason}"))?;
    }
    // A fenced bundle transfer cannot resume: the partial file is a staging artifact, not
    // the run's log or worktree. Its removal matches §5.5 (a fenced run removes it).
    super::result::discard_staged_bundle(deps, &assignment.run_id);
    Ok(assignment)
}

/// Fences every active assignment whose run lease has expired.
pub fn sweep_expired(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    now: Timestamp,
) -> Result<Vec<AgentAssignment>, CoreError> {
    let active = store.list_assignments(&AssignmentFilter {
        node: Some(node.clone()),
        states: dispatch_states().to_vec(),
        ..AssignmentFilter::default()
    })?;
    let mut lost = Vec::new();
    for assignment in active {
        if assignment.expires_at.is_some_and(|expires| expires <= now) {
            lost.push(lose(
                deps,
                store,
                node,
                &assignment.run_id,
                assignment.epoch,
                "the run lease expired",
                now,
            )?);
        }
    }
    Ok(lost)
}

/// Re-sends one active assignment for a duplicate `AssignRun` or a reconnect.
///
/// The payload and epoch are unchanged; only a fresh lease token and expiry are minted.
/// `None` means the row is terminal or unknown; an expired lease fences it first.
pub fn resume_assignment(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    run_id: &RunId,
    epoch: u64,
    now: Timestamp,
) -> Result<Option<ClaimedAssignment>, CoreError> {
    let Some(assignment) = store
        .assignments_for_run(run_id)?
        .into_iter()
        .find(|assignment| assignment.epoch == epoch)
    else {
        return Ok(None);
    };
    if assignment.node != *node {
        return Err(CoreError::Refused(format!(
            "run {run_id} belongs to node {}, not {node}",
            assignment.node
        )));
    }
    if assignment.state.is_terminal() {
        return Ok(None);
    }
    if assignment.expires_at.is_some_and(|expires| expires <= now) {
        lose(deps, store, node, run_id, epoch, "the run lease expired", now)?;
        return Ok(None);
    }
    let (token, expires_at) = issue_run_lease(store, &assignment, now)?;
    let assignment = store.renew_assignment(&assignment.id, now, expires_at, "resumed on a live connection")?;
    Ok(Some(ClaimedAssignment {
        frame: assign_frame(&assignment, token, expires_at),
        assignment,
    }))
}

/// Replaces a run's active assignment with a higher epoch.
///
/// The durable payload is copied from the newest row with the new epoch and assignment id.
/// Older rows for the same run become `lost` but the run itself stays open, because the
/// higher epoch supersedes the lower dispatch rather than ending the work. A lower or equal
/// epoch is refused.
pub fn supersede_assignment(
    store: &Store,
    run_id: &RunId,
    epoch: u64,
    now: Timestamp,
) -> Result<AgentAssignment, CoreError> {
    let rows = store.assignments_for_run(run_id)?;
    let Some(latest) = rows.last() else {
        return Err(CoreError::NotFound(format!("run {run_id} has no assignment")));
    };
    if epoch <= latest.epoch {
        return Err(CoreError::Invalid(format!(
            "epoch {epoch} does not supersede epoch {} for run {run_id}",
            latest.epoch
        )));
    }
    let id = format!("assignment-{}", RunId::new());
    let mut payload = latest.payload.clone();
    payload.assignment_id = id.clone();
    payload.epoch = epoch;
    let assignment = AgentAssignment {
        id,
        run_id: run_id.clone(),
        session_id: latest.session_id.clone(),
        project: latest.project.clone(),
        node: latest.node.clone(),
        epoch,
        state: AgentAssignmentState::Queued,
        payload,
        created_at: now,
        claimed_at: None,
        expires_at: None,
        detail: format!("supersedes epoch {}", latest.epoch),
    };
    store.insert_assignment(&assignment)?;
    for old in &rows {
        if old.state.is_active() {
            store.advance_assignment(
                &old.id,
                AgentAssignmentState::Lost,
                None,
                &format!("superseded by epoch {epoch}"),
            )?;
        }
    }
    Ok(assignment)
}

/// Handles one `Resume` frame: per-run and per-surface tails, plus fresh `AssignRun` frames.
pub fn resume_connection(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    runs: &[RunResume],
    surfaces: &[SurfaceResume],
    now: Timestamp,
) -> Result<ResumeOutcome, CoreError> {
    let expired = sweep_expired(deps, store, node, now)?;
    let mut run_results = Vec::with_capacity(runs.len());
    let mut assignments = Vec::new();
    let mut lost = expired
        .iter()
        .map(|assignment| (assignment.run_id.clone(), "the run lease expired".to_owned()))
        .collect::<Vec<_>>();
    for resume in runs {
        let tail_seq = store
            .assignments_for_run(&resume.run_id)?
            .into_iter()
            .find(|assignment| assignment.epoch == resume.epoch)
            .and_then(|assignment| store.session(&assignment.session_id).ok().flatten())
            .map(|session| session.last_seq)
            .unwrap_or(resume.last_acked);
        match resume_assignment(deps, store, node, &resume.run_id, resume.epoch, now) {
            Ok(Some(claimed)) => {
                run_results.push(RunResumeResult {
                    run_id: resume.run_id.clone(),
                    epoch: resume.epoch,
                    accepted: true,
                    reason: None,
                    tail_seq,
                });
                assignments.push(claimed);
            }
            Ok(None) => {
                run_results.push(RunResumeResult {
                    run_id: resume.run_id.clone(),
                    epoch: resume.epoch,
                    accepted: false,
                    reason: Some("the run is not current on this node".to_owned()),
                    tail_seq,
                });
                lost.push((resume.run_id.clone(), "the run is not current on this node".to_owned()));
            }
            Err(error) => {
                run_results.push(RunResumeResult {
                    run_id: resume.run_id.clone(),
                    epoch: resume.epoch,
                    accepted: false,
                    reason: Some(error.to_string()),
                    tail_seq,
                });
                lost.push((resume.run_id.clone(), error.to_string()));
            }
        }
    }
    let surface_results = surfaces
        .iter()
        .map(|resume| resume_surface(store, node, resume))
        .collect::<Result<Vec<_>, _>>()?;
    lost.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    lost.dedup_by(|left, right| left.0 == right.0);
    Ok(ResumeOutcome {
        resume_ok: AgentFrame::ResumeOk {
            runs: run_results,
            surfaces: surface_results,
        },
        assignments,
        lost,
    })
}

/// Sends `CancelRun` for each active assignment of a cancelled or parked job.
///
/// The run is closed and its lease released before the frame is returned, so the durable
/// state never waits on the transport.
pub fn cancel_job(
    deps: &Deps,
    store: &Store,
    job: &JobId,
    reason: &str,
    now: Timestamp,
) -> Result<Vec<AgentFrame>, CoreError> {
    let mut frames = Vec::new();
    for assignment in store.assignments_for_job(job)? {
        if !assignment.state.is_active() {
            continue;
        }
        let detail = format!("cancelled: {}", scrub(deps, reason));
        let assignment =
            store.advance_assignment(&assignment.id, AgentAssignmentState::Cancelled, Some(now), &detail)?;
        let project_store = project_store(deps, store, &assignment.project)?;
        finish_open_run(deps, &project_store, &assignment.run_id, RunOutcome::Killed, &detail)?;
        release_run_lease(store, &assignment.session_id)?;
        frames.push(AgentFrame::CancelRun {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            reason: reason.to_owned(),
        });
    }
    Ok(frames)
}

/// Returns the `Cleanup` frame for a terminal assignment.
///
/// The executor removes the worktree; the home only names the `PathRef`. `None` means the
/// assignment is not terminal yet or does not exist.
pub fn cleanup_assignment(store: &Store, assignment_id: &str) -> Result<Option<AgentFrame>, CoreError> {
    let Some(assignment) = store.assignment(assignment_id)? else {
        return Ok(None);
    };
    if !assignment.state.is_terminal() {
        return Ok(None);
    }
    Ok(Some(AgentFrame::Cleanup {
        run_id: assignment.run_id.clone(),
        job: assignment.payload.job.clone(),
        worktree: PathRef::Worktree {
            project: assignment.project.clone(),
            job: assignment.payload.job.clone(),
        },
    }))
}

/// Marks a superseded or closed connection's active assignments `disconnected`.
///
/// The runs are not fenced: a reconnect may still resume them within the grace period. A
/// `sent` row has not been accepted yet, so it stays `sent` and is picked up by `Resume`.
pub fn disconnect_node(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    reason: &str,
    now: Timestamp,
) -> Result<Vec<AgentAssignment>, CoreError> {
    let active = store.list_assignments(&AssignmentFilter {
        node: Some(node.clone()),
        states: dispatch_states().to_vec(),
        ..AssignmentFilter::default()
    })?;
    let mut changed = Vec::new();
    for assignment in active {
        if matches!(
            assignment.state,
            AgentAssignmentState::Accepted | AgentAssignmentState::Running
        ) {
            changed.push(store.advance_assignment(
                &assignment.id,
                AgentAssignmentState::Disconnected,
                Some(now),
                &format!("connection closed: {}", scrub(deps, reason)),
            )?);
        }
    }
    Ok(changed)
}

/// Fences every active assignment of a revoked node.
pub fn fence_node(
    deps: &Deps,
    store: &Store,
    node: &NodeName,
    reason: &str,
    now: Timestamp,
) -> Result<Vec<AgentAssignment>, CoreError> {
    let active = store.list_assignments(&AssignmentFilter {
        node: Some(node.clone()),
        states: dispatch_states().to_vec(),
        ..AssignmentFilter::default()
    })?;
    let mut lost = Vec::new();
    for assignment in active {
        lost.push(lose(
            deps,
            store,
            node,
            &assignment.run_id,
            assignment.epoch,
            reason,
            now,
        )?);
    }
    Ok(lost)
}

/// Returns whether `job` has an assignment that still holds a run or a slot.
pub fn has_live_assignment(store: &Store, job: &JobId) -> Result<bool, CoreError> {
    Ok(store
        .assignments_for_job(job)?
        .iter()
        .any(|assignment| assignment.state.is_active()))
}

/// Counts assignments that occupy a fleet or node slot.
pub fn active_assignment_count(store: &Store) -> Result<u32, CoreError> {
    let active = store.list_assignments(&AssignmentFilter {
        states: dispatch_states().to_vec(),
        ..AssignmentFilter::default()
    })?;
    Ok(u32::try_from(active.len()).unwrap_or(u32::MAX))
}

/// The assignment states that hold a run or a slot (queued rows do not occupy a slot yet).
fn dispatch_states() -> &'static [AgentAssignmentState] {
    &[
        AgentAssignmentState::Sent,
        AgentAssignmentState::Accepted,
        AgentAssignmentState::Running,
        AgentAssignmentState::Disconnected,
    ]
}

/// Returns whether the newest rejected assignment still suppresses a retry.
fn is_in_rejection_cooldown(store: &Store, job: &JobId, now: Timestamp) -> Result<bool, CoreError> {
    Ok(store.assignments_for_job(job)?.iter().any(|assignment| {
        assignment.state == AgentAssignmentState::Rejected && assignment.expires_at.is_some_and(|expires| expires > now)
    }))
}

/// Resolves a run/epoch assignment and validates the connection node.
fn find_assignment(store: &Store, node: &NodeName, run_id: &RunId, epoch: u64) -> Result<AgentAssignment, CoreError> {
    let assignment = store
        .assignments_for_run(run_id)?
        .into_iter()
        .find(|assignment| assignment.epoch == epoch)
        .ok_or_else(|| CoreError::NotFound(format!("assignment for run {run_id} epoch {epoch}")))?;
    if assignment.node != *node {
        return Err(CoreError::Refused(format!(
            "run {run_id} belongs to node {}, not {node}",
            assignment.node
        )));
    }
    Ok(assignment)
}

/// Mints a run lease token and stores only its hash.
fn issue_run_lease(
    store: &Store,
    assignment: &AgentAssignment,
    now: Timestamp,
) -> Result<(String, Timestamp), CoreError> {
    let token = crate::surface::new_lease_token();
    let expires_at = now.plus(RUN_LEASE_TTL);
    store.take_lease(&Lease {
        session_id: assignment.session_id.clone(),
        surface_id: None,
        surface_kind: SurfaceKind::Agent,
        node: assignment.node.clone(),
        holder: "agent".to_owned(),
        token: token.clone(),
        acquired_at: now,
        heartbeat_at: now,
    })?;
    Ok((token, expires_at))
}

/// Releases the run lease of `session`, when one exists.
pub(crate) fn release_run_lease(store: &Store, session: &SessionId) -> Result<(), CoreError> {
    if let Some(lease) = store.lease(session)? {
        store.release_lease_hash(session, &lease.token)?;
    }
    Ok(())
}

/// Builds the `AssignRun` frame from a durable row and a fresh token.
fn assign_frame(assignment: &AgentAssignment, token: String, expires_at: Timestamp) -> AgentFrame {
    AgentFrame::AssignRun {
        assignment: assignment.payload.clone(),
        lease_token: token,
        lease_expires_at: expires_at,
    }
}

/// Opens a session on the target node if the job has none and starts a run on it.
///
/// A new session is a normal session row with an `agent`-kind holder and a relative cwd;
/// the run row and framing events come from [`begin_turn`], so there is one run log.
#[allow(clippy::too_many_arguments)]
fn open_agent_run(
    deps: &Deps,
    global: &Store,
    project_store: &Store,
    job: &Job,
    node: &NodeName,
    executor: &Executor,
    purpose: RunPurpose,
    prompt: &str,
) -> Result<(SessionId, RunId), CoreError> {
    let session_id = match crate::loops::latest_session(global, &job.id)? {
        Some(id) => {
            let row = global.require_session(&id)?;
            if row.node == *node && row.state != SessionState::Closed {
                id
            } else {
                create_agent_session(deps, global, job, node, executor)?
            }
        }
        None => create_agent_session(deps, global, job, node, executor)?,
    };
    // A previous attempt may have left a stale lease after a hard home crash.
    release_run_lease(global, &session_id)?;
    let grant = begin_turn(
        deps,
        project_store,
        &BeginTurn {
            session: session_id.clone(),
            lease: String::new(),
            executor: ExecutorRef {
                executor: executor.clone(),
                node: Some(node.clone()),
            },
            purpose,
            node: node.clone(),
            cwd: None,
            prompt: prompt.to_owned(),
        },
    )?;
    Ok((session_id, grant.run_id))
}

/// Creates one agent-node session row and its `session.created` event.
fn create_agent_session(
    deps: &Deps,
    store: &Store,
    job: &Job,
    node: &NodeName,
    executor: &Executor,
) -> Result<SessionId, CoreError> {
    let id = SessionId::new();
    let now = deps.clock.now();
    let log = SessionLog::create(&deps.paths, &id, deps.clock.clone())?;
    let role = session_role(job.role);
    let session = Session {
        id: id.clone(),
        project: Some(job.project.clone()),
        job_id: Some(job.id.clone()),
        role: Some(role),
        title: job.id.as_str().to_owned(),
        parent_session_id: None,
        node: node.clone(),
        cwd: ".".to_owned(),
        state: SessionState::Idle,
        executor: Some(executor.clone()),
        last_seq: 0,
        last_capsule_id: None,
        log_bytes: 0,
        created_at: now,
        updated_at: now,
    };
    store.upsert_session(&session)?;
    let appended = log.append(crate::vault::redact_event(
        deps,
        NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(node.clone()),
                native_session_id: None,
            },
            body: EventBody::SessionCreated(SessionCreated {
                title: session.title.clone(),
                cwd: session.cwd.clone(),
                project: session.project.clone(),
                job_id: session.job_id.clone(),
                role: session.role,
                parent_session_id: None,
            }),
            raw: None,
            ts: Some(now),
        },
    )?)?;
    store.index_event(&index_row(&appended))?;
    log.sync()?;
    Ok(id)
}

/// Closes an open run without a second run log; an already-ended run is a no-op.
fn finish_open_run(
    deps: &Deps,
    project_store: &Store,
    run_id: &RunId,
    outcome: RunOutcome,
    detail: &str,
) -> Result<(), CoreError> {
    let Some(run) = project_store.run(run_id)? else {
        return Ok(());
    };
    if run.ended_at.is_some() {
        return Ok(());
    }
    project_store.finish_run(
        run_id,
        &RunFinish {
            outcome,
            exit_code: None,
            limit_kind: None,
            cost: vowp_protocol::Cost::unknown(),
            native_session_id: None,
            detail: scrub(deps, detail),
        },
    )?;
    if let Some(session) = &run.session_id {
        project_store.set_session_state(session, SessionState::Idle, run.executor.as_ref())?;
    }
    Ok(())
}

/// Returns an opened writable project store using the injected clock.
fn project_store(deps: &Deps, store: &Store, project: &ProjectName) -> Result<Store, CoreError> {
    let paths = store
        .paths()
        .ok_or_else(|| CoreError::Invalid("offload dispatch needs a file-backed store".to_owned()))?;
    Ok(Store::open_with_project(paths, project)?.with_clock(deps.clock.clone()))
}

/// Returns a job's effective placement: per-job, then project, then fleet.
fn effective_placement(store: &Store, job: &Job, config: &VowpConfig) -> Result<Placement, CoreError> {
    let policy = config.offload.project_policy(&job.project);
    Ok(store
        .job_placement(&job.id)?
        .or_else(|| policy.placement.clone())
        .unwrap_or_else(|| config.offload.default.clone()))
}

/// Returns the project config from its checkout, then the registry, then the live config.
pub(crate) fn project_config(live: &VowpConfig, project: &vowp_protocol::Project) -> VowpConfig {
    if project.name == live.project.name {
        return live.clone();
    }
    // The project's own `vowp.toml` is authoritative, exactly as `vowp offload status`
    // reads it. A missing or invalid file falls back to the registry copy and then to the
    // selected project's live document rather than failing the whole dispatch pass.
    if let Ok(text) = std::fs::read_to_string(Path::new(&project.path).join("vowp.toml"))
        && let Ok(config) = VowpConfig::from_toml(&text)
    {
        return config;
    }
    serde_json::from_value(project.config.clone()).unwrap_or_else(|_| live.clone())
}

/// Picks the profile-pinned node when eligible, otherwise the first eligible node.
fn choose_target(pinned: Option<&NodeName>, candidates: &[NodeName]) -> Option<NodeName> {
    if let Some(pinned) = pinned
        && candidates.contains(pinned)
    {
        return Some(pinned.clone());
    }
    candidates.first().cloned()
}

/// Cheapest useful substring of a commit for a redacted detail.
fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// Builds names-only policy facts for one candidate node.
fn node_facts(
    deps: &Deps,
    store: &Store,
    card: &NodeCard,
    project: &ProjectName,
    base_commit: &str,
    remote_url: Option<&str>,
) -> Result<NodeFacts, CoreError> {
    let raw = store.require_node(&card.name)?;
    let capabilities = serde_json::from_value::<vowp_protocol::AgentCapabilities>(raw.capabilities.clone()).ok();
    let mut mapped = card.mapped.clone();
    let mut harnesses = Vec::new();
    let mut secrets = Vec::new();
    if let Some(capabilities) = &capabilities {
        harnesses = capabilities.harnesses.keys().cloned().collect();
        secrets = capabilities.secret_names.clone();
        for name in &capabilities.projects {
            if !mapped.contains(name) {
                mapped.push(name.clone());
            }
        }
    }
    let path = store
        .list_node_paths(&card.name)?
        .into_iter()
        .find(|path| &path.project == project);
    let path_facts = path.map(|map| {
        if raw.is_local {
            PathFacts {
                map_present: true,
                local_root_present: Path::new(&map.local_root).is_dir(),
                worktree_root_present: Path::new(&map.worktree_root).is_dir(),
                writable: is_writable(Path::new(&map.local_root)) && is_writable(Path::new(&map.worktree_root)),
            }
        } else {
            // The roots live on the node. The agent validates them again and refuses
            // with `path_missing` or `path_not_writable`; the home must not probe a
            // remote filesystem.
            PathFacts {
                map_present: true,
                local_root_present: true,
                worktree_root_present: true,
                writable: true,
            }
        }
    });
    let limits = store
        .node_limits(&card.name)?
        .into_iter()
        .filter(|limit| limit.resets_at.is_none_or(|resets| resets > deps.clock.now()))
        .map(|limit| LimitFact {
            provider: limit.provider,
            kind: limit.kind,
            detail: limit.detail,
        })
        .collect();
    Ok(NodeFacts {
        name: card.name.clone(),
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
        path: path_facts,
        // The home knows the project URL and the base commit. The node's local origin,
        // cleanliness and fetchability are agent-local facts; the agent validates them
        // and sends `RunRejected` when they do not hold.
        origin_matches: remote_url.map(|_| true),
        base_fetchable: (!base_commit.is_empty()).then_some(true),
        clean_base: Some(true),
        owns_conflict: Some(false),
    })
}

/// Returns whether a local path exists and is writable.
fn is_writable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.permissions().readonly())
        .unwrap_or(false)
}

/// Reads the home brake from the derived capacity snapshot, when one exists.
fn home_brake_active(deps: &Deps) -> bool {
    crate::read_snapshot(&deps.paths)
        .map(|snapshot| snapshot.capacity.brake.active)
        .unwrap_or(false)
}

/// Returns the `[harnesses.<name>].env_pass` names without their values.
fn harness_env_names(config: &VowpConfig, executor: &Executor) -> Vec<String> {
    let key = match executor.harness {
        HarnessKind::ClaudeCode => "claude-code".to_owned(),
        HarnessKind::Opencode => "opencode".to_owned(),
        HarnessKind::Acp => "acp".to_owned(),
        HarnessKind::Cli => executor.model.clone(),
        HarnessKind::Vowp => return Vec::new(),
    };
    config
        .harnesses
        .get(&key)
        .or_else(|| config.harnesses.get(&format!("cli:{key}")))
        .map(|harness| harness.env_pass.clone())
        .unwrap_or_default()
}

/// Writes the dispatcher-owned waiting note for the read model.
fn record_waiting(store: &Store, job: &Job, reason: WaitingReason, detail: &str) -> Result<(), CoreError> {
    record_waiting_note(store, &job.id, &format!("{}: {detail}", waiting_reason_name(reason)))
}

/// Writes one `waiting: offload …` note.
fn record_waiting_note(store: &Store, job: &JobId, detail: &str) -> Result<(), CoreError> {
    store.set_job_note(job, &format!("{WAITING_PREFIX}{detail}"))?;
    Ok(())
}

/// Clears the note only when it is the dispatcher's own waiting note.
pub(crate) fn clear_waiting(store: &Store, job: &JobId) -> Result<(), CoreError> {
    let current = store.require_job(job)?.note;
    if current.starts_with(WAITING_PREFIX) {
        store.set_job_note(job, "")?;
    }
    Ok(())
}

/// Returns the snake_case waiting reason used in a job note.
fn waiting_reason_name(reason: WaitingReason) -> &'static str {
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

/// Returns the snake_case agent refusal reason used in a job note.
fn reject_reason_name(reason: RunRejectReason) -> &'static str {
    match reason {
        RunRejectReason::NodeInactive => "node_inactive",
        RunRejectReason::ProjectUnmapped => "project_unmapped",
        RunRejectReason::PathMissing => "path_missing",
        RunRejectReason::PathNotWritable => "path_not_writable",
        RunRejectReason::CwdOutsideAllow => "cwd_outside_allow",
        RunRejectReason::HarnessMissing => "harness_missing",
        RunRejectReason::EnvMissing => "env_missing",
        RunRejectReason::DiskLow => "disk_low",
        RunRejectReason::WorktreeConflict => "worktree_conflict",
        RunRejectReason::OriginMismatch => "origin_mismatch",
        RunRejectReason::Other => "other",
    }
}

/// Maps a job role onto the session role.
fn session_role(role: vowp_protocol::Role) -> SessionRole {
    match role {
        vowp_protocol::Role::Planner => SessionRole::Planner,
        vowp_protocol::Role::Keeper => SessionRole::Keeper,
        vowp_protocol::Role::Verifier => SessionRole::Verifier,
        vowp_protocol::Role::Worker => SessionRole::Worker,
    }
}

/// Removes credentials from an HTTP(S) or SSH URL before it becomes durable.
///
/// The agent compares its push URL with this value. An scp-like `git@host:path` is left
/// alone: its user is a protocol name, not a secret, and stripping it would not parse.
pub fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    match authority.rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

/// Builds a path-free, home-seeded prompt for the agent.
fn seed_prompt(job: &Job) -> String {
    let branch = job.branch.as_deref().unwrap_or("job/<unknown>");
    format!(
        "You are the worker for job {job_id} of project {project}. Your working directory is the job worktree on branch {branch}.\n\
         Read AGENTS.md and your brief at {brief} first, and do exactly what the brief says. Change only the paths in `owns`.\n\
         When finished, commit on this branch (do not push), run the gates listed in the brief, and write your report to {report}\n\
         with the front-matter required by the brief. Do not stop before the report exists. If you need the owner, write the question under\n\
         questions_for_owner and set status: blocked.",
        job_id = job.id,
        project = job.project,
        brief = job.brief,
        report = job.report,
    )
}

/// Truncates agent-supplied detail and removes the home root before it becomes durable.
fn scrub(deps: &Deps, text: &str) -> String {
    let root = deps.paths.root().display().to_string();
    let text = if root.is_empty() {
        text.to_owned()
    } else {
        text.replace(&root, "<home>")
    };
    vowp_adapters::classify::redact(&text, 500)
}

/// Handles one resume surface entry without requiring a live surface lease.
fn resume_surface(store: &Store, node: &NodeName, resume: &SurfaceResume) -> Result<SurfaceResumeResult, CoreError> {
    let Some(surface) = store.surface_by_id(&resume.surface)? else {
        return Ok(SurfaceResumeResult {
            surface: resume.surface.clone(),
            accepted: false,
            reason: Some("the surface is not registered".to_owned()),
            tail_seq: resume.last_acked,
        });
    };
    if surface.node != *node {
        return Ok(SurfaceResumeResult {
            surface: resume.surface.clone(),
            accepted: false,
            reason: Some("the surface belongs to another node".to_owned()),
            tail_seq: resume.last_acked,
        });
    }
    if surface.state != SurfaceState::Attached {
        return Ok(SurfaceResumeResult {
            surface: resume.surface.clone(),
            accepted: false,
            reason: Some("the surface is not attached".to_owned()),
            tail_seq: resume.last_acked,
        });
    }
    let tail_seq = surface
        .metadata
        .get("session")
        .and_then(serde_json::Value::as_str)
        .and_then(|session| SessionId::parse(session).ok())
        .and_then(|session| store.session(&session).ok().flatten())
        .map(|session| session.last_seq)
        .unwrap_or(resume.last_acked);
    Ok(SurfaceResumeResult {
        surface: resume.surface.clone(),
        accepted: true,
        reason: None,
        tail_seq,
    })
}
