//! L1 tick, dispatch, stall/continue and L3 verify tests.
//! Spec: `docs/design/crates/vowp-core.md` §7 and `docs/design/loops.md` §1–3, §5, §12.

#[path = "loops_support.rs"]
mod h;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use h::*;
use vowp_core::requests::{Request, push};
use vowp_protocol::{
    Cost, EdgeKind, Executor, Gate, Job, JobKind, JobState, Limit, LimitRowKind, ParkedReason, Run, RunId, RunOutcome,
    RunPurpose, RunRole, Timestamp,
};
use vowp_store::JobFilter;

fn now() -> Timestamp {
    ts()
}

fn worker_job(node: &TestNode, id: &str) -> Job {
    node.insert_job(id, JobKind::Build, vowp_protocol::Role::Worker, Some(vec![]))
}

#[test]
fn tick_step_failure_does_not_abort_tick() {
    let node = TestNode::new();
    // Force the board step to fail by making its temporary path a directory.
    std::fs::create_dir_all(node.deps.paths.root().join(".board.json.tmp")).expect("blocking dir");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick still returns Ok");
    let board = report
        .steps
        .iter()
        .find(|step| step.name == "board")
        .expect("board step");
    assert!(!board.ok, "board step should have failed: {board:?}");
    let heartbeat = report
        .steps
        .iter()
        .find(|step| step.name == "heartbeat")
        .expect("heartbeat step");
    assert!(heartbeat.ok, "later steps must still run: {heartbeat:?}");
    assert!(node.deps.paths.root().join("heartbeat").is_file());
}

#[test]
fn tick_updates_the_l8_watchdog_row() {
    let node = TestNode::new();
    let store = node.store();
    let mut supervisor = node.supervisor();
    assert!(
        store
            .loop_("l8-watchdog")
            .expect("loop lookup")
            .expect("loop row")
            .last_run_at
            .is_none(),
        "the fresh loop row has not run yet"
    );

    supervisor.tick().expect("tick");

    let row = store.loop_("l8-watchdog").expect("loop lookup").expect("loop row");
    assert!(
        row.last_run_at.is_some(),
        "the heartbeat step records the L8 run: {row:?}"
    );
    assert_eq!(row.last_status, "heartbeat written", "{row:?}");
    assert_eq!(row.fail_streak, 0, "{row:?}");
}

#[test]
fn tick_does_not_block_on_slow_gate() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("dispatched while gating"),
    ));
    let node = TestNode::with_adapter(false, adapter);
    let gate_job = node.insert_job(
        "slow-gate",
        JobKind::Build,
        vowp_protocol::Role::Worker,
        Some(vec![Gate::Cmd(vowp_protocol::CmdGate {
            run: "sleep 1; echo slow-gate-tail >&2; exit 1".to_owned(),
            ..vowp_protocol::CmdGate::default()
        })]),
    );
    node.force_state(&gate_job, JobState::Gating);
    let dispatch_job = worker_job(&node, "during-gate");
    node.store()
        .patch_job(
            &dispatch_job.id,
            &vowp_protocol::JobPatch {
                owns: Some(vec!["other/**".to_owned()]),
                ..vowp_protocol::JobPatch::default()
            },
        )
        .expect("patch owns");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick");
    assert!(
        report.dispatched.contains(&dispatch_job.id),
        "dispatch must run before the gate finishes: {:?}",
        report.dispatched
    );
    assert_eq!(
        node.store().require_job(&gate_job.id).expect("gate job").state,
        JobState::Gating
    );
    assert!(node.deps.paths.root().join("heartbeat").is_file());
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&gate_job.id).expect("gate job").state == JobState::Fixing
    });
}

#[test]
fn tick_does_not_block_on_slow_merge() {
    let node = TestNode::with_adapter_config(
        true,
        Arc::new(ScriptAdapter::always(
            vowp_protocol::HarnessKind::Opencode,
            ScriptAction::ok("dispatched while merging"),
        )),
        |config| {
            config.merge.check_cmds = vec!["sleep 1; exit 0".to_owned()];
        },
    );
    let merge_job = node.insert_job("slow-merge", JobKind::Build, vowp_protocol::Role::Worker, Some(vec![]));
    node.worktree_with_file(&merge_job, "src/merge.rs", "pub fn merged() {}\n");
    node.force_state(&merge_job, JobState::Merging);
    let dispatch_job = worker_job(&node, "during-merge");
    node.store()
        .patch_job(
            &dispatch_job.id,
            &vowp_protocol::JobPatch {
                owns: Some(vec!["other/**".to_owned()]),
                ..vowp_protocol::JobPatch::default()
            },
        )
        .expect("patch owns");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick");
    assert!(
        report.dispatched.contains(&dispatch_job.id),
        "dispatch must run before the merge finishes: {:?}",
        report.dispatched
    );
    assert_eq!(
        node.store().require_job(&merge_job.id).expect("merge job").state,
        JobState::Merging
    );
    assert!(node.deps.paths.root().join("heartbeat").is_file());
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&merge_job.id).expect("merge job").state == JobState::Done
    });
}

