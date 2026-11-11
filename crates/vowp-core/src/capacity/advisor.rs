//! Capacity advice: one measured proposal per turn, never a target change.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.4. The advisor runs on the node's
//! thirty-minute timer and on capacity events. It reads the same [`CapacityReport`] as
//! `vowp capacity report --json`, writes one `capacity_advice` row and one node-inbox
//! `decision` item with `apply`/`keep` options, and leaves `capacity_targets` untouched.
//! Applying a proposal is the owner's `vowp capacity set --target N`; the AI never changes
//! the target itself.
//!
//! The corresponding framework skill is `framework/skills/vowp-capacity-advisor/SKILL.md`.

use std::collections::BTreeSet;
use std::time::Duration;

use vowp_protocol::{
    AdviceState, CapacityAdvice, CapacityReport, InboxId, InboxItem, InboxKind, InboxState, JobState, ProjectState,
    SampleSource, TargetMode, Timestamp,
};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

use super::admit::CapacityManager;
use super::targets::{MAX_TARGET, MIN_TARGET};

/// The shortest gap between two timer-driven advice turns.
pub const ADVICE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// How far back an OOM kill still counts as an advice event before the first advice.
const OOM_LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);

/// Key holding the timestamp of the last advice row this node wrote.
const LAST_ADVICE_KEY: &str = "capacity:advice_at";

/// Key holding the project names seen by the last advice turn.
const PROJECTS_KEY: &str = "capacity:advice_projects";

/// Key holding whether the brake was active at the last advice turn.
const BRAKE_KEY: &str = "capacity:advice_brake";

/// Key holding the instant the node became idle.
const IDLE_KEY: &str = "capacity:advice_idle_since";

/// Why the advisor is considering a new target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceTrigger {
    /// The thirty-minute timer elapsed.
    Timer,
    /// A project was registered.
    NewProject,
    /// A brake tripped.
    Brake,
    /// A run was OOM-killed.
    Oom,
    /// The node stayed idle for long enough.
    Idle,
}

/// Produces one advice row when the trigger calls for it.
///
/// Event triggers are the caller's evidence that the event happened; this function only
/// trusts them. The timer trigger is the one rule the advisor enforces itself: two
/// timer-driven turns are at least [`ADVICE_INTERVAL`] apart. An event turn also moves that
/// gap, so a burst of events cannot be followed by a redundant timer turn a minute later.
///
/// The call writes an advice row, a linked node-inbox `decision` item and a `kv` timestamp.
/// It never writes a `capacity_targets` row and never calls the target setter.
pub fn maybe_advise(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    trigger: AdviceTrigger,
) -> Result<Option<CapacityAdvice>, CoreError> {
    maybe_advise_with_report(deps, store, now, trigger, None)
}

/// Runs one node-level advice check, choosing the timer or one of the four events.
///
/// This is the supervisor's entry point: one call per node tick. The timer itself is
/// rate-limited by [`maybe_advise`]; the event comparisons are kept here so the supervisor
/// only needs the one call.
pub fn advise_tick(deps: &Deps, store: &Store, now: Timestamp) -> Result<Option<CapacityAdvice>, CoreError> {
    let report = CapacityManager::new(deps.clone()).report(store, now)?;
    let trigger = choose_trigger(store, now, &report)?;
    let advice = maybe_advise_with_report(deps, store, now, trigger, Some(report.clone()))?;
    if advice.is_some() {
        update_markers(store, now, trigger, &report)?;
    }
    Ok(advice)
}

