//! L6 failover: quota/credits/auth/rate/context outcomes, unknown-failure streaks and probes.
//!
//! Spec: `docs/design/loops.md` §8 and decisions.md D15. L6 never parses harness text: it
//! consumes the adapter's [`vowp_protocol::RunSummary`] classification and writes `limits`,
//! ledger and park requests. Moving a session to the next ladder rung happens through the
//! session switch protocol so the new executor is seeded with a capsule.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use vowp_protocol::{
    CancelToken, CapsuleTrigger, Executor, ExecutorRef, HarnessKind, LedgerEntry, LedgerKind, Limit, LimitEvent,
    LimitKind, LimitRowKind, NewSession, ParkedReason, Profile, Run, RunOutcome, RunPurpose, RunRole, RunSummary,
    SessionId, SessionRole, SessionState, SwitchReason, SwitchRequest, Timestamp,
};
use vowp_store::{SessionFilter, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::session::service::SessionService;

/// The single two-minute margin applied when a quota reset time is recorded.
const LIMIT_MARGIN: Duration = Duration::from_secs(120);

/// Default rate backoff base.
const RATE_BASE: Duration = Duration::from_secs(600);

/// Maximum rate backoff.
const RATE_MAX: Duration = Duration::from_secs(3600);

/// Unknown-failure streak that marks a provider unavailable.
const UNKNOWN_FAILURE_TRIP: u32 = 3;

/// How long an unavailable provider stays out.
const UNAVAILABLE_FOR: Duration = Duration::from_secs(15 * 60);

/// Probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Result of one probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The provider answered; its limit row was cleared.
    Available,
    /// The probe hit a limit again; `resets_at` is the new reset when reported.
    StillLimited {
        /// New reset instant, when the probe reported one.
        resets_at: Option<Timestamp>,
    },
    /// The probe failed for another reason; it counts toward `probe_fail_streak`.
    Failed(String),
}

/// What L6 decided for one finished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverAction {
    /// Nothing to do.
    None,
    /// A rate limit: the continue run waits until `until` (T35).
    Backoff {
        /// First instant the continue run may start.
        until: Timestamp,
    },
    /// The session moved to the next ladder rung.
    NextRung {
        /// New rung index.
        rung: usize,
    },
    /// Park every job whose current rung uses this provider.
    ParkProvider {
        /// Provider to park.
        provider: String,
    },
    /// Park the affected job only.
    ParkJob {
        /// Why it is parked.
        reason: ParkedReason,
    },
    /// The context window was exhausted and the session was reseeded.
    FreshSessionWithCapsule,
    /// A probe succeeded and sessions returned to the preferred rung.
    ProbeBack {
        /// Provider that came back.
        provider: String,
    },
}

/// The L6 failover loop.
#[derive(Clone)]
pub struct Failover {
    deps: Deps,
}

impl Failover {
    /// Creates a failover loop over `deps`.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Applies the failover policy for one finished run.
    ///
    /// Limit rows, the ledger and park requests are written here; the caller applies the
    /// returned decision to the job graph (parking, T35 delay, next rung).
    pub fn on_run_outcome(&self, store: &Store, run: &Run, summary: &RunSummary) -> Result<FailoverAction, CoreError> {
        let Some(provider) = provider_of(run) else {
            return Ok(FailoverAction::None);
        };
        if summary.limit.is_some() {
            // A classified limit is not an "unknown" failure; it breaks that streak.
            store.kv_set(&format!("fail_streak:{provider}"), "0")?;
        }
        match summary.limit.as_ref().map(|limit| limit.kind) {
            Some(LimitKind::Quota) => self.on_quota(store, run, summary, &provider),
            Some(LimitKind::Credits) => self.on_credits_or_auth(store, run, summary, &provider, true),
            Some(LimitKind::Auth) => self.on_credits_or_auth(store, run, summary, &provider, false),
            Some(LimitKind::Rate) => self.on_rate(store, run, summary, &provider),
            Some(LimitKind::Context) => self.on_context(store, run),
            None => match summary.outcome {
                RunOutcome::Limit => self.on_quota(store, run, summary, &provider),
                RunOutcome::Credits => self.on_credits_or_auth(store, run, summary, &provider, true),
                RunOutcome::Auth => self.on_credits_or_auth(store, run, summary, &provider, false),
                RunOutcome::Rate => self.on_rate(store, run, summary, &provider),
                RunOutcome::Context => self.on_context(store, run),
                RunOutcome::Crash | RunOutcome::Error | RunOutcome::Killed => {
                    self.on_unknown_failure(store, run, summary, &provider)
                }
                RunOutcome::Ok => {
                    store.kv_set(&format!("fail_streak:{provider}"), "0")?;
                    store.kv_set(&format!("rate_streak:{provider}"), "0")?;
                    Ok(FailoverAction::None)
                }
                RunOutcome::Stalled => Ok(FailoverAction::None),
            },
        }
    }