#[test]
fn startup_marks_orphaned_runs_crash_and_continues_job() {
    let node = TestNode::new();
    let job = worker_job(&node, "orphan-job");
    node.force_state(&job, JobState::Running);
    let session = create_session(&node, &job);
    let store = node.store();
    let run = Run {
        id: RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: Some(session),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: Some(999_999),
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("run row");

    let mut supervisor = node.supervisor();
    let report = supervisor.startup().expect("startup");
    assert_eq!(report.orphaned_runs, 1);
    let finished = store.require_run(&run.id).expect("run");
    assert_eq!(finished.outcome, Some(RunOutcome::Crash));
    assert!(finished.ended_at.is_some());

    let tick = supervisor.tick().expect("tick");
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.continues, 1);
    assert!(
        tick.transitions
            .iter()
            .any(|(id, from, to)| id == &job.id && *from == JobState::Running && *to == JobState::Running),
        "harvest should apply RunEndedNoReport: {:?}",
        tick.transitions
    );
}

#[test]
fn dispatch_respects_ownership_locks_and_slots() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("dispatched"),
    ));
    let node = TestNode::with_adapter_config(false, adapter, |config| {
        config.node.max_workers = 2;
    });
    let a = node.insert_job("job-a", JobKind::Build, vowp_protocol::Role::Worker, Some(vec![]));
    let b = node.insert_job("job-b", JobKind::Build, vowp_protocol::Role::Worker, Some(vec![]));
    let c = node.insert_job("job-c", JobKind::Build, vowp_protocol::Role::Worker, Some(vec![]));
    node.store()
        .patch_job(
            &c.id,
            &vowp_protocol::JobPatch {
                owns: Some(vec!["docs/**".to_owned()]),
                ..vowp_protocol::JobPatch::default()
            },
        )
        .expect("patch owns");
    let mut supervisor = node.supervisor();
    let first = supervisor.tick().expect("tick");
    assert_eq!(first.steps[0].name, "requests");
    let store = node.store();
    let running = store
        .list_jobs(&JobFilter {
            states: vec![JobState::Running],
            ..JobFilter::default()
        })
        .expect("running");
    assert_eq!(running.len(), 2, "two free slots, but only one src/** holder");
    let running_ids = running.iter().map(|job| job.id.clone()).collect::<Vec<_>>();
    assert!(running_ids.contains(&a.id), "job-a acquires the src/** lock first");
    assert!(running_ids.contains(&c.id), "job-c owns a disjoint path");
    assert!(
        !running_ids.contains(&b.id),
        "job-b overlaps job-a's src/** and must stay ready"
    );
    let b_now = store.require_job(&b.id).expect("job-b");
    assert_eq!(b_now.state, JobState::Ready);
    assert_eq!(store.runs_of_job(&a.id).expect("a runs").len(), 1);
    assert_eq!(store.runs_of_job(&c.id).expect("c runs").len(), 1);
    assert!(store.runs_of_job(&b.id).expect("b runs").is_empty());
}

#[test]
fn dispatch_skips_limited_provider_and_uses_next_rung() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("fallback"),
    ));
    let node = TestNode::with_adapter_config(false, adapter, |config| {
        let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude");
        let opencode = Executor::parse("opencode:deepseek/deepseek-flash").expect("opencode");
        let profile = config.profiles.get_mut("worker").expect("worker profile");
        profile.ladder = vec![claude, opencode];
    });
    let store = node.store();
    store
        .upsert_limit(&Limit {
            provider: "claude".to_owned(),
            harness: Some(vowp_protocol::HarnessKind::ClaudeCode),
            kind: LimitRowKind::Quota,
            resets_at: Some(now().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: now(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");
    let job = worker_job(&node, "rung-job");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick");
    assert!(report.dispatched.contains(&job.id));
    let run = store.last_run_of_job(&job.id).expect("run").expect("some run");
    assert_eq!(run.rung, 1);
    assert_eq!(run.provider.as_deref(), Some("deepseek"));
}

#[test]
fn dispatch_resumes_job_with_prior_run_after_retry() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("resumed"),
    ));
    let node = TestNode::with_adapter(true, adapter.clone());
    let job = worker_job(&node, "retry-job");
    let session = create_session(&node, &job);
    let store = node.store();
    let opencode = Executor::parse("opencode:deepseek/deepseek-flash").expect("executor");
    store
        .upsert_native(
            &session,
            vowp_protocol::HarnessKind::Opencode,
            "native-1",
            &node.deps.node,
            1,
        )
        .expect("native");
    let prior = Run {
        id: RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: Some(session),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(opencode.clone()),
        provider: Some("deepseek".to_owned()),
        node: node.deps.node.clone(),
        native_session_id: Some("native-1".to_owned()),
        pid: None,
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&prior).expect("prior run");
    store
        .finish_run(
            &prior.id,
            &vowp_store::RunFinish {
                outcome: RunOutcome::Ok,
                exit_code: Some(0),
                limit_kind: None,
                cost: Cost::unknown(),
                native_session_id: Some("native-1".to_owned()),
                detail: "prior".to_owned(),
            },
        )
        .expect("finish prior");
    node.force_state(&job, JobState::Failed);
    push(&store, &Request::RetryJob { job: job.id.clone() }).expect("request");

    let mut supervisor = node.supervisor();
    supervisor.tick().expect("tick");
    node.wait_runs();
    let starts = adapter.starts();
    assert!(!starts.is_empty(), "a resume run must start");
    assert_eq!(starts[0].resume.as_deref(), Some("native-1"));
    let resumed = store
        .runs_of_job(&job.id)
        .expect("runs")
        .into_iter()
        .find(|run| run.started_at >= ts() && run.purpose == RunPurpose::Continue)
        .expect("continue run");
    assert_eq!(resumed.purpose, RunPurpose::Continue);
}