/// Writes one proposal unless the trigger is not due or the owner is already applying one.
fn maybe_advise_with_report(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    trigger: AdviceTrigger,
    report: Option<CapacityReport>,
) -> Result<Option<CapacityAdvice>, CoreError> {
    reconcile_dismissed_advice(store, now)?;
    if !is_due(store, now, trigger)? {
        return Ok(None);
    }
    if pending_apply(store, now)? {
        // The owner answered `apply`; wait for `vowp capacity set` instead of replacing
        // the proposal with a new one before the target changes.
        return Ok(None);
    }

    let report = match report {
        Some(report) => report,
        None => CapacityManager::new(deps.clone()).report(store, now)?,
    };
    if trigger == AdviceTrigger::Idle && report.running.runs > 0 {
        // The caller reported idle, but a run is open now. A target proposal would be
        // based on the wrong picture; wait for the next turn.
        return Ok(None);
    }

    let advised = report.target.advised_workers.unwrap_or(report.target.target_workers);
    let measured = report
        .host
        .as_ref()
        .is_some_and(|host| host.source != SampleSource::Fallback && host.mem_total_bytes > 0);
    let proposed = if measured {
        advised.clamp(MIN_TARGET, MAX_TARGET)
    } else {
        // Without a measured host sample the conservative auto target is one worker.
        // Proposing that would silently shrink an owner's fixed target on macOS, so the
        // fallback advice is to keep what the owner already chose.
        report.target.target_workers.clamp(MIN_TARGET, MAX_TARGET)
    };

    close_superseded_advice(store, now)?;
    let reasons = build_reasons(&report, trigger, measured, advised);
    let expected_effect = expected_effect(&report, proposed, measured);
    let inbox_id = InboxId::new();
    let advice = CapacityAdvice {
        id: ulid::Ulid::generate().to_string(),
        created_at: now,
        current_target: report.target.target_workers,
        proposed_target: proposed,
        mode: report.target.mode,
        reasons,
        expected_effect,
        inbox_id: Some(inbox_id.clone()),
        state: AdviceState::Open,
        applied_at: None,
        applied_by: None,
    };
    let inbox = InboxItem {
        id: inbox_id,
        project: None,
        job_id: None,
        kind: InboxKind::Decision,
        title: format!("Capacity: {proposed} workers (advised {advised})"),
        body: inbox_body(&advice, &report, trigger),
        options: vec!["apply".to_owned(), "keep".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now,
        answered_at: None,
    };

    store.transaction(|store| {
        store.insert_capacity_advice(&advice)?;
        store.insert_node_inbox(&inbox)?;
        store.kv_set(LAST_ADVICE_KEY, &now.as_rfc3339())?;
        Ok(())
    })?;
    Ok(Some(advice))
}

/// Chooses the event or timer trigger for one node tick.
///
/// Precedence is OOM, brake, new project, sustained idle, timer. A trigger that is not
/// chosen keeps its marker so the next tick sees it again.
fn choose_trigger(store: &Store, now: Timestamp, report: &CapacityReport) -> Result<AdviceTrigger, CoreError> {
    let since = last_advice_at(store)?.unwrap_or_else(|| now.minus(OOM_LOOKBACK));
    let oom = store
        .run_usage_for_group(
            &vowp_store::repo_capacity::UsageQuery {
                since: Some(since),
                ..vowp_store::repo_capacity::UsageQuery::default()
            },
            u32::MAX,
        )?
        .iter()
        .any(|row| row.oom_killed);
    if oom {
        return Ok(AdviceTrigger::Oom);
    }

    let brake_was = store.kv_get(BRAKE_KEY)?.as_deref() == Some("1");
    if report.brakes.active && !brake_was {
        return Ok(AdviceTrigger::Brake);
    }
    if !report.brakes.active && brake_was {
        // Record the clear even when no advice turn is due, so the next trip is an event.
        store.kv_set(BRAKE_KEY, "0")?;
    }

    let current = project_names(store)?;
    match read_project_marker(store)? {
        Some(previous) => {
            if current.iter().any(|name| !previous.contains(name)) {
                return Ok(AdviceTrigger::NewProject);
            }
        }
        // The first turn adopts the existing projects without calling them new.
        None => store.kv_set(PROJECTS_KEY, &project_marker_json(&current)?)?,
    }

    if report.running.runs == 0 && report.waiting.is_empty() && !has_pending_work(store)? {
        match read_idle_since(store)? {
            Some(since) if now.since(since) >= ADVICE_INTERVAL => return Ok(AdviceTrigger::Idle),
            Some(_) => {}
            None => store.kv_set(IDLE_KEY, &now.as_rfc3339())?,
        }
    } else {
        let _ = store.kv_delete(IDLE_KEY);
    }
    Ok(AdviceTrigger::Timer)
}

/// Updates the event markers after a turn that wrote advice.
fn update_markers(
    store: &Store,
    now: Timestamp,
    trigger: AdviceTrigger,
    report: &CapacityReport,
) -> Result<(), CoreError> {
    if !report.brakes.active || trigger == AdviceTrigger::Brake {
        store.kv_set(BRAKE_KEY, if report.brakes.active { "1" } else { "0" })?;
    }
    if trigger == AdviceTrigger::Idle {
        store.kv_set(IDLE_KEY, &now.as_rfc3339())?;
    }
    let current = project_names(store)?;
    let pending_new =
        read_project_marker(store)?.is_some_and(|previous| current.iter().any(|name| !previous.contains(name)));
    if trigger == AdviceTrigger::NewProject || !pending_new {
        store.kv_set(PROJECTS_KEY, &project_marker_json(&current)?)?;
    }
    Ok(())
}

/// Returns whether the owner answered `apply` recently and the target has not changed yet.
fn pending_apply(store: &Store, now: Timestamp) -> Result<bool, CoreError> {
    let Some(advice) = store.latest_open_advice()? else {
        return Ok(false);
    };
    let Some(inbox_id) = advice.inbox_id.as_ref() else {
        return Ok(false);
    };
    let Some(item) = store.inbox_item(inbox_id)? else {
        return Ok(false);
    };
    if item.state != InboxState::Answered {
        return Ok(false);
    }
    let applies = item
        .answer
        .as_deref()
        .is_some_and(|answer| answer.trim().eq_ignore_ascii_case("apply"));
    if !applies {
        return Ok(false);
    }
    let since = item.answered_at.unwrap_or(item.created_at);
    Ok(now.since(since) < ADVICE_INTERVAL)
}

/// Closes every older proposal so a node has at most one open advice row.
///
/// The previous inbox item is dismissed too, so the owner sees one `Capacity:` decision
/// instead of a growing stack. An advice whose item the owner answered `apply` is kept by
/// [`pending_apply`] and is not closed here.
fn close_superseded_advice(store: &Store, now: Timestamp) -> Result<(), CoreError> {
    while let Some(advice) = store.latest_open_advice()? {
        if let Some(inbox_id) = advice.inbox_id.as_ref()
            && let Some(item) = store.inbox_item(inbox_id)?
            && item.state == InboxState::Open
        {
            // A concurrent answer can make this a conflict; the advice still closes.
            let _ = store.dismiss_inbox(inbox_id, "supervisor");
        }
        store.finish_capacity_advice(&advice.id, AdviceState::Dismissed, "supervisor", now)?;
    }
    Ok(())
}

/// Returns whether any project has a pending or ready job.
fn has_pending_work(store: &Store) -> Result<bool, CoreError> {
    let filter = vowp_store::JobFilter {
        states: vec![JobState::Pending, JobState::Ready],
        ..vowp_store::JobFilter::default()
    };
    if let Some(paths) = store.paths() {
        for entry in store.list_project_entries()? {
            if entry.state == ProjectState::Archived || entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
                continue;
            }
            match Store::open_read_only(paths, Some(&entry.name)) {
                Ok(project_store) => {
                    if !project_store.list_jobs(&filter)?.is_empty() {
                        return Ok(true);
                    }
                }
                // An unreadable project might have work; never call the node idle then.
                Err(_) => return Ok(true),
            }
        }
        return Ok(false);
    }
    if store.bound_project().is_some() {
        return Ok(!store.list_jobs(&filter)?.is_empty());
    }
    Ok(false)
}