    /// Writes a quota limit and either moves the session to the next rung or parks the jobs.
    fn on_quota(
        &self,
        store: &Store,
        run: &Run,
        summary: &RunSummary,
        provider: &str,
    ) -> Result<FailoverAction, CoreError> {
        let now = store.clock().now();
        let event = summary.limit.as_ref();
        let existing = store.limit_for(provider)?;
        let resets_at = event
            .and_then(|limit| limit.resets_at)
            .map(|at| at.plus(LIMIT_MARGIN))
            .or_else(|| {
                existing
                    .as_ref()
                    .filter(|row| row.kind == LimitRowKind::Quota)
                    .and_then(|row| row.resets_at)
            })
            .unwrap_or_else(|| now.plus(Duration::from_secs(60 * 60)));
        let detail = event
            .map(|limit| limit.message.clone())
            .unwrap_or_else(|| "quota exhausted".to_owned());
        self.write_limit(
            store,
            run,
            provider,
            LimitRowKind::Quota,
            Some(resets_at),
            &detail,
            None,
            None,
        )?;
        let policy = self
            .profile_for(run)
            .map(|profile| profile.on_quota)
            .unwrap_or(vowp_protocol::OnQuota::NextThenProbeBack);
        self.mark_provider(
            store,
            provider,
            "quota",
            policy == vowp_protocol::OnQuota::NextThenProbeBack,
        )?;
        if policy == vowp_protocol::OnQuota::Park {
            self.request_park(store, provider, "provider_blocked")?;
            return Ok(FailoverAction::ParkProvider {
                provider: provider.to_owned(),
            });
        }
        let next = self.next_rung(run, store, now)?;
        let Some((rung, executor, node)) = next else {
            self.request_park(store, provider, "provider_blocked")?;
            return Ok(FailoverAction::ParkProvider {
                provider: provider.to_owned(),
            });
        };
        let Some(session) = &run.session_id else {
            self.request_park(store, provider, "provider_blocked")?;
            return Ok(FailoverAction::ParkJob {
                reason: ParkedReason::ProviderBlocked,
            });
        };
        let service = SessionService::new(self.deps.clone());
        let to = ExecutorRef { executor, node };
        match service.switch(&SwitchRequest {
            session_id: session.clone(),
            to: to.clone(),
            reason: SwitchReason::Limit,
            drain_secs: 5,
        }) {
            Ok(switch) => {
                self.record_switch(store, Some(run), &switch.session_id, &switch.to_executor, "limit")?;
                Ok(FailoverAction::NextRung { rung })
            }
            Err(error) => {
                self.deps.log.log(
                    vowp_protocol::Level::Error,
                    "l6",
                    &format!("cannot switch {session} to rung {rung}: {error}"),
                );
                self.request_park(store, provider, "provider_blocked")?;
                // loops.md §8.4.3: a failed switch still leaves a `switch` ledger
                // failure behind, in addition to parking the provider.
                self.record_switch_failure(store, Some(run), session, &to.executor, "limit", &error)?;
                Ok(FailoverAction::ParkProvider {
                    provider: provider.to_owned(),
                })
            }
        }
    }