#[test]
fn rate_limit_delays_continue_until_resets() {
    let node = TestNode::new();
    let job = worker_job(&node, "rate-job");
    node.force_state(&job, JobState::Running);
    let session = create_session(&node, &job);
    let resets_at = now().plus(Duration::from_secs(1800));
    let store = node.store();
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(vowp_protocol::HarnessKind::Opencode),
            kind: LimitRowKind::Rate,
            resets_at: Some(resets_at),
            detail: "rate".to_owned(),
            since: now(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");
    let run = Run {
        id: RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: Some(session),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("run");
    store
        .finish_run(
            &run.id,
            &vowp_store::RunFinish {
                outcome: RunOutcome::Rate,
                exit_code: Some(1),
                limit_kind: Some(vowp_protocol::LimitKind::Rate),
                cost: Cost::unknown(),
                native_session_id: None,
                detail: "429".to_owned(),
            },
        )
        .expect("finish");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("tick");
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Running);
    assert!(job.note.contains("rate-limited; retry after"));
    assert_eq!(
        store.kv_get(&format!("delay:{}", job.id)).expect("kv"),
        Some(resets_at.as_rfc3339())
    );
    assert_eq!(store.runs_of_job(&job.id).expect("runs").len(), 1);
}

#[test]
fn stall_judge_triggers_stop_and_continue() {
    let deps_slot: Arc<OnceLock<vowp_core::deps::Deps>> = Arc::new(OnceLock::new());
    let starts = Arc::new(AtomicU32::new(0));
    let script: ScriptFn = {
        let deps_slot = Arc::clone(&deps_slot);
        let starts = Arc::clone(&starts);
        Arc::new(move |_input: &vowp_protocol::StartInput| {
            if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                let deps = deps_slot.get().cloned();
                let job = vowp_protocol::JobId::parse("stall-job").expect("job id");
                // Keep the run alive until the stall transition has been applied. A fixed
                // delay races a loaded runner: the run can finish before the test's next tick.
                let hold = ScriptHold::new().release_when(move || {
                    let Some(deps) = &deps else {
                        return false;
                    };
                    let Ok(store) = deps.open_store() else {
                        return false;
                    };
                    store
                        .job(&job)
                        .ok()
                        .flatten()
                        .is_some_and(|job| job.state != JobState::Running || job.continues >= 1)
                });
                return ScriptAction::ok("working").with_hold(hold);
            }
            ScriptAction::ok("working")
        })
    };
    let adapter = Arc::new(ScriptAdapter::new(vowp_protocol::HarnessKind::Opencode, script));
    let node = TestNode::with_adapter(false, adapter);
    assert!(deps_slot.set(node.deps.clone()).is_ok(), "deps slot already set");
    let job = worker_job(&node, "stall-job");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("dispatch tick");
    let store = node.store();
    let run = store.last_run_of_job(&job.id).expect("run").expect("open run");
    std::fs::write(node.deps.paths.run_raw_path(&run.id), b"some output\n").expect("output");
    node.advance(21 * 60);
    let report = supervisor.tick().expect("stall tick");
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.continues, 1);
    assert!(
        report
            .transitions
            .iter()
            .any(|(id, from, to)| id == &job.id && *from == JobState::Running && *to == JobState::Running),
        "stall should continue the job: {:?}",
        report.transitions
    );
    assert!(report.steps.iter().any(|step| step.name == "stalls" && step.ok));
}

#[test]
fn hang_before_first_output_restart_up_to_3() {
    let deps_slot: Arc<OnceLock<vowp_core::deps::Deps>> = Arc::new(OnceLock::new());
    let starts = Arc::new(AtomicU32::new(0));
    let script: ScriptFn = {
        let deps_slot = Arc::clone(&deps_slot);
        let starts = Arc::clone(&starts);
        Arc::new(move |_input: &vowp_protocol::StartInput| {
            let restart = starts.fetch_add(1, Ordering::SeqCst) + 1;
            let deps = deps_slot.get().cloned();
            let job = vowp_protocol::JobId::parse("hang-job").expect("job id");
            // Each silent run stays open until its own hang restart has been recorded (or the
            // job reaches Stalled), instead of hoping a wall-clock delay outlives the test.
            let hold = ScriptHold::new().release_when(move || {
                let Some(deps) = &deps else {
                    return false;
                };
                let Ok(store) = deps.open_store() else {
                    return false;
                };
                store
                    .job(&job)
                    .ok()
                    .flatten()
                    .is_some_and(|job| job.state == JobState::Stalled || job.hang_restarts >= restart)
            });
            ScriptAction::ok("silent").with_hold(hold)
        })
    };
    let adapter = Arc::new(ScriptAdapter::new(vowp_protocol::HarnessKind::Opencode, script));
    let node = TestNode::with_adapter(false, adapter);
    assert!(deps_slot.set(node.deps.clone()).is_ok(), "deps slot already set");
    let job = worker_job(&node, "hang-job");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("dispatch");
    let store = node.store();
    for expected in 1..=3 {
        node.advance(9 * 60);
        supervisor.tick().expect("hang tick");
        let current = store.require_job(&job.id).expect("job");
        assert_eq!(current.hang_restarts, expected);
        assert_eq!(current.state, JobState::Running);
    }
    node.advance(9 * 60);
    supervisor.tick().expect("final hang tick");
    let current = store.require_job(&job.id).expect("job");
    assert_eq!(current.state, JobState::Stalled);
    assert_eq!(current.hang_restarts, 3);
    assert_eq!(store.kv_get("wake:keeper").expect("kv"), Some("1".to_owned()));
}

