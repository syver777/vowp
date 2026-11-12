//! L9 schedule engine: config sync, missed-run catch-up, due ticks and the three actions.
//! Spec: `docs/design/crates/vowp-core.md` §6 and `docs/design/loops.md` §11.
//!
//! `tick` advances `last_run_at`/`next_run_at` **before** returning a due run, so a crash can
//! lose at most one firing and can never double-fire. `fire` writes the ledger row carrying
//! the idempotency key `"<schedule id>@<due>"` before it executes the action.
//!
//! Interval schedules re-arm on the row's `created_at` phase, so a restart never snaps an
//! occurrence to the epoch-aligned grid; cron schedules ignore the anchor.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use vowp_protocol::{
    ActiveState, InboxItem, InboxKind, InboxState, JobId, LedgerEntry, LedgerKind, MissedPolicy, Schedule,
    ScheduleAction, ScheduleId, ScheduleSpecKind, Timestamp, VowpConfig,
};
use vowp_store::{LedgerFilter, Store};

use crate::{CommandSpec, CoreError, Deps, Request};

use super::spec::ScheduleSpec;

/// A schedule occurrence that is due now.
#[derive(Debug, Clone, PartialEq)]
pub struct DueRun {
    /// The schedule row at the time it became due.
    pub schedule: Schedule,
    /// The occurrence time (the schedule's old `next_run_at`).
    pub due_at: Timestamp,
}

/// What one firing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// A job template created a job.
    JobCreated(JobId),
    /// A loop was marked due.
    LoopTriggered(String),
    /// A command ran (successfully or not).
    CommandRan {
        /// Process exit code, or `None` when a signal or timeout ended it.
        exit_code: Option<i32>,
    },
    /// Nothing ran because the occurrence was already recorded.
    Skipped(String),
}

/// The L9 schedule engine.
#[derive(Clone)]
pub struct ScheduleEngine {
    deps: Deps,
}

impl ScheduleEngine {
    /// Creates an engine over `deps`.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Creates or refreshes the `[[schedules]]` rows from `cfg`, keyed by name.
    ///
    /// Existing rows keep their id, state and run history; a changed spec, timezone, action or
    /// missed policy replaces the row in one transaction. New rows get a fresh id and their
    /// first `next_run_at`. Interval phase is anchored at `created_at` (or `now` for a new row);
    /// an existing non-null `next_run_at` is never recomputed.
    pub fn sync_from_config(&self, store: &Store, cfg: &VowpConfig) -> Result<(), CoreError> {
        let now = store.clock().now();
        for entry in &cfg.schedules {
            let (kind, spec, tz) = match (&entry.cron, &entry.every) {
                (Some(cron), None) => (
                    ScheduleSpecKind::Cron,
                    cron.clone(),
                    entry.tz.clone().unwrap_or_else(|| "UTC".to_owned()),
                ),
                (None, Some(every)) => (
                    ScheduleSpecKind::Interval,
                    every.clone(),
                    entry.tz.clone().unwrap_or_else(|| "UTC".to_owned()),
                ),
                _ => {
                    return Err(CoreError::Invalid(format!(
                        "schedule {}: exactly one of cron and every is required",
                        entry.name
                    )));
                }
            };
            let existing = store.schedule(&entry.name)?;
            let anchor = existing.as_ref().map_or(now, |schedule| schedule.created_at);
            let parsed = ScheduleSpec::parse_at(kind, &spec, &tz, anchor)?;
            let missed = entry.missed.unwrap_or(MissedPolicy::RunOnce);
            let next_run_at = parsed.next_after(now);

            match existing {
                Some(existing)
                    if existing.spec_kind == kind
                        && existing.spec == spec
                        && existing.tz == tz
                        && existing.action == entry.action
                        && existing.missed_policy == missed =>
                {
                    if existing.next_run_at.is_none() {
                        store.record_schedule_run(
                            &existing.id,
                            existing.last_run_at.unwrap_or(existing.created_at),
                            next_run_at,
                            &existing.last_status,
                        )?;
                    }
                }
                Some(existing) => {
                    let replacement = Schedule {
                        id: existing.id.clone(),
                        project: existing.project.clone(),
                        name: existing.name.clone(),
                        spec_kind: kind,
                        spec,
                        tz,
                        action: entry.action.clone(),
                        missed_policy: missed,
                        state: existing.state,
                        last_run_at: existing.last_run_at,
                        next_run_at,
                        last_status: existing.last_status,
                        created_at: existing.created_at,
                    };
                    store.transaction(|tx| {
                        tx.remove_schedule(existing.id.as_str())?;
                        tx.insert_schedule(&replacement)
                    })?;
                }
                None => {
                    let schedule = Schedule {
                        id: ScheduleId::new(),
                        project: Some(cfg.project.name.clone()),
                        name: entry.name.clone(),
                        spec_kind: kind,
                        spec,
                        tz,
                        action: entry.action.clone(),
                        missed_policy: missed,
                        state: ActiveState::Active,
                        last_run_at: None,
                        next_run_at,
                        last_status: String::new(),
                        created_at: now,
                    };
                    store.insert_schedule(&schedule)?;
                }
            }
        }
        Ok(())
    }