    /// Credits and auth both park every job whose current rung uses the provider.
    fn on_credits_or_auth(
        &self,
        store: &Store,
        run: &Run,
        summary: &RunSummary,
        provider: &str,
        credits: bool,
    ) -> Result<FailoverAction, CoreError> {
        let kind = if credits {
            LimitRowKind::Credits
        } else {
            LimitRowKind::Auth
        };
        let detail = summary
            .limit
            .as_ref()
            .map(|limit| limit.message.clone())
            .unwrap_or_else(|| {
                if credits {
                    "credits exhausted".to_owned()
                } else {
                    "authentication failed".to_owned()
                }
            });
        self.write_limit(store, run, provider, kind, None, &detail, None, None)?;
        self.mark_provider(store, provider, if credits { "credits" } else { "auth" }, false)?;
        self.request_park(store, provider, if credits { "credits" } else { "auth" })?;
        Ok(FailoverAction::ParkProvider {
            provider: provider.to_owned(),
        })
    }

    /// Writes the deterministic rate backoff and lowers the provider concurrency cap.
    fn on_rate(
        &self,
        store: &Store,
        run: &Run,
        summary: &RunSummary,
        provider: &str,
    ) -> Result<FailoverAction, CoreError> {
        let now = store.clock().now();
        let key = format!("rate_streak:{provider}");
        let streak = store
            .kv_get(&key)?
            .and_then(|text| text.parse::<u32>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        store.kv_set(&key, &streak.to_string())?;
        let shift = streak.saturating_sub(1).min(6);
        let base = RATE_BASE
            .as_secs()
            .saturating_mul(1_u64 << shift)
            .min(RATE_MAX.as_secs());
        let span = base / 5;
        let draw = self.deps.rng.jitter_secs(span.saturating_mul(2));
        let offset = if draw == 0 { 0 } else { draw as i64 - span as i64 };
        let computed = now.plus(Duration::from_secs((base as i64 + offset).max(1) as u64));
        // loops.md §8.2: every rate event records `now + backoff`. A second event
        // inside the existing window may extend it, but never shorten it below the
        // reset already recorded for that provider.
        let existing = store
            .limit_for(provider)?
            .filter(|row| row.kind == LimitRowKind::Rate)
            .and_then(|row| row.resets_at);
        let until = existing.map_or(computed, |existing| computed.max(existing));
        let running = store
            .open_runs()?
            .iter()
            .filter(|open| provider_of(open).as_deref() == Some(provider))
            .count() as u32;
        let cap = running.saturating_sub(1).max(1);
        let detail = summary
            .limit
            .as_ref()
            .map(|limit| limit.message.clone())
            .unwrap_or_else(|| "rate limited".to_owned());
        self.write_limit(
            store,
            run,
            provider,
            LimitRowKind::Rate,
            Some(until),
            &detail,
            Some(cap),
            Some(until),
        )?;
        self.mark_provider(store, provider, "rate", true)?;
        Ok(FailoverAction::Backoff { until })
    }

    /// Writes a capsule and reseeds the same harness on a fresh native session.
    fn on_context(&self, store: &Store, run: &Run) -> Result<FailoverAction, CoreError> {
        let Some(session) = &run.session_id else {
            return Ok(FailoverAction::None);
        };
        let Some(executor) = run.executor.clone() else {
            return Ok(FailoverAction::None);
        };
        let service = SessionService::new(self.deps.clone());
        service.write_capsule(session, CapsuleTrigger::Context)?;
        let switch = service.switch(&SwitchRequest {
            session_id: session.clone(),
            to: ExecutorRef { executor, node: None },
            reason: SwitchReason::Context,
            drain_secs: 5,
        })?;
        self.record_switch(store, Some(run), &switch.session_id, &switch.to_executor, "context")?;
        Ok(FailoverAction::FreshSessionWithCapsule)
    }

    /// Counts an unknown error/crash; three in a row make the provider unavailable for 15 min.
    fn on_unknown_failure(
        &self,
        store: &Store,
        run: &Run,
        summary: &RunSummary,
        provider: &str,
    ) -> Result<FailoverAction, CoreError> {
        if summary.limit.is_some() {
            return Ok(FailoverAction::None);
        }
        let key = format!("fail_streak:{provider}");
        let streak = store
            .kv_get(&key)?
            .and_then(|text| text.parse::<u32>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        if streak < UNKNOWN_FAILURE_TRIP {
            store.kv_set(&key, &streak.to_string())?;
            return Ok(FailoverAction::None);
        }
        let now = store.clock().now();
        let resets_at = now.plus(UNAVAILABLE_FOR);
        self.write_limit(
            store,
            run,
            provider,
            LimitRowKind::Unavailable,
            Some(resets_at),
            &format!("{UNKNOWN_FAILURE_TRIP} consecutive unknown failures"),
            None,
            None,
        )?;
        store.kv_set(&key, "0")?;
        self.mark_provider(store, provider, "unavailable", true)?;
        Ok(FailoverAction::None)
    }

    /// Providers whose probes are due now.
    pub fn probe_due(&self, store: &Store, now: Timestamp) -> Result<Vec<String>, CoreError> {
        let mut providers = BTreeSet::new();
        for profile in self.deps.config.profiles.values() {
            for executor in &profile.ladder {
                providers.insert(executor.provider());
            }
        }
        for (key, _) in store.kv_prefix("limit:")? {
            if let Some(provider) = key.strip_prefix("limit:") {
                providers.insert(provider.to_owned());
            }
        }
        let interval = Duration::from_secs(
            u64::from(
                self.deps
                    .config
                    .loops
                    .get("l6-failover")
                    .and_then(|loop_| loop_.bounds.probe_interval_minutes)
                    .unwrap_or(15),
            ) * 60,
        );
        let open = store.open_runs()?;
        let mut due = Vec::new();
        for provider in providers {
            let Some(limit) = store.limit_for(&provider)? else {
                continue;
            };
            if !matches!(
                limit.kind,
                LimitRowKind::Quota | LimitRowKind::Rate | LimitRowKind::Unavailable
            ) {
                continue;
            }
            if store.kv_get(&format!("probe_back:{provider}"))?.as_deref() == Some("0") {
                continue;
            }
            if store.kv_get(&format!("probe:active:{provider}"))?.as_deref() == Some("1") {
                continue;
            }
            if let Some(resets_at) = limit.resets_at
                && now < resets_at
            {
                continue;
            }
            if let Some(last) = limit.last_probe_at
                && now < last.plus(interval)
            {
                continue;
            }
            if open
                .iter()
                .any(|run| run.role == RunRole::Probe && run.provider.as_deref() == Some(provider.as_str()))
            {
                continue;
            }
            due.push(provider);
        }
        Ok(due)
    }

    /// Runs one tiny probe turn on the limited provider and records the outcome.
    pub fn run_probe(&self, store: &Store, provider: &str) -> Result<ProbeVerdict, CoreError> {
        let now = store.clock().now();
        let Some(executor) = self.executor_for_provider(provider) else {
            return Ok(ProbeVerdict::Failed(format!(
                "no configured executor for provider {provider}"
            )));
        };
        let scratch = self.scratch_dir(provider, now);
        std::fs::create_dir_all(&scratch).map_err(|source| CoreError::Io {
            path: scratch.display().to_string(),
            source,
        })?;
        let service = SessionService::new(self.deps.clone());
        let session = service.create_session(NewSession {
            project: Some(self.deps.config.project.name.clone()),
            job_id: None,
            role: SessionRole::User,
            title: format!("probe {provider}"),
            cwd: scratch,
            parent: None,
            executor: Some(executor.clone()),
        })?;
        let result = service.run_turn(
            vowp_protocol::TurnRequest {
                session_id: session.clone(),
                executor: ExecutorRef {
                    executor: executor.clone(),
                    node: None,
                },
                purpose: RunPurpose::Probe,
                role: RunRole::Probe,
                job_id: None,
                prompt: super::prompts::PROBE_PROMPT.to_owned(),
                profile: None,
                rung: 0,
                window: None,
                timeout: Some(PROBE_TIMEOUT),
            },
            &CancelToken::default(),
        );
        match result {
            Ok(result) => match result.summary.outcome {
                RunOutcome::Ok => {
                    store.clear_limit(provider)?;
                    let _ = store.record_probe(provider, store.clock().now(), true);
                    self.record_limit_ledger(store, provider, "provider back", json!({"probe": "ok"}), None)?;
                    Ok(ProbeVerdict::Available)
                }
                RunOutcome::Limit | RunOutcome::Credits | RunOutcome::Auth | RunOutcome::Rate | RunOutcome::Context => {
                    let resets_at = result
                        .summary
                        .limit
                        .as_ref()
                        .and_then(|limit| limit.resets_at)
                        .map(|at| at.plus(LIMIT_MARGIN));
                    if let Some(existing) = store.limit_for(provider)? {
                        let detail = result
                            .summary
                            .limit
                            .as_ref()
                            .map(|limit| limit.message.clone())
                            .unwrap_or_else(|| "probe still limited".to_owned());
                        store.upsert_limit(&Limit {
                            provider: provider.to_owned(),
                            harness: existing.harness,
                            kind: existing.kind,
                            resets_at: resets_at.or(existing.resets_at),
                            detail,
                            since: existing.since,
                            last_probe_at: Some(store.clock().now()),
                            probe_fail_streak: existing.probe_fail_streak.saturating_add(1),
                            concurrency_cap: existing.concurrency_cap,
                            cap_until: existing.cap_until,
                        })?;
                    }
                    Ok(ProbeVerdict::StillLimited { resets_at })
                }
                _ => {
                    let _ = store.record_probe(provider, store.clock().now(), false);
                    Ok(ProbeVerdict::Failed(format!(
                        "probe ended with {:?}",
                        result.summary.outcome
                    )))
                }
            },
            Err(error) => {
                let _ = store.record_probe(provider, store.clock().now(), false);
                Ok(ProbeVerdict::Failed(error.to_string()))
            }
        }
    }

    /// Switches planner/keeper sessions back to their preferred rung after a successful probe.
    ///
    /// Returns `true` when every preferred session is already back (or does not exist), and
    /// `false` when a turn is still running and the hand-back must be retried on a quiet tick
    /// (loops.md §8.5: never mid-turn).
    pub fn probe_back(&self, store: &Store, provider: &str) -> Result<bool, CoreError> {
        let service = SessionService::new(self.deps.clone());
        let mut complete = true;
        for (profile_name, role) in [("planner", SessionRole::Planner), ("keeper", SessionRole::Keeper)] {
            let Some(profile) = self.deps.config.profiles.get(profile_name) else {
                continue;
            };
            let Some(preferred) = profile.ladder.first() else {
                continue;
            };
            if preferred.provider() != provider {
                continue;
            }
            let Some(session) = role_session(store, &self.deps.config.project.name, role)? else {
                continue;
            };
            if matches!(
                session.state,
                SessionState::Running | SessionState::Switching | SessionState::Closed
            ) {
                complete = false;
                continue;
            }
            let already_preferred = session.state != SessionState::Blocked
                && session.executor.as_ref().map(Executor::provider).as_deref() == Some(provider);
            if already_preferred {
                continue;
            }
            match service.switch(&SwitchRequest {
                session_id: session.id.clone(),
                to: ExecutorRef {
                    executor: preferred.clone(),
                    node: profile.node.clone(),
                },
                reason: SwitchReason::ProbeBack,
                drain_secs: 5,
            }) {
                Ok(handoff) => {
                    self.record_switch(store, None, &handoff.session_id, &handoff.to_executor, "probe_back")?;
                    if role == SessionRole::Planner {
                        store.kv_set("wake:planner", "1")?;
                        store.kv_delete("planner:wake_consumed")?;
                    }
                }
                Err(error) => {
                    self.deps.log.log(
                        vowp_protocol::Level::Warn,
                        "l6",
                        &format!("cannot switch {} back to {provider}: {error}", session.id),
                    );
                    complete = false;
                }
            }
        }
        Ok(complete)
    }

    /// Returns the next ladder rung after the run's rung whose provider is not limited.
    fn next_rung(
        &self,
        run: &Run,
        store: &Store,
        now: Timestamp,
    ) -> Result<Option<(usize, Executor, Option<vowp_protocol::NodeName>)>, CoreError> {
        let Some(profile) = self.profile_for(run) else {
            return Ok(None);
        };
        let limited = super::limited_map(store, now)?;
        for (index, executor) in profile.ladder.iter().enumerate() {
            if u32::try_from(index).unwrap_or(u32::MAX) <= run.rung {
                continue;
            }
            let provider = executor.provider();
            let blocked = match limited.get(&provider) {
                Some(None) => true,
                Some(Some(resets_at)) => *resets_at > now,
                None => false,
            };
            if !blocked {
                return Ok(Some((index, executor.clone(), profile.node.clone())));
            }
        }
        Ok(None)
    }

    /// Returns the configured profile of a run.
    fn profile_for(&self, run: &Run) -> Option<&Profile> {
        let name = run.profile.as_deref().or(match run.role {
            RunRole::Planner => Some("planner"),
            RunRole::Keeper => Some("keeper"),
            _ => None,
        })?;
        self.deps.config.profiles.get(name)
    }

    /// Returns the first configured executor for `provider`, preferring role order.
    fn executor_for_provider(&self, provider: &str) -> Option<Executor> {
        for name in ["planner", "keeper", "worker", "verifier"] {
            if let Some(profile) = self.deps.config.profiles.get(name)
                && let Some(executor) = profile.ladder.iter().find(|executor| executor.provider() == provider)
            {
                return Some(executor.clone());
            }
        }
        for profile in self.deps.config.profiles.values() {
            if let Some(executor) = profile.ladder.iter().find(|executor| executor.provider() == provider) {
                return Some(executor.clone());
            }
        }
        None
    }

    /// Returns a scratch directory inside the allowed roots for a probe.
    fn scratch_dir(&self, provider: &str, now: Timestamp) -> PathBuf {
        let root = self
            .deps
            .config
            .project
            .worktree_root
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| {
                self.deps
                    .config
                    .project
                    .path
                    .as_ref()
                    .map(|path| PathBuf::from(path).join(".vowp/probes"))
            })
            .unwrap_or_else(|| self.deps.paths.tmp_dir());
        root.join(format!("probe-{provider}-{}", now.unix_millis()))
    }

    /// Inserts or replaces one limit row and writes the matching ledger row.
    #[allow(clippy::too_many_arguments)]
    fn write_limit(
        &self,
        store: &Store,
        run: &Run,
        provider: &str,
        kind: LimitRowKind,
        resets_at: Option<Timestamp>,
        detail: &str,
        concurrency_cap: Option<u32>,
        cap_until: Option<Timestamp>,
    ) -> Result<(), CoreError> {
        let now = store.clock().now();
        store.upsert_limit(&Limit {
            provider: provider.to_owned(),
            harness: run.executor.as_ref().map(|executor| executor.harness),
            kind,
            resets_at,
            detail: super::clip_chars(detail, 1000),
            since: now,
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap,
            cap_until,
        })?;
        self.record_limit_ledger(
            store,
            provider,
            &format!("{kind:?} limit recorded"),
            json!({
                "kind": format!("{kind:?}").to_ascii_lowercase(),
                "resets_at": resets_at.map(|at| at.as_rfc3339()),
                "detail": super::clip_chars(detail, 300),
                "concurrency_cap": concurrency_cap,
            }),
            Some(run),
        )
    }

    /// Appends one ledger `limit` row.
    fn record_limit_ledger(
        &self,
        store: &Store,
        provider: &str,
        summary: &str,
        detail: serde_json::Value,
        run: Option<&Run>,
    ) -> Result<(), CoreError> {
        let project = run
            .and_then(|run| run.job_id.as_ref())
            .and_then(|job| store.job(job).ok().flatten())
            .map(|job| job.project)
            .unwrap_or_else(|| self.deps.config.project.name.clone());
        store.append_ledger(&LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Limit,
            project: Some(project),
            job_id: run.and_then(|run| run.job_id.clone()),
            run_id: run.map(|run| run.id.clone()),
            session_id: run.and_then(|run| run.session_id.clone()),
            role: run.map(|run| run.role),
            provider: Some(provider.to_owned()),
            harness: run.and_then(|run| run.executor.as_ref().map(|executor| executor.harness)),
            model: run.and_then(|run| run.executor.as_ref().map(|executor| executor.model.clone())),
            cost: None,
            summary: format!("{provider}: {summary}"),
            detail,
        })?;
        Ok(())
    }