#[test]
fn continue_twice_then_keeper() {
    // The supervisor bounds its run-row handoff at five seconds. Under extreme scheduler
    // starvation that bound can expire; the supervisor then records the continue without a
    // third run row. Retry the scripted scenario only for that logged transient, so the
    // assertions below still require the real crash -> continue -> continue -> stalled path.
    for attempt in 1..=3 {
        let adapter = Arc::new(ScriptAdapter::always(
            vowp_protocol::HarnessKind::Opencode,
            ScriptAction::crash("no report"),
        ));
        let node = TestNode::with_adapter(false, adapter);
        let job = worker_job(&node, "continue-job");
        let mut supervisor = node.supervisor();
        for _ in 0..4 {
            node.tick_settled(&mut supervisor);
        }
        let handoff_timed_out = node.log.contains("timed out waiting for run of continue-job");
        let store = node.store();
        let job = store.require_job(&job.id).expect("job");
        let runs = store.runs_of_job(&job.id).expect("runs");
        if handoff_timed_out && attempt < 3 {
            continue;
        }
        assert_eq!(job.continues, 2);
        assert_eq!(job.state, JobState::Stalled);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].purpose, RunPurpose::Start);
        assert!(runs[1..].iter().all(|run| run.purpose == RunPurpose::Continue));
        assert_eq!(store.kv_get("wake:keeper").expect("kv"), Some("1".to_owned()));
        return;
    }
    unreachable!("continue scenario retried without returning");
}

/// Builds a script that writes the expected report for whichever job is starting.
fn report_script(original: &str, verdict: Option<&'static str>, status: &'static str) -> h::ScriptFn {
    let original = original.to_owned();
    Arc::new(move |input: &vowp_protocol::StartInput| {
        if let Some(verify_job) = verify_prompt_job(&input.prompt) {
            let branch = format!("job/{original}");
            let file = format!(".vowp/jobs/{verify_job}/report.md");
            let text = report_markdown(
                &verify_job,
                &branch,
                "done",
                verdict,
                if verdict == Some("fail") {
                    &["concrete finding"][..]
                } else {
                    &[][..]
                },
            );
            return ScriptAction::ok("verified").with_file(file, text);
        }
        if let Some(round) = fix_round(&input.prompt) {
            let fix = format!("{original}-fix-{round}");
            let branch = format!("job/{original}");
            let file = format!(".vowp/jobs/{fix}/report.md");
            return ScriptAction::ok("fixed").with_file(file, report_markdown(&fix, &branch, status, None, &[]));
        }
        let branch = format!("job/{original}");
        let file = format!(".vowp/jobs/{original}/report.md");
        ScriptAction::ok("worked").with_file(file, report_markdown(&original, &branch, status, None, &[]))
    })
}

fn reviewing_job(node: &TestNode, id: &str) -> Job {
    let job = node.insert_job(
        id,
        JobKind::Build,
        vowp_protocol::Role::Worker,
        Some(vec![
            Gate::Report(vowp_protocol::ReportGate::default()),
            Gate::Review(vowp_protocol::ReviewGate::default()),
        ]),
    );
    let worktree = node.worktree_with_file(&job, "src/lib.rs", "pub fn a() {}\n");
    let text = report_markdown(id, &format!("job/{id}"), "done", None, &[]);
    let report = worktree.join(&job.report);
    std::fs::create_dir_all(report.parent().expect("parent")).expect("report dir");
    std::fs::write(&report, text).expect("report");
    node.force_state(&job, JobState::Reviewing);
    node.store().require_job(&job.id).expect("job")
}

#[test]
fn verify_job_goes_done_via_gating_never_merged_and_original_receives_verdict() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        report_script("verify-pass", Some("pass"), "done"),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let original = reviewing_job(&node, "verify-pass");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 40, |store| {
        matches!(
            store.require_job(&original.id).expect("original").state,
            JobState::Merging | JobState::Done
        )
    });
    let store = node.store();
    let verify = store
        .require_job(&vowp_protocol::JobId::parse(&verify_id("verify-pass", 0)).expect("id"))
        .expect("verify job");
    assert_eq!(verify.state, JobState::Done);
    assert_eq!(verify.kind, JobKind::Verify);
    let state_rows = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::State),
            job_id: Some(verify.id.clone()),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("state rows");
    assert!(
        state_rows
            .iter()
            .all(|entry| entry.detail.get("to") != Some(&serde_json::Value::String("merging".to_owned()))),
        "a verify job must never enter merging"
    );
    let merge_rows = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Merge),
            job_id: Some(verify.id.clone()),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("merge rows");
    assert!(merge_rows.is_empty(), "a verify job is never merged");
    let results = store.gate_results_for(&original.id, 0).expect("gate results");
    let review = results
        .iter()
        .find(|result| matches!(result.gate, Gate::Review(_)))
        .expect("review result");
    assert!(review.passed);
    assert!(matches!(
        store.require_job(&original.id).expect("original").state,
        JobState::Merging | JobState::Done
    ));
}

