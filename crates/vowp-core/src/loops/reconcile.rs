//! Startup and per-tick reconciliation: orphaned runs, workers, loop rows and schedules.
//!
//! Spec: `docs/design/loops.md` §1 steps 1 and 13, and §11 catch-up. A run row without a live
//! runner is an orphan: the supervisor finalises it as `crash` and lets L2 decide whether to
//! continue the job.

use std::collections::HashSet;

use vowp_adapters::proc::{Signal, process_alive, signal_group};
use vowp_protocol::{ActiveState, Loop, LoopKind, NodeName, Run, RunId, RunOutcome, WorkerState};
use vowp_store::{RunFinish, Store};

use crate::CoreError;
use crate::deps::Deps;

use super::supervisor::ReconcileReport;

/// Finalises open runs from `open` that no live runner owns.
///
/// `open` and `active` must come from the same snapshot: a run that appeared after the
/// snapshot has no owner id yet, and must wait for the next tick instead of being
/// finalised as an orphan while its runner thread is still starting.
pub(crate) fn orphaned_runs(
    deps: &Deps,
    store: &Store,
    open: &[Run],
    active: &HashSet<RunId>,
) -> Result<u32, CoreError> {
    let mut count = 0;
    for snapshot in open {
        // The row may have finished since the snapshot; never signal or finish it again.
        let Some(run) = store.run(&snapshot.id)? else {
            continue;
        };
        if run.ended_at.is_some() || run.node != deps.node || active.contains(&run.id) {
            continue;
        }
        if let Some(pid) = run.pid
            && process_alive(pid)
        {
            // The child survived a supervisor crash; kill its process group before marking
            // the run crashed. A missing pid or an already-dead process needs no signal.
            let _ = signal_group(pid, Signal::Kill);
        }
        finish_crash(store, &run)?;
        count += 1;
    }
    free_dead_workers(store, &deps.node)?;
    Ok(count)
}

/// Marks one orphaned run as crashed; an already-finished row is left alone.
fn finish_crash(store: &Store, run: &Run) -> Result<(), CoreError> {
    let finish = RunFinish {
        outcome: RunOutcome::Crash,
        exit_code: run.exit_code,
        limit_kind: run.limit_kind,
        cost: run.cost.clone(),
        native_session_id: run.native_session_id.clone(),
        detail: "orphaned at supervisor start".to_owned(),
    };
    match store.finish_run(&run.id, &finish) {
        Ok(()) => {
            // An orphan never had a live runner thread to run L6; the marker lets the
            // harvest step apply the L2 crash path without waiting for it.
            store.kv_set(&format!("l6:{}", run.id), "orphan")?;
            Ok(())
        }
        Err(vowp_store::StoreError::Conflict(_)) => Ok(()),
        Err(error) => Err(CoreError::Store(error)),
    }
}

/// Frees busy worker rows whose run row no longer exists or has ended.
pub(crate) fn free_dead_workers(store: &Store, node: &NodeName) -> Result<u32, CoreError> {
    let open: HashSet<RunId> = store.open_runs()?.into_iter().map(|run| run.id).collect();
    let mut freed = 0;
    for worker in store.list_workers(node)? {
        if worker.state != WorkerState::Busy {
            continue;
        }
        let dead = match &worker.run_id {
            Some(run) => !open.contains(run),
            None => true,
        };
        if dead {
            store.upsert_worker(&vowp_protocol::Worker {
                node: node.clone(),
                slot: worker.slot,
                kind: worker.kind,
                state: WorkerState::Idle,
                run_id: None,
                job_id: None,
                window: None,
                updated_at: store.clock().now(),
            })?;
            freed += 1;
        }
    }
    Ok(freed)
}

/// Creates the nine default loop rows on first start and fills missing fields from config.
pub(crate) fn ensure_loops(store: &Store, deps: &Deps) -> Result<(), CoreError> {
    let defaults: [(&str, LoopKind, u64); 9] = [
        ("l1-supervisor", LoopKind::L1, 60),
        ("l2-worker", LoopKind::L2, 0),
        ("l3-verify", LoopKind::L3, 0),
        ("l4-planner", LoopKind::L4, 1800),
        ("l5-keeper", LoopKind::L5, 1200),
        ("l6-failover", LoopKind::L6, 0),
        ("l7-inbox", LoopKind::L7, 0),
        ("l8-watchdog", LoopKind::L8, 300),
        ("l9-schedules", LoopKind::L9, 60),
    ];
    let now = deps.clock.now();
    for (name, kind, default_interval) in defaults {
        if store.loop_(name)?.is_some() {
            continue;
        }
        let config = deps.config.loops.get(name);
        let interval_secs = config
            .and_then(|config| config.interval_secs)
            .unwrap_or(default_interval);
        let bounds = config.map(|config| config.bounds.clone()).unwrap_or_default();
        let state = config.and_then(|config| config.state).unwrap_or(ActiveState::Active);
        let next_run_at = (interval_secs > 0).then(|| now.plus(std::time::Duration::from_secs(interval_secs)));
        store.upsert_loop(&Loop {
            name: name.to_owned(),
            kind,
            project: Some(deps.config.project.name.clone()),
            interval_secs,
            state,
            bounds,
            last_run_at: None,
            next_run_at,
            last_status: String::new(),
            fail_streak: 0,
            updated_at: now,
        })?;
    }
    Ok(())
}