/// Returns the names of every non-archived, non-reserved project.
fn project_names(store: &Store) -> Result<BTreeSet<String>, CoreError> {
    let mut names = BTreeSet::new();
    for entry in store.list_project_entries()? {
        if entry.state == ProjectState::Archived || entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        names.insert(entry.name.as_str().to_owned());
    }
    Ok(names)
}

/// Reads the project-name marker, if the first advice turn has already run.
fn read_project_marker(store: &Store) -> Result<Option<BTreeSet<String>>, CoreError> {
    let Some(text) = store.kv_get(PROJECTS_KEY)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&text).unwrap_or_default()))
}

/// Serializes the project-name marker.
fn project_marker_json(names: &BTreeSet<String>) -> Result<String, CoreError> {
    serde_json::to_string(names).map_err(|error| CoreError::Invalid(format!("capacity project marker: {error}")))
}

/// Reads the instant the node became idle.
fn read_idle_since(store: &Store) -> Result<Option<Timestamp>, CoreError> {
    Ok(store.kv_get(IDLE_KEY)?.and_then(|text| Timestamp::parse(&text).ok()))
}

/// Returns the timestamp of the newest advice row, open or closed.
///
/// The `kv` marker is the durable record; the open row is a second source after a direct
/// store write in a test or in a follow-up tool.
fn last_advice_at(store: &Store) -> Result<Option<Timestamp>, CoreError> {
    let mut last = store
        .kv_get(LAST_ADVICE_KEY)?
        .and_then(|text| Timestamp::parse(&text).ok());
    if let Some(open) = store.latest_open_advice()? {
        last = Some(last.map_or(open.created_at, |last| last.max(open.created_at)));
    }
    Ok(last)
}