#[test]
fn verify_prompt_contains_original_gate_command_and_tail() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        report_script("verify-evidence", Some("pass"), "done"),
    ));
    let node = TestNode::with_adapter(true, adapter.clone());
    let original = reviewing_job(&node, "verify-evidence");
    node.store()
        .insert_gate_result(&vowp_protocol::GateResult {
            id: None,
            job_id: original.id.clone(),
            run_id: None,
            attempt: 0,
            gate_index: 0,
            gate: Gate::Cmd(vowp_protocol::CmdGate {
                run: "cargo test --workspace".to_owned(),
                ..vowp_protocol::CmdGate::default()
            }),
            passed: false,
            evidence: vowp_protocol::Evidence::Cmd {
                command: "cargo test --workspace".to_owned(),
                exit_code: Some(101),
                timed_out: false,
                tail: "EARLY-TAIL-LINE\ncompiling the workspace\nTAIL-MARKER-42".to_owned(),
                duration_ms: 10,
            },
            output_blob: None,
            duration_ms: 10,
            ts: now(),
        })
        .expect("gate result");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 40, |store| {
        store
            .job(&vowp_protocol::JobId::parse(&verify_id("verify-evidence", 0)).expect("id"))
            .expect("verify")
            .is_some_and(|job| job.state == JobState::Done)
    });
    let starts = adapter.starts();
    let prompt = starts
        .iter()
        .map(|start| start.prompt.as_str())
        .find(|prompt| prompt.contains("You are the verifier for job"))
        .expect("verifier prompt");
    assert!(prompt.contains("cargo test --workspace"), "{prompt}");
    assert!(prompt.contains("EARLY-TAIL-LINE"), "{prompt}");
    assert!(prompt.contains("TAIL-MARKER-42"), "{prompt}");
    assert!(prompt.contains("git diff main...job/verify-evidence"), "{prompt}");
}

#[test]
fn verify_no_verdict_is_retried_once_then_accepts_a_valid_report() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if let Some(verify_job) = verify_prompt_job(&input.prompt) {
                let verdict = if verify_job.ends_with("-r1") {
                    Some("pass")
                } else {
                    None
                };
                return ScriptAction::ok("verdict attempt").with_file(
                    format!(".vowp/jobs/{verify_job}/report.md"),
                    report_markdown(&verify_job, "job/verify-retry", "done", verdict, &[]),
                );
            }
            ScriptAction::ok("nothing")
        }),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let original = reviewing_job(&node, "verify-retry");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store.require_job(&original.id).expect("original").state == JobState::Merging
    });
    let store = node.store();
    let retry = store
        .require_job(&vowp_protocol::JobId::parse(&format!("{}-r1", verify_id("verify-retry", 0))).expect("id"))
        .expect("retry verify job");
    assert_eq!(retry.state, JobState::Done);
    let review = store
        .gate_results_for(&original.id, 0)
        .expect("gate results")
        .into_iter()
        .find(|result| matches!(result.gate, Gate::Review(_)))
        .expect("review result");
    assert!(review.passed);
    assert_ne!(
        store.require_job(&original.id).expect("original").state,
        JobState::Parked
    );
}

#[test]
fn verify_no_verdict_twice_parks_original_with_question() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if let Some(verify_job) = verify_prompt_job(&input.prompt) {
                return ScriptAction::ok("no verdict").with_file(
                    format!(".vowp/jobs/{verify_job}/report.md"),
                    report_markdown(&verify_job, "job/verify-no-verdict", "done", None, &[]),
                );
            }
            ScriptAction::ok("nothing")
        }),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let original = reviewing_job(&node, "verify-no-verdict");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store.require_job(&original.id).expect("original").state == JobState::Parked
    });
    let store = node.store();
    let job = store.require_job(&original.id).expect("job");
    assert_eq!(job.parked_reason, Some(ParkedReason::Question));
    let inbox = store.list_inbox(None).expect("inbox");
    assert!(inbox.iter().any(|item| {
        item.job_id.as_ref() == Some(&job.id)
            && item
                .body
                .contains(&format!("verifier could not produce a verdict for {}", job.id))
    }));
}

#[test]
fn verify_fail_creates_fix_job_same_branch_max_3_then_escalates() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        report_script("verify-fail", Some("fail"), "done"),
    ));
    let node = TestNode::with_adapter(true, adapter.clone());
    let original = reviewing_job(&node, "verify-fail");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store.require_job(&original.id).expect("original").state == JobState::Failed
    });
    let store = node.store();
    let job = store.require_job(&original.id).expect("job");
    assert_eq!(job.attempts, 3);
    let fix_jobs = store
        .list_jobs(&JobFilter {
            kind: Some(JobKind::Fix),
            ..JobFilter::default()
        })
        .expect("fix jobs");
    assert_eq!(fix_jobs.len(), 3);
    for fix in &fix_jobs {
        assert_eq!(fix.branch, job.branch);
        assert_eq!(fix.worktree, job.worktree);
        assert!(
            store
                .edges_of(&fix.id)
                .expect("edges")
                .iter()
                .any(|edge| edge.kind == EdgeKind::Fixes && edge.dst == job.id)
        );
    }
    let fix_prompts = adapter
        .starts()
        .into_iter()
        .filter(|start| start.prompt.contains("Job verify-fail failed"))
        .collect::<Vec<_>>();
    assert!(!fix_prompts.is_empty());
    assert!(
        fix_prompts
            .iter()
            .all(|start| start.prompt.contains("concrete finding")),
        "verifier findings must reach every fix prompt: {fix_prompts:?}"
    );
    let decision = store
        .require_job(&vowp_protocol::JobId::parse("verify-fail-decision").expect("id"))
        .expect("decision job");
    assert_eq!(decision.kind, JobKind::Decision);
    let inbox = store.list_inbox(None).expect("inbox");
    assert!(inbox.iter().any(|item| item.kind == vowp_protocol::InboxKind::Decision));
}