/// Creates the fixed tmux layout if the project session has no windows.
pub(crate) fn ensure_windows(deps: &Deps) -> Result<u32, CoreError> {
    let session = vowp_tmux::session_name_for_project(deps.config.project.name.as_str());
    if !deps.windows.list_windows(&session)?.is_empty() {
        return Ok(0);
    }
    let argv = |name: &str| vec!["vowp".to_owned(), "__window".to_owned(), name.to_owned()];
    let spec = vowp_tmux::LayoutSpec {
        board: argv("board"),
        supervisor: argv("supervisor"),
        planner: argv("planner"),
        keeper: argv("keeper"),
        inbox: argv("inbox"),
        cwd: None,
    };
    deps.windows.ensure_layout(deps.config.project.name.as_str(), &spec)?;
    Ok(u32::try_from(vowp_tmux::FIXED_WINDOWS.len()).unwrap_or(5))
}

/// Runs startup reconciliation and returns the report counters.
pub(crate) fn startup(
    deps: &Deps,
    store: &Store,
    open: &[Run],
    active: &HashSet<RunId>,
) -> Result<ReconcileReport, CoreError> {
    // Step 1 owns orphan classification; the session service's broader recovery runs after it
    // so its own "close every dangling run" pass cannot hide the counter from this report.
    let orphaned_runs = orphaned_runs(deps, store, open, active)?;
    let session_report = crate::session::service::SessionService::new(deps.clone()).recover(store)?;
    ensure_loops(store, deps)?;
    let windows_recreated = ensure_windows(deps).unwrap_or_else(|error| {
        deps.log.log(
            vowp_protocol::Level::Warn,
            "l1",
            &format!("cannot recreate tmux layout: {error}"),
        );
        0
    });
    let engine = crate::schedules::ScheduleEngine::new(deps.clone());
    engine.sync_from_config(store, &deps.config)?;
    // A paused L9 loop must not fire missed schedules on startup; the per-tick step makes
    // the same check. The rows stay armed, so resuming the loop lets the next tick apply
    // the normal missed-run policy.
    let l9_active = store
        .loop_("l9-schedules")?
        .is_none_or(|loop_| loop_.state == ActiveState::Active);
    let mut schedules_caught_up = 0;
    if l9_active {
        let due = engine.catch_up(store, deps.clock.now())?;
        for due_run in &due {
            match engine.fire(store, due_run) {
                Ok(_) => schedules_caught_up += 1,
                Err(error) => deps.log.log(
                    vowp_protocol::Level::Error,
                    "l9",
                    &format!("cannot fire missed schedule {}: {error}", due_run.schedule.name),
                ),
            }
        }
    }
    Ok(ReconcileReport {
        orphaned_runs,
        switches_recovered: session_report.switches_completed + session_report.switches_aborted,
        schedules_caught_up,
        windows_recreated,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use vowp_protocol::{
        Clock, Cost, Executor, FakeClock, MemoryLog, NodeName, Run, RunId, RunPurpose, RunRole, Timestamp, VowpConfig,
    };
    use vowp_store::NodePaths;

    use super::orphaned_runs;
    use crate::Deps;
    use crate::SystemGit;
    use crate::deps::{AdapterRegistry, FixedRng, SystemRunner};

    #[test]
    fn a_run_that_appears_after_the_snapshot_is_not_an_orphan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path().join("node"));
        let clock = Arc::new(FakeClock::at(
            Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp"),
        ));
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: NodeName::parse("local").expect("node"),
            clock: clock.clone(),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        let store = deps.open_store().expect("store");
        let run = Run {
            id: RunId::new(),
            job_id: None,
            session_id: None,
            role: RunRole::Probe,
            purpose: RunPurpose::Probe,
            profile: None,
            rung: 0,
            executor: Some(Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor")),
            provider: Some("claude".to_owned()),
            node: deps.node.clone(),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at: clock.now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        };
        store.insert_run(&run).expect("insert run");

        // The supervisor took this snapshot just before the probe thread inserted its run row.
        // A run absent from the snapshot has no owner id yet and must wait for the next tick.
        let count = orphaned_runs(&deps, &store, &[], &HashSet::new()).expect("reconcile");
        assert_eq!(count, 0, "the new run must not be finalised as an orphan");
        assert_eq!(
            store.require_run(&run.id).expect("run").ended_at,
            None,
            "the live probe run row stays open"
        );
    }
}