    /// Remembers which providers have limit rows and whether probe-back applies.
    fn mark_provider(&self, store: &Store, provider: &str, kind: &str, probe_back: bool) -> Result<(), CoreError> {
        store.kv_set(&format!("limit:{provider}"), kind)?;
        store.kv_set(&format!("probe_back:{provider}"), if probe_back { "1" } else { "0" })?;
        // A limit/failover event is an unseen planner event (loops.md §7 due rule a).
        super::l4_planner::raise_wake(store, "planner")?;
        Ok(())
    }

    /// Asks the supervisor to park every job whose current rung uses `provider`.
    fn request_park(&self, store: &Store, provider: &str, reason: &str) -> Result<(), CoreError> {
        store.kv_set(&format!("park:{provider}"), reason)?;
        store.kv_set("wake:keeper", "1")?;
        store.kv_delete("keeper:wake_consumed")?;
        Ok(())
    }

    /// Appends a ledger `switch` row for a limit/failover/probe-back handoff.
    fn record_switch(
        &self,
        store: &Store,
        run: Option<&Run>,
        session: &SessionId,
        to: &Executor,
        reason: &str,
    ) -> Result<(), CoreError> {
        store.append_ledger(&LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Switch,
            project: Some(self.deps.config.project.name.clone()),
            job_id: run.and_then(|run| run.job_id.clone()),
            run_id: run.map(|run| run.id.clone()),
            session_id: Some(session.clone()),
            role: run.map(|run| run.role),
            provider: Some(to.provider()),
            harness: Some(to.harness),
            model: Some(to.model.clone()),
            cost: None,
            summary: format!("switch {session} to {to} ({reason})"),
            detail: json!({"reason": reason}),
        })?;
        Ok(())
    }

    /// Appends a ledger `switch` failure row (loops.md §8.4.3).
    fn record_switch_failure(
        &self,
        store: &Store,
        run: Option<&Run>,
        session: &SessionId,
        to: &Executor,
        reason: &str,
        error: &CoreError,
    ) -> Result<(), CoreError> {
        let detail = error.to_string();
        store.append_ledger(&LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Switch,
            project: Some(self.deps.config.project.name.clone()),
            job_id: run.and_then(|run| run.job_id.clone()),
            run_id: run.map(|run| run.id.clone()),
            session_id: Some(session.clone()),
            role: run.map(|run| run.role),
            provider: Some(to.provider()),
            harness: Some(to.harness),
            model: Some(to.model.clone()),
            cost: None,
            summary: format!("switch {session} to {to} failed ({reason}): {detail}"),
            detail: json!({"reason": reason, "outcome": "failed", "error": detail}),
        })?;
        Ok(())
    }
}