#[test]
fn failed_fix_job_gates_fail_original_not_fix_of_fix() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        report_script("fix-fail", Some("fail"), "partial"),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let original = reviewing_job(&node, "fix-fail");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 40, |store| {
        store.require_job(&original.id).expect("original").state == JobState::Failed
    });
    let store = node.store();
    let fix_jobs = store
        .list_jobs(&JobFilter {
            kind: Some(JobKind::Fix),
            ..JobFilter::default()
        })
        .expect("fix jobs");
    assert_eq!(fix_jobs.len(), 1);
    assert!(
        !fix_jobs[0].id.as_str().contains("-fix-1-fix-"),
        "a failed fix must not create a fix-of-fix"
    );
}

#[test]
fn failed_verify_job_parks_original_with_question() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        report_script("verify-blocked", None, "partial"),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let original = reviewing_job(&node, "verify-blocked");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 30, |store| {
        store.require_job(&original.id).expect("original").state == JobState::Parked
    });
    let store = node.store();
    let job = store.require_job(&original.id).expect("job");
    assert_eq!(job.parked_reason, Some(ParkedReason::Question));
    let inbox = store.list_inbox(None).expect("inbox");
    assert!(
        inbox
            .iter()
            .any(|item| item.kind == vowp_protocol::InboxKind::Question && item.job_id.as_ref() == Some(&job.id))
    );
}

#[test]
fn paused_loop_is_skipped() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("paused"),
    ));
    let node = TestNode::with_adapter(true, adapter);
    let mut supervisor = node.supervisor();
    let store = node.store();
    let dispatch_job = worker_job(&node, "paused-dispatch");
    let review_job = reviewing_job(&node, "paused-review");
    let template = vowp_protocol::NewJob {
        id: vowp_protocol::JobId::parse("sched-job").expect("id"),
        title: "scheduled".to_owned(),
        kind: JobKind::Build,
        role: vowp_protocol::Role::Worker,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: Some(Vec::new()),
        priority: None,
        loop_: None,
        brief_text: None,
    };
    store
        .insert_schedule(&vowp_protocol::Schedule {
            id: vowp_protocol::ScheduleId::new(),
            project: Some(node.project.clone()),
            name: "paused-schedule".to_owned(),
            spec_kind: vowp_protocol::ScheduleSpecKind::Interval,
            spec: "every 30m".to_owned(),
            tz: "UTC".to_owned(),
            action: vowp_protocol::ScheduleAction::JobTemplate { template },
            missed_policy: vowp_protocol::MissedPolicy::RunOnce,
            state: vowp_protocol::ActiveState::Active,
            last_run_at: None,
            next_run_at: Some(now()),
            last_status: String::new(),
            created_at: now(),
        })
        .expect("schedule");
    for name in ["l2-worker", "l3-verify", "l9-schedules"] {
        store
            .set_loop_state(name, vowp_protocol::ActiveState::Paused)
            .expect("pause loop");
    }
    let report = supervisor.tick().expect("paused tick");
    assert!(report.dispatched.is_empty());
    assert_ne!(
        store.require_job(&dispatch_job.id).expect("job").state,
        JobState::Running
    );
    assert!(
        store
            .job(&vowp_protocol::JobId::parse(&verify_id("paused-review", 0)).expect("id"))
            .expect("verify")
            .is_none(),
        "paused l3-verify must not create a verify job"
    );
    assert_eq!(
        store.require_job(&review_job.id).expect("review").state,
        JobState::Reviewing
    );
    let scheduled = store
        .list_jobs(&JobFilter {
            id_prefix: Some("sched-job".to_owned()),
            ..JobFilter::default()
        })
        .expect("scheduled jobs");
    assert!(scheduled.is_empty(), "paused l9-schedules must not fire");
    assert!(report.steps.iter().any(|step| step.name == "heartbeat" && step.ok));
}

#[test]
fn global_pause_skips_dispatch() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("paused"),
    ));
    let node = TestNode::with_adapter(false, adapter);
    let job = worker_job(&node, "paused-job");
    let store = node.store();
    store.kv_set("paused", "1").expect("paused");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("paused tick");
    assert!(report.dispatched.is_empty());
    assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Pending);
    assert!(report.steps.iter().any(|step| step.name == "heartbeat" && step.ok));
    assert!(!report.steps.iter().any(|step| step.name == "dispatch"));
}

#[test]
fn tick_requests_apply_pause_resume_run_and_wake_with_ledger_state_rows() {
    let node = TestNode::new();
    let store = node.store();
    push(
        &store,
        &Request::PauseLoop {
            name: "l2-worker".to_owned(),
        },
    )
    .expect("push");
    push(
        &store,
        &Request::RunLoop {
            name: "l3-verify".to_owned(),
        },
    )
    .expect("push");
    push(&store, &Request::Wake).expect("push");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("tick");
    assert_eq!(
        store.loop_("l2-worker").expect("loop").expect("row").state,
        vowp_protocol::ActiveState::Paused
    );
    assert!(
        store
            .loop_("l3-verify")
            .expect("loop")
            .expect("row")
            .next_run_at
            .is_some()
    );
    let ledger = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::State),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    assert!(ledger.iter().any(|entry| entry.summary.contains("pause_loop")));
    assert!(ledger.iter().any(|entry| entry.summary.contains("run_loop")));
    assert!(ledger.iter().any(|entry| entry.summary.contains("wake")));
    assert!(store.take_requests().expect("requests").is_empty());
}