    /// Startup catch-up for occurrences missed while the supervisor was down.
    ///
    /// A lateness of at most two ticks is left to [`ScheduleEngine::tick`]; a longer gap fires
    /// once under `run_once` (the earliest missed occurrence, then collapses every later one)
    /// or records `skipped missed run` under `skip`. Both policies re-arm before returning.
    pub fn catch_up(&self, store: &Store, now: Timestamp) -> Result<Vec<DueRun>, CoreError> {
        let mut due_runs = Vec::new();
        for schedule in store.list_schedules()? {
            if schedule.state != ActiveState::Active {
                continue;
            }
            let Some(due_at) = schedule.next_run_at else {
                continue;
            };
            if due_at > now || now.since(due_at) <= self.late_threshold() {
                continue;
            }
            let parsed = ScheduleSpec::parse_at(schedule.spec_kind, &schedule.spec, &schedule.tz, schedule.created_at)?;
            let next_run_at = parsed.next_after(now);
            let key = idempotency_key(&schedule.id, due_at);
            let already_fired = self.already_fired(store, &key)?;
            match schedule.missed_policy {
                MissedPolicy::RunOnce => {
                    store.record_schedule_run(&schedule.id, now, next_run_at, "missed: run_once")?;
                    if !already_fired {
                        due_runs.push(DueRun { schedule, due_at });
                    }
                }
                MissedPolicy::Skip => {
                    store.record_schedule_run(&schedule.id, now, next_run_at, "skipped missed run")?;
                    if !already_fired {
                        self.append_schedule_row(
                            store,
                            &schedule,
                            &key,
                            "skipped missed run",
                            json!({"skipped": true}),
                        )?;
                    }
                }
            }
        }
        Ok(due_runs)
    }

    /// Returns every active schedule whose occurrence is due, after advancing it.
    ///
    /// The row's `last_run_at` is set to `now` and `next_run_at` to the first occurrence after
    /// `now`, so the caller's [`ScheduleEngine::fire`] cannot double-fire. A longer-than-two-tick
    /// gap applies the missed policy exactly as [`ScheduleEngine::catch_up`] does.
    pub fn tick(&self, store: &Store, now: Timestamp) -> Result<Vec<DueRun>, CoreError> {
        // loops.md §11 step 3a: an active schedule without a next time is armed, not fired.
        for schedule in store.list_schedules()? {
            if schedule.state == ActiveState::Active && schedule.next_run_at.is_none() {
                let parsed =
                    ScheduleSpec::parse_at(schedule.spec_kind, &schedule.spec, &schedule.tz, schedule.created_at)?;
                store.record_schedule_run(
                    &schedule.id,
                    schedule.last_run_at.unwrap_or(schedule.created_at),
                    parsed.next_after(now),
                    &schedule.last_status,
                )?;
            }
        }

        let mut due_runs = Vec::new();
        for schedule in store.due_schedules(now)? {
            let Some(due_at) = schedule.next_run_at else {
                continue;
            };
            let parsed = ScheduleSpec::parse_at(schedule.spec_kind, &schedule.spec, &schedule.tz, schedule.created_at)?;
            let next_run_at = parsed.next_after(now);
            let key = idempotency_key(&schedule.id, due_at);
            if self.already_fired(store, &key)? {
                store.record_schedule_run(&schedule.id, now, next_run_at, "duplicate: already fired")?;
                continue;
            }

            let late = now.since(due_at) > self.late_threshold();
            if late && schedule.missed_policy == MissedPolicy::Skip {
                store.record_schedule_run(&schedule.id, now, next_run_at, "skipped missed run")?;
                self.append_schedule_row(store, &schedule, &key, "skipped missed run", json!({"skipped": true}))?;
                continue;
            }

            let status = if late { "missed: run_once" } else { "due" };
            store.record_schedule_run(&schedule.id, now, next_run_at, status)?;
            due_runs.push(DueRun { schedule, due_at });
        }
        Ok(due_runs)
    }