/// Marks the newest open advice dismissed when its inbox item was kept or removed.
///
/// `inbox answer <id> apply` intentionally leaves the advice open: the owner still has to
/// run `vowp capacity set`, which is the only writer of the target. Any other resolution
/// (dismissal, `keep`, or a missing item) closes the proposal.
fn reconcile_dismissed_advice(store: &Store, now: Timestamp) -> Result<(), CoreError> {
    let Some(advice) = store.latest_open_advice()? else {
        return Ok(());
    };
    let Some(inbox_id) = advice.inbox_id.as_ref() else {
        return Ok(());
    };
    let Some(item) = store.inbox_item(inbox_id)? else {
        // §3.6: a missing item only means the owner dismissed it.
        close_advice(store, &advice.id, now)?;
        return Ok(());
    };
    let kept = match item.state {
        InboxState::Open => false,
        InboxState::Dismissed => true,
        InboxState::Answered => !item
            .answer
            .as_deref()
            .is_some_and(|answer| answer.trim().eq_ignore_ascii_case("apply")),
    };
    if kept {
        close_advice(store, &advice.id, now)?;
    }
    Ok(())
}

/// Dismisses one proposal and starts a fresh thirty-minute gap.
///
/// Without the gap reset, an owner who answers `keep` just before a timer tick would get
/// the same proposal again one minute later. Events still bypass the gap by design.
fn close_advice(store: &Store, id: &str, now: Timestamp) -> Result<(), CoreError> {
    store.finish_capacity_advice(id, AdviceState::Dismissed, "owner", now)?;
    store.kv_set(LAST_ADVICE_KEY, &now.as_rfc3339())?;
    Ok(())
}

/// Returns whether this trigger may write an advice row now.
fn is_due(store: &Store, now: Timestamp, trigger: AdviceTrigger) -> Result<bool, CoreError> {
    if trigger != AdviceTrigger::Timer {
        return Ok(true);
    }
    Ok(last_advice_at(store)?.is_none_or(|last| now.since(last) >= ADVICE_INTERVAL))
}