#[test]
fn requests_drained_and_applied() {
    let node = TestNode::new();
    let store = node.store();
    let job = worker_job(&node, "drain-job");
    store
        .set_job_state(
            &job.id,
            JobState::Parked,
            &vowp_store::StateChange {
                note: None,
                parked_reason: Some(ParkedReason::Manual),
            },
        )
        .expect("parked");
    let id = push(&store, &Request::ResolveJob { job: job.id.clone() }).expect("push");
    store.kv_set("paused", "1").expect("paused");
    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick");
    assert!(report.steps[0].ok);
    assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Ready);
    assert!(store.take_requests().expect("queue").is_empty());
    let ledger = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::State),
            job_id: Some(job.id.clone()),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    assert!(ledger.iter().any(|entry| {
        entry.detail.get("why") == Some(&serde_json::Value::String("Resolved".to_owned()))
            && entry.detail.get("from") == Some(&serde_json::Value::String("parked".to_owned()))
            && entry.detail.get("to") == Some(&serde_json::Value::String("ready".to_owned()))
    }));
    let _ = id;
}

#[test]
fn request_rows_apply_every_kind_end_to_end() {
    let release = Arc::new(AtomicBool::new(false));
    let script: ScriptFn = {
        let release = Arc::clone(&release);
        Arc::new(move |_input: &vowp_protocol::StartInput| {
            let release = Arc::clone(&release);
            // Runs must stay open through the whole tick, but their liveness is controlled by
            // the test rather than by a sleep that load can stretch past.
            ScriptAction::ok("slow run")
                .with_hold(ScriptHold::new().release_when(move || release.load(Ordering::SeqCst)))
        })
    };
    let adapter = Arc::new(ScriptAdapter::new(vowp_protocol::HarnessKind::Opencode, script));
    let node = TestNode::with_adapter(false, adapter);
    let mut supervisor = node.supervisor();
    let store = node.store();
    store
        .set_loop_state("l2-worker", vowp_protocol::ActiveState::Paused)
        .expect("pause l2");

    let retry_failed = worker_job(&node, "req-retry");
    node.force_state(&retry_failed, JobState::Failed);
    let restart_stalled = worker_job(&node, "req-restart");
    node.force_state(&restart_stalled, JobState::Stalled);
    let resolve_parked = worker_job(&node, "req-resolve");
    node.force_state(&resolve_parked, JobState::Parked);
    let cancel_pending = worker_job(&node, "req-cancel");
    let nudge_stalled = worker_job(&node, "req-nudge");
    node.force_state(&nudge_stalled, JobState::Stalled);
    let park_ready = worker_job(&node, "req-park");
    node.force_state(&park_ready, JobState::Ready);
    let replan_parked = worker_job(&node, "req-replan");
    node.force_state(&replan_parked, JobState::Parked);
    let fail_stalled = worker_job(&node, "req-fail");
    node.force_state(&fail_stalled, JobState::Stalled);
    let answer_parked = worker_job(&node, "req-answer");
    node.force_state(&answer_parked, JobState::Parked);

    let inbox_id = vowp_protocol::InboxId::new();
    store
        .insert_inbox(&vowp_protocol::InboxItem {
            id: inbox_id.clone(),
            project: Some(node.project.clone()),
            job_id: Some(answer_parked.id.clone()),
            kind: vowp_protocol::InboxKind::Question,
            title: "answer me".to_owned(),
            body: "?".to_owned(),
            options: vec!["resolved".to_owned()],
            state: vowp_protocol::InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now(),
            answered_at: None,
        })
        .expect("inbox");
    store.answer_inbox(&inbox_id, "resolved", "test").expect("answer");

    let requests = [
        Request::RetryJob {
            job: retry_failed.id.clone(),
        },
        Request::RestartJob {
            job: restart_stalled.id.clone(),
        },
        Request::ResolveJob {
            job: resolve_parked.id.clone(),
        },
        Request::CancelJob {
            job: cancel_pending.id.clone(),
        },
        Request::NudgeJob {
            job: nudge_stalled.id.clone(),
        },
        Request::ParkJob {
            job: park_ready.id.clone(),
            reason: "needs a human".to_owned(),
        },
        Request::ReplanJob {
            job: replan_parked.id.clone(),
        },
        Request::FailJob {
            job: fail_stalled.id.clone(),
            reason: "gave up".to_owned(),
        },
        Request::AnswerInbox { id: inbox_id },
        Request::PauseLoop {
            name: "l7-inbox".to_owned(),
        },
        Request::ResumeLoop {
            name: "l7-inbox".to_owned(),
        },
        Request::RunLoop {
            name: "l3-verify".to_owned(),
        },
        Request::Wake,
    ];
    for request in &requests {
        push(&store, request).expect("push request");
    }
    supervisor.tick().expect("tick");
    release.store(true, Ordering::SeqCst);
    node.wait_runs();

    assert_eq!(store.require_job(&retry_failed.id).expect("job").state, JobState::Ready);
    assert_eq!(
        store.require_job(&restart_stalled.id).expect("job").state,
        JobState::Running
    );
    assert_eq!(
        store.require_job(&resolve_parked.id).expect("job").state,
        JobState::Ready
    );
    assert_eq!(
        store.require_job(&cancel_pending.id).expect("job").state,
        JobState::Cancelled
    );
    assert_eq!(
        store.require_job(&nudge_stalled.id).expect("job").state,
        JobState::Running
    );
    let parked = store.require_job(&park_ready.id).expect("job");
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(parked.parked_reason, Some(ParkedReason::Manual));
    assert_eq!(parked.note, "needs a human");
    assert_eq!(
        store.require_job(&replan_parked.id).expect("job").state,
        JobState::Replanned
    );
    assert_eq!(
        store.require_job(&fail_stalled.id).expect("job").state,
        JobState::Failed
    );
    assert_eq!(
        store.require_job(&answer_parked.id).expect("job").state,
        JobState::Ready
    );
    assert_eq!(
        store.loop_("l7-inbox").expect("loop").expect("row").state,
        vowp_protocol::ActiveState::Active
    );
    assert_eq!(
        store.loop_("l3-verify").expect("loop").expect("row").next_run_at,
        Some(now())
    );
    let restart_runs = store.runs_of_job(&restart_stalled.id).expect("runs");
    assert!(
        restart_runs.iter().any(|run| run.purpose == RunPurpose::Start),
        "restart must start a fresh run, not only continue: {restart_runs:?}"
    );
    assert!(
        restart_runs
            .iter()
            .all(|run| matches!(run.purpose, RunPurpose::Start | RunPurpose::Continue)),
        "restart must not turn into a fix or verify run: {restart_runs:?}"
    );
    let nudge_runs = store.runs_of_job(&nudge_stalled.id).expect("runs");
    assert!(
        !nudge_runs.is_empty() && nudge_runs.iter().all(|run| run.purpose == RunPurpose::Continue),
        "nudge must continue the job, never start it fresh: {nudge_runs:?}"
    );
    let ledger = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::State),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    for why in ["pause_loop", "resume_loop", "run_loop", "wake"] {
        assert!(
            ledger.iter().any(|entry| entry.summary.contains(why)),
            "missing {why} ledger row"
        );
    }
    assert!(store.take_requests().expect("queue").is_empty());
}