    /// Writes the idempotency-key ledger row, then executes the schedule's action.
    ///
    /// The ledger row is written first so a crash between the write and the action leaves at
    /// most one unconfirmed firing; a repeated call with the same key is a no-op.
    pub fn fire(&self, store: &Store, due: &DueRun) -> Result<FireOutcome, CoreError> {
        let key = idempotency_key(&due.schedule.id, due.due_at);
        if self.already_fired(store, &key)? {
            return Ok(FireOutcome::Skipped("already fired".to_owned()));
        }
        let action = action_label(&due.schedule.action);
        self.append_schedule_row(
            store,
            &due.schedule,
            &key,
            &format!("schedule {} fired ({action})", due.schedule.name),
            json!({"due": due.due_at.as_rfc3339(), "action": action}),
        )?;

        let now = store.clock().now();
        let (outcome, status) = match &due.schedule.action {
            ScheduleAction::JobTemplate { template } => {
                let suffix = due_suffix(due.due_at)?;
                let new_id = JobId::parse(&format!("{}-{suffix}", template.id))?;
                let mut new = template.clone();
                new.id = new_id;
                let project = due
                    .schedule
                    .project
                    .clone()
                    .unwrap_or_else(|| self.deps.config.project.name.clone());
                let created_by = format!("schedule:{}", due.schedule.id);
                let job = store.insert_job(&project, &new, &created_by)?;
                (FireOutcome::JobCreated(job.id), None)
            }
            ScheduleAction::Loop { loop_name } => {
                crate::requests::push(
                    store,
                    &Request::RunLoop {
                        name: loop_name.clone(),
                    },
                )?;
                (FireOutcome::LoopTriggered(loop_name.clone()), None)
            }
            ScheduleAction::Command {
                argv,
                cwd,
                timeout_secs,
            } => {
                if argv.is_empty() {
                    return Err(CoreError::Invalid(format!(
                        "schedule {}: command argv is empty",
                        due.schedule.name
                    )));
                }
                let output = self.deps.cmd.run(&CommandSpec {
                    shell: None,
                    argv: argv.clone(),
                    cwd: cwd.as_ref().map(PathBuf::from),
                    env: Vec::new(),
                    timeout: Duration::from_secs((*timeout_secs).max(1)),
                })?;
                let tail = output.tail_lines(200);
                let status = if output.timed_out {
                    "failed: timeout".to_owned()
                } else {
                    match output.exit_code {
                        Some(0) => "ok".to_owned(),
                        Some(code) => format!("failed: exit {code}"),
                        None => "failed: signal".to_owned(),
                    }
                };
                self.append_schedule_row(
                    store,
                    &due.schedule,
                    &key,
                    &format!("command {status}"),
                    json!({
                        "result": true,
                        "tail": tail,
                        "timed_out": output.timed_out,
                        "exit_code": output.exit_code,
                    }),
                )?;
                if output.exit_code != Some(0) {
                    self.open_failure_notice(store, &due.schedule, due.due_at, &status, &tail, now)?;
                }
                (
                    FireOutcome::CommandRan {
                        exit_code: output.exit_code,
                    },
                    Some(status),
                )
            }
        };

        let status = status.unwrap_or_else(|| outcome_status(&outcome));
        self.update_status(store, &due.schedule, &status, now)?;
        Ok(outcome)
    }