/// Builds the human-readable reasons stored on the advice row.
fn build_reasons(report: &CapacityReport, trigger: AdviceTrigger, measured: bool, advised: u32) -> Vec<String> {
    let mut reasons = vec![trigger_reason(trigger).to_owned()];
    reasons.push(format!(
        "current target: {} worker(s) ({}), measured advice {advised}",
        report.target.target_workers,
        mode_name(report.target.mode)
    ));
    if report.target.is_override {
        reasons.push(format!(
            "owner override: {} worker(s) above the advised {advised}",
            report.target.target_workers
        ));
    }
    match &report.host {
        Some(host) if !measured => reasons.push(format!(
            "host: fallback sample at {}; no measured limit, so the current target is kept",
            host.at.as_rfc3339()
        )),
        Some(host) => reasons.push(format!(
            "host: {} available of {}, reserve {}, {} CPU core(s), {} free disk",
            bytes(host.mem_available_bytes),
            bytes(host.mem_total_bytes),
            bytes(report.reserve_ram_bytes),
            host.cpu_count,
            bytes(host.disk_free_home_bytes)
        )),
        None => reasons.push("host: no sample yet; the conservative target stands".to_owned()),
    }
    reasons.push(format!(
        "running: {} run(s), p90 {} RAM and {:.2} CPU core(s)",
        report.running.runs,
        bytes(report.running.ram_p90_bytes),
        report.running.cpu_p90_cores
    ));
    if report.estimates.is_empty() {
        reasons.push("estimates: none yet; the compiled default is used".to_owned());
    } else {
        let samples: u64 = report
            .estimates
            .iter()
            .map(|estimate| u64::from(estimate.samples))
            .sum();
        let ram = report
            .estimates
            .iter()
            .map(|estimate| estimate.ram_p90_bytes)
            .max()
            .unwrap_or(0);
        let cpu = report
            .estimates
            .iter()
            .map(|estimate| estimate.cpu_p90_cores)
            .fold(0.0_f64, f64::max);
        reasons.push(format!(
            "estimates: {} group(s), {samples} sample(s), largest p90 {} RAM and {cpu:.2} CPU core(s)",
            report.estimates.len(),
            bytes(ram)
        ));
    }
    match report.waiting.first() {
        Some(first) => reasons.push(format!(
            "waiting: {} job(s); first {} in {}: {}",
            report.waiting.len(),
            first.job,
            first.project,
            first.reason.detail
        )),
        None => reasons.push("waiting: no job is waiting for capacity".to_owned()),
    }
    if report.brakes.active {
        let detail = report
            .brakes
            .reasons
            .iter()
            .map(|reason| reason.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        reasons.push(format!("brake: active ({detail}); a target change does not lift it"));
    } else if trigger == AdviceTrigger::Brake {
        reasons.push("brake: off; the reported brake cleared before this turn".to_owned());
    } else {
        reasons.push("brake: off".to_owned());
    }
    if !report.limits.is_empty() {
        let providers = report
            .limits
            .iter()
            .map(|limit| limit.provider.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        reasons.push(format!("provider limits: {providers}"));
    }
    reasons
}

/// Returns one sentence describing what applying the proposal changes.
fn expected_effect(report: &CapacityReport, proposed: u32, measured: bool) -> String {
    if !measured {
        return format!(
            "Keep the target at {proposed} worker(s) because this host has no measured sample; no RAM or CPU fit is claimed. Every run still passes the brake, provider and per-run checks."
        );
    }
    let current = report.target.target_workers;
    let mut effect = if proposed > current {
        format!(
            "Admit up to {proposed} concurrent run(s), {} more than now; the node keeps its RAM reserve and CPU headroom.",
            proposed - current
        )
    } else if proposed < current {
        format!(
            "Admit up to {proposed} concurrent run(s), {} fewer than now, leaving more RAM and CPU headroom; running work is not stopped.",
            current - proposed
        )
    } else {
        format!("Keep the target at {proposed} worker(s); the measurements and reserves fit it.")
    };
    effect.push_str(" Every run still passes the brake, provider and per-run checks.");
    if report.target.is_override {
        effect.push_str(" Applying it replaces the current owner override.");
    }
    if report.brakes.active {
        effect.push_str(" The brake is active; a target change does not lift it and running work is never stopped.");
    }
    effect
}

/// Builds the node-inbox body shown to the owner.
fn inbox_body(advice: &CapacityAdvice, report: &CapacityReport, trigger: AdviceTrigger) -> String {
    let mut body = format!(
        "Trigger: {}.\nCurrent target: {} worker(s) ({}).\nProposed target: {} worker(s).\n\nReasons:\n",
        trigger_name(trigger),
        advice.current_target,
        mode_name(advice.mode),
        advice.proposed_target
    );
    for reason in &advice.reasons {
        body.push_str(&format!("- {reason}\n"));
    }
    body.push_str(&format!("\nExpected effect: {}\n", advice.expected_effect));
    if report
        .host
        .as_ref()
        .is_some_and(|host| host.source == SampleSource::Fallback || host.mem_total_bytes == 0)
    {
        body.push_str(
            "\nThis host has no measured sample; the proposal keeps the current target rather than guessing.\n",
        );
    }
    body.push_str(&format!(
        "\nApply only after the owner says yes:\n  vowp capacity set --target {}\nKeep the current target: answer \"keep\" or dismiss this item; nothing changes.\n",
        advice.proposed_target
    ));
    body
}

/// Returns a stable short trigger name for the inbox body.
fn trigger_name(trigger: AdviceTrigger) -> &'static str {
    match trigger {
        AdviceTrigger::Timer => "timer",
        AdviceTrigger::NewProject => "new project",
        AdviceTrigger::Brake => "brake",
        AdviceTrigger::Oom => "OOM",
        AdviceTrigger::Idle => "idle",
    }
}

/// Returns the one-line reason for the trigger.
fn trigger_reason(trigger: AdviceTrigger) -> &'static str {
    match trigger {
        AdviceTrigger::Timer => "scheduled review: the thirty-minute interval elapsed",
        AdviceTrigger::NewProject => "event: a project was registered",
        AdviceTrigger::Brake => "event: a capacity brake was reported",
        AdviceTrigger::Oom => "event: a run was OOM-killed",
        AdviceTrigger::Idle => "event: the node stayed idle",
    }
}

/// Returns the lowercase target-mode name.
fn mode_name(mode: TargetMode) -> &'static str {
    match mode {
        TargetMode::Auto => "auto",
        TargetMode::Fixed => "fixed",
    }
}

/// Formats bytes as GiB with one decimal, or MiB for small values.
fn bytes(value: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if value as f64 >= GIB {
        format!("{:.1} GiB", value as f64 / GIB)
    } else {
        format!("{:.0} MiB", value as f64 / MIB)
    }
}