#[test]
fn dispatch_brief_precedence_copies_project_then_keeps_worktree_then_falls_back() {
    // 1. The planner's project-checkout brief reaches the worktree byte-for-byte.
    let node = TestNode::new();
    let job = worker_job(&node, "brief-project");
    let project_brief = node.main_repo.join(&job.brief);
    std::fs::create_dir_all(project_brief.parent().expect("project brief dir")).expect("project brief dir");
    let planned = vowp_protocol::Brief::from_job(&job, "# Planner task\n\nImplement the real thing.\n").render();
    std::fs::write(&project_brief, &planned).expect("project brief");
    let mut supervisor = node.supervisor();
    let tick = supervisor.tick().expect("tick");
    assert!(
        tick.dispatched.contains(&job.id),
        "job was not dispatched: {:?}",
        tick.dispatched
    );
    node.wait_runs();
    let dispatched = node.store().require_job(&job.id).expect("job");
    let worktree = PathBuf::from(dispatched.worktree.as_ref().expect("worktree"));
    let worktree_brief = worktree.join(&dispatched.brief);
    assert_eq!(
        std::fs::read(&worktree_brief).expect("worktree brief"),
        std::fs::read(&project_brief).expect("project brief"),
        "the worktree brief must be a byte-for-byte copy of the project brief"
    );
    assert!(
        std::fs::read_to_string(&worktree_brief)
            .expect("brief")
            .contains("Implement the real thing."),
        "the planner's body must reach the worker"
    );

    // 2. An existing worktree brief wins over a different project copy.
    let node = TestNode::new();
    let job = worker_job(&node, "brief-existing");
    let worktree = node.tmp.path().join("worktrees").join(job.id.as_str());
    let worktree_text = worktree.to_string_lossy().into_owned();
    let existing = vowp_protocol::Brief::from_job(&job, "# Worktree task\n\nKeep this worktree copy.\n").render();
    let worktree_brief = worktree.join(&job.brief);
    std::fs::create_dir_all(worktree_brief.parent().expect("worktree brief dir")).expect("worktree brief dir");
    std::fs::write(&worktree_brief, &existing).expect("worktree brief");
    let project_brief = node.main_repo.join(&job.brief);
    std::fs::create_dir_all(project_brief.parent().expect("project brief dir")).expect("project brief dir");
    std::fs::write(
        &project_brief,
        vowp_protocol::Brief::from_job(&job, "# Project task\n\nDo not use this copy.\n").render(),
    )
    .expect("project brief");
    node.store()
        .set_job_workspace(&job.id, Some("job/brief-existing"), Some(&worktree_text))
        .expect("workspace");
    let mut supervisor = node.supervisor();
    let tick = supervisor.tick().expect("tick");
    assert!(
        tick.dispatched.contains(&job.id),
        "job was not dispatched: {:?}",
        tick.dispatched
    );
    node.wait_runs();
    assert_eq!(
        std::fs::read_to_string(&worktree_brief).expect("worktree brief"),
        existing,
        "an existing worktree brief is never overwritten"
    );

    // 3. With neither source, the generated fallback is written.
    let node = TestNode::new();
    let job = worker_job(&node, "brief-fallback");
    let mut supervisor = node.supervisor();
    let tick = supervisor.tick().expect("tick");
    assert!(
        tick.dispatched.contains(&job.id),
        "job was not dispatched: {:?}",
        tick.dispatched
    );
    node.wait_runs();
    let dispatched = node.store().require_job(&job.id).expect("job");
    let worktree = PathBuf::from(dispatched.worktree.as_ref().expect("worktree"));
    let fallback_body = format!(
        "Do job {job}. Read AGENTS.md first, change only the paths in `owns`, commit on the job branch, run the gates, and write the report at {report}.\n",
        job = dispatched.id,
        report = dispatched.report,
    );
    let expected = vowp_protocol::Brief::from_job(&dispatched, &fallback_body).render();
    assert_eq!(
        std::fs::read_to_string(worktree.join(&dispatched.brief)).expect("fallback brief"),
        expected,
        "with neither source, the generated fallback must be written"
    );
}