    /// Writes one `schedule` ledger row and returns its id.
    fn append_schedule_row(
        &self,
        store: &Store,
        schedule: &Schedule,
        key: &str,
        summary: &str,
        detail: serde_json::Value,
    ) -> Result<i64, CoreError> {
        let mut detail = detail;
        if let Some(object) = detail.as_object_mut() {
            object.insert("key".to_owned(), json!(key));
        }
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Schedule,
            project: schedule.project.clone(),
            job_id: None,
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: summary.to_owned(),
            detail,
        };
        Ok(store.append_ledger(&entry)?)
    }

    /// Returns true when a `schedule` ledger row carries `key` in `detail_json`.
    fn already_fired(&self, store: &Store, key: &str) -> Result<bool, CoreError> {
        let rows = store.ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Schedule),
            job_id: None,
            since: None,
            limit: None,
        })?;
        Ok(rows
            .iter()
            .any(|entry| entry.detail.get("key").and_then(serde_json::Value::as_str) == Some(key)))
    }

    /// Updates only the schedule's `last_status`, keeping its advanced run times.
    fn update_status(&self, store: &Store, schedule: &Schedule, status: &str, now: Timestamp) -> Result<(), CoreError> {
        let Some(current) = store.schedule(schedule.id.as_str())? else {
            return Ok(());
        };
        store.record_schedule_run(
            &current.id,
            current.last_run_at.unwrap_or(now),
            current.next_run_at,
            status,
        )?;
        Ok(())
    }

    /// Opens one inbox `notice` per schedule per UTC day for a failed command.
    fn open_failure_notice(
        &self,
        store: &Store,
        schedule: &Schedule,
        due_at: Timestamp,
        status: &str,
        tail: &str,
        now: Timestamp,
    ) -> Result<(), CoreError> {
        let title = format!("schedule {} command failed", schedule.name);
        let since = start_of_day(now);
        for item in store.list_inbox(None)? {
            if item.kind == InboxKind::Notice && item.title == title && item.created_at >= since {
                return Ok(());
            }
        }
        let body: String = format!(
            "schedule {} firing at {}: {status}\n\n{}",
            schedule.name,
            due_at.as_rfc3339(),
            tail
        )
        .chars()
        .take(4000)
        .collect();
        let item = InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: schedule.project.clone(),
            job_id: None,
            kind: InboxKind::Notice,
            title,
            body,
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now,
            answered_at: None,
        };
        store.insert_inbox(&item)?;
        Ok(())
    }

    /// Two supervisor ticks: a shorter gap is an ordinary due run.
    fn late_threshold(&self) -> Duration {
        Duration::from_secs(self.deps.config.node.tick_secs.max(1).saturating_mul(2))
    }
}

/// Returns the idempotency key `"<schedule id>@<due RFC 3339>"`.
fn idempotency_key(id: &ScheduleId, due: Timestamp) -> String {
    format!("{id}@{}", due.as_rfc3339())
}

/// Returns the short action name used in ledger detail.
fn action_label(action: &ScheduleAction) -> &'static str {
    match action {
        ScheduleAction::JobTemplate { .. } => "job_template",
        ScheduleAction::Loop { .. } => "loop",
        ScheduleAction::Command { .. } => "command",
    }
}

/// Returns the status stored on a successful action.
fn outcome_status(outcome: &FireOutcome) -> String {
    match outcome {
        FireOutcome::JobCreated(job) => format!("ok: job {job}"),
        FireOutcome::LoopTriggered(name) => format!("ok: loop {name}"),
        FireOutcome::CommandRan { exit_code: Some(0) } => "ok".to_owned(),
        FireOutcome::CommandRan { exit_code: Some(code) } => format!("failed: exit {code}"),
        FireOutcome::CommandRan { exit_code: None } => "failed: signal".to_owned(),
        FireOutcome::Skipped(reason) => format!("skipped: {reason}"),
    }
}

/// Returns the `-<yyyymmddhhmm>` suffix for a job-template firing at `due`, in UTC.
fn due_suffix(due: Timestamp) -> Result<String, CoreError> {
    let text = due.as_rfc3339();
    let digits: String = text.chars().filter(char::is_ascii_digit).take(12).collect();
    if digits.len() != 12 {
        return Err(CoreError::Invalid(format!("cannot format schedule suffix from {text}")));
    }
    Ok(digits)
}

/// Returns midnight UTC of `now`.
fn start_of_day(now: Timestamp) -> Timestamp {
    let secs = now.unix_secs();
    Timestamp::from_unix_secs(secs - secs.rem_euclid(86_400))
}
