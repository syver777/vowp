//! Shared helpers for the failover/planner/inbox tests.
//!
//! Everything reuses the hermetic `loops_support` node: temp directories, a fake clock,
//! scripted adapters and (when needed) a real git repository under the temp dir.

#![allow(dead_code)]

#[path = "loops_support.rs"]
pub mod h;

pub use h::*;

use std::sync::Arc;
use std::time::{Duration, Instant};

use vowp_core::deps::AdapterRegistry;
use vowp_protocol::{
    Cost, Executor, Job, JobId, LedgerKind, LimitEvent, LimitKind, NodeName, Profile, Run, RunId, RunOutcome,
    RunPurpose, RunRole, RunSummary, Session, SessionId, SessionRole, StopReason, Switch, SwitchReason, SwitchState,
    Timestamp, VowpConfig,
};
use vowp_store::{LedgerFilter, Store};

/// A valid capsule draft that the capsule writer accepts.
pub fn draft_json(intent: &str) -> String {
    serde_json::json!({
        "intent": intent,
        "constraints": [],
        "decisions": [],
        "state": {"done": ["work"], "in_progress": [], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

/// A scripted action that answers the handoff/capsule writer with a valid draft.
pub fn capsule_action(intent: &str) -> ScriptAction {
    ScriptAction::ok(&draft_json(intent))
}

/// Returns true for the capsule writer prompt.
pub fn is_capsule_prompt(prompt: &str) -> bool {
    prompt.contains("handoff writer")
}

/// Returns true for the planner prompt.
pub fn is_planner_prompt(prompt: &str) -> bool {
    !is_capsule_prompt(prompt) && prompt.contains("You are the planner")
}

/// Returns true for the keeper prompt.
pub fn is_keeper_prompt(prompt: &str) -> bool {
    !is_capsule_prompt(prompt) && prompt.contains("You are the keeper")
}

/// Returns true for the worker start prompt.
pub fn is_worker_prompt(prompt: &str) -> bool {
    !is_capsule_prompt(prompt) && prompt.contains("You are the worker")
}

/// Returns true for the L6 probe prompt.
pub fn is_probe_prompt(prompt: &str) -> bool {
    prompt == vowp_core::loops::prompts::PROBE_PROMPT
}

/// Builds one classified limit summary.
pub fn limit_summary(kind: LimitKind, provider: &str, resets_at: Option<Timestamp>) -> RunSummary {
    let outcome = match kind {
        LimitKind::Quota => RunOutcome::Limit,
        LimitKind::Credits => RunOutcome::Credits,
        LimitKind::Auth => RunOutcome::Auth,
        LimitKind::Rate => RunOutcome::Rate,
        LimitKind::Context => RunOutcome::Context,
    };
    RunSummary {
        exit_code: Some(1),
        outcome,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: Some(LimitEvent {
            kind,
            provider: provider.to_owned(),
            resets_at,
            message: format!("{kind:?} from {provider}"),
            http_status: None,
        }),
        result_text: None,
        stop_reason: StopReason::Limit,
    }
}

/// Builds an unclassified crash/error summary.
pub fn failure_summary(outcome: RunOutcome) -> RunSummary {
    RunSummary {
        exit_code: Some(1),
        outcome,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("unknown failure".to_owned()),
        stop_reason: StopReason::Error,
    }
}

/// Replaces the planner/keeper ladders in a test configuration.
pub fn set_ladders(config: &mut VowpConfig, planner: &[&str], keeper: &[&str]) {
    let ladder = |specs: &[&str]| {
        specs
            .iter()
            .map(|spec| Executor::parse(spec).expect("executor"))
            .collect::<Vec<_>>()
    };
    config.profiles.insert(
        "planner".to_owned(),
        Profile {
            ladder: ladder(planner),
            on_quota: vowp_protocol::OnQuota::NextThenProbeBack,
            node: None,
        },
    );
    config.profiles.insert(
        "keeper".to_owned(),
        Profile {
            ladder: ladder(keeper),
            on_quota: vowp_protocol::OnQuota::NextThenProbeBack,
            node: None,
        },
    );
}

/// Creates a node whose registry has independent opencode and claude-code adapters.
pub fn two_adapter_node<F>(
    real_git: bool,
    opencode: Arc<ScriptAdapter>,
    claude: Arc<ScriptAdapter>,
    configure: F,
) -> TestNode
where
    F: FnOnce(&mut VowpConfig),
{
    let mut node = TestNode::with_adapter_config(real_git, opencode.clone(), configure);
    node.deps.adapters = Arc::new(
        AdapterRegistry::new()
            .with("opencode", opencode)
            .with("claude-code", claude),
    );
    node
}

/// Builds a `Run` row value for L6 unit tests.
pub fn run_for(
    node: &TestNode,
    job: Option<&Job>,
    provider: &str,
    role: RunRole,
    outcome: RunOutcome,
    limit_kind: Option<LimitKind>,
) -> Run {
    let executor = Executor::parse(match provider {
        "claude" => "claude-code:claude-sonnet-5-5@medium",
        "deepseek" => "opencode:deepseek/deepseek-flash#max",
        _ => "opencode:deepseek/deepseek-flash#max",
    })
    .expect("executor");
    Run {
        id: RunId::new(),
        job_id: job.map(|job| job.id.clone()),
        session_id: None,
        role,
        purpose: match role {
            RunRole::Planner => RunPurpose::Plan,
            RunRole::Keeper => RunPurpose::Keep,
            RunRole::Probe => RunPurpose::Probe,
            _ => RunPurpose::Start,
        },
        profile: Some(
            match role {
                RunRole::Planner => "planner",
                RunRole::Keeper => "keeper",
                RunRole::Verifier => "verifier",
                _ => "worker",
            }
            .to_owned(),
        ),
        rung: 0,
        executor: Some(executor),
        provider: Some(provider.to_owned()),
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: ts(),
        ended_at: Some(ts()),
        exit_code: Some(1),
        outcome: Some(outcome),
        limit_kind,
        cost: Cost::unknown(),
        detail: "test run".to_owned(),
    }
}

/// Polls `predicate` until it is true or `timeout` elapses.
pub fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Returns the newest planner session of the node's project.
pub fn planner_session(node: &TestNode) -> Option<Session> {
    role_session(node, SessionRole::Planner)
}

/// Returns the newest session of `role`.
pub fn role_session(node: &TestNode, role: SessionRole) -> Option<Session> {
    node.store()
        .list_sessions(&vowp_store::SessionFilter {
            project: Some(node.project.clone()),
            ..vowp_store::SessionFilter::default()
        })
        .ok()?
        .into_iter()
        .find(|session| session.role == Some(role))
}

/// Returns every switch row of `session`.
pub fn switches(store: &Store, session: &SessionId) -> Vec<Switch> {
    store.list_switches(session).unwrap_or_default()
}

/// Returns the switch reasons recorded for a role's newest session.
pub fn switch_reasons(node: &TestNode, role: SessionRole) -> Vec<SwitchReason> {
    match role_session(node, role) {
        Some(session) => switches(&node.store(), &session.id)
            .into_iter()
            .map(|switch| switch.reason)
            .collect(),
        None => Vec::new(),
    }
}

/// Returns the completed switch reasons for a role's newest session.
pub fn completed_switch_reasons(node: &TestNode, role: SessionRole) -> Vec<SwitchReason> {
    match role_session(node, role) {
        Some(session) => switches(&node.store(), &session.id)
            .into_iter()
            .filter(|switch| switch.state == SwitchState::Completed)
            .map(|switch| switch.reason)
            .collect(),
        None => Vec::new(),
    }
}

/// Returns whether the switch ledger contains an entry for `reason` on the planner session.
///
/// L6 finishes the session switch row before it appends the matching `switch` ledger row
/// (the two writes use different store handles), so a test that only waits for the
/// completed switch can observe it while the ledger row is still missing. Wait on this
/// durable condition before asserting the ledger contents. The ledger spans every session,
/// so match `session_id` as well as the reason.
pub fn switch_ledger_reason_recorded(node: &TestNode, reason: &str) -> bool {
    let Some(session) = planner_session(node) else {
        return false;
    };
    node.store()
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Switch),
            ..LedgerFilter::default()
        })
        .map(|entries| {
            entries.iter().any(|entry| {
                entry.session_id.as_ref() == Some(&session.id)
                    && entry.detail.get("reason").and_then(|value| value.as_str()) == Some(reason)
            })
        })
        .unwrap_or(false)
}

/// Returns true when L6 has recorded its policy for every job's newest run and no
/// `park:<provider>` request is still queued.
///
/// The runner thread ends the run row inside `run_turn`, then applies L6 (which writes
/// the limit and `park:<provider>` request) and only then writes the `l6:<run>` marker.
/// A test that answers a provider notice in that gap can have the answer resolve the jobs
/// at tick step 0 and the same tick's harvest step drain the stale park request and park
/// them again. Wait for the durable marker and an empty park queue before answering.
pub fn l6_policy_settled(node: &TestNode, jobs: &[JobId]) -> bool {
    let store = node.store();
    let parks_clear = store
        .kv_prefix("park:")
        .map(|entries| entries.is_empty())
        .unwrap_or(false);
    if !parks_clear {
        return false;
    }
    jobs.iter().all(|job| match store.last_run_of_job(job) {
        Ok(Some(run)) => run.ended_at.is_some() && store.kv_get(&format!("l6:{}", run.id)).ok().flatten().is_some(),
        // A job parked before its first run has no L6 work to wait for.
        Ok(None) => true,
        Err(_) => false,
    })
}

/// Counts jobs whose id starts with `prefix`.
pub fn jobs_with_prefix(node: &TestNode, prefix: &str) -> usize {
    node.store()
        .list_jobs(&vowp_store::JobFilter {
            project: Some(node.project.clone()),
            id_prefix: Some(prefix.to_owned()),
            ..vowp_store::JobFilter::default()
        })
        .map(|jobs| jobs.len())
        .unwrap_or(0)
}

/// Breaks a `job/<id>` branch name out of a worker prompt.
pub fn prompt_job(prompt: &str) -> Option<JobId> {
    let start = prompt.find("for job ")? + "for job ".len();
    let rest = &prompt[start..];
    let end = rest.find(" of project")?;
    JobId::parse(&rest[..end]).ok()
}

/// Returns the parent id from a fix prompt.
pub fn fix_parent(prompt: &str) -> Option<String> {
    let rest = prompt.strip_prefix("Job ")?;
    let end = rest.find(" failed")?;
    Some(rest[..end].to_owned())
}

/// Returns a node name for tests.
pub fn local_node() -> NodeName {
    NodeName::parse("local").expect("node")
}