/// Returns the provider key of a run.
pub fn provider_of(run: &Run) -> Option<String> {
    run.provider
        .clone()
        .or_else(|| run.executor.as_ref().map(Executor::provider))
}

/// Finds the newest session of `role` for a project.
fn role_session(
    store: &Store,
    project: &vowp_protocol::ProjectName,
    role: SessionRole,
) -> Result<Option<vowp_protocol::Session>, CoreError> {
    Ok(store
        .list_sessions(&SessionFilter {
            project: Some(project.clone()),
            ..SessionFilter::default()
        })?
        .into_iter()
        .find(|session| session.role == Some(role)))
}

/// Maps one limit event kind to its row kind (used by tests and callers).
pub fn row_kind_for(kind: LimitKind) -> LimitRowKind {
    match kind {
        LimitKind::Quota => LimitRowKind::Quota,
        LimitKind::Credits => LimitRowKind::Credits,
        LimitKind::Rate => LimitRowKind::Rate,
        LimitKind::Context => LimitRowKind::Context,
        LimitKind::Auth => LimitRowKind::Auth,
    }
}

/// Returns `true` when `event` is a limit event of `kind` (small helper for callers).
pub fn is_limit(event: &Option<LimitEvent>, kind: LimitKind) -> bool {
    event.as_ref().is_some_and(|event| event.kind == kind)
}

/// Returns the harness of an executor, when one exists (kept for callers/tests).
pub fn harness_of(executor: Option<&Executor>) -> Option<HarnessKind> {
    executor.map(|executor| executor.harness)
}
