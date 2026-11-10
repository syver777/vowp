//! L4/L5 planner and keeper tests, plus L8 board/heartbeat and the repeat guard.
//! Spec: `docs/design/loops.md` §7, §10, §12.

#[path = "failover_support.rs"]
mod f;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use f::*;
use vowp_protocol::{
    Gate, HarnessKind, InboxKind, JobKind, JobLoop, JobState, LedgerKind, LimitKind, OnExhausted, Role, RunRole,
    SwitchReason,
};
use vowp_store::LedgerFilter;

fn ok_adapter(kind: HarnessKind) -> ScriptAdapter {
    ScriptAdapter::always(kind, ScriptAction::ok("ok"))
}

#[test]
fn planner_one_turn_at_a_time() {
    let hold = ScriptHold::new();
    let planner_hold = hold.clone();
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(move |input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("planner handoff");
            }
            if is_planner_prompt(&input.prompt) {
                return ScriptAction::ok("planned").with_hold(planner_hold.clone());
            }
            ScriptAction::ok("ok")
        }),
    ));
    let node = two_adapter_node(
        false,
        opencode.clone(),
        Arc::new(ok_adapter(HarnessKind::ClaudeCode)),
        |_| {},
    );
    let store = node.store();
    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first tick");
    assert!(
        hold.wait_started(Duration::from_secs(10)),
        "the planner adapter records its start"
    );
    assert!(
        store
            .open_runs()
            .map(|runs| runs.iter().any(|run| run.role == RunRole::Planner))
            .unwrap_or(false),
        "the planner run is still open while the adapter is held"
    );
    supervisor.tick().expect("second tick while the planner turn is held");
    let planner_starts = opencode
        .starts()
        .into_iter()
        .filter(|start| is_planner_prompt(&start.prompt))
        .count();
    assert_eq!(planner_starts, 1, "never two planner turns at once");

    hold.release();
    node.wait_runs();
    // The same fixed clock is inside the 5-minute minimum gap: no second turn.
    store.kv_set("wake:planner", "1").expect("wake again");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    supervisor.tick().expect("third tick");
    assert_eq!(
        opencode
            .starts()
            .into_iter()
            .filter(|start| is_planner_prompt(&start.prompt))
            .count(),
        1,
        "the minimum gap blocks a tight second turn"
    );
    // After the gap the next unseen event starts exactly one more turn.
    node.advance(5 * 60);
    store.kv_set("wake:planner", "1").expect("wake after gap");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    let mut restarted = false;
    for _ in 0..600 {
        supervisor.tick().expect("fourth tick");
        std::thread::sleep(Duration::from_millis(10));
        if opencode
            .starts()
            .into_iter()
            .filter(|start| is_planner_prompt(&start.prompt))
            .count()
            == 2
        {
            restarted = true;
            break;
        }
    }
    assert!(restarted, "the next unseen event starts a second planner turn");
    node.wait_runs();
}

#[test]
fn keeper_covers_planner_when_provider_limited_and_hands_back() {
    let back = Arc::new(AtomicBool::new(false));
    let back_for_claude = Arc::clone(&back);
    let claude = Arc::new(ScriptAdapter::new(
        HarnessKind::ClaudeCode,
        Arc::new(move |input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("claude handoff");
            }
            if is_probe_prompt(&input.prompt) {
                return ScriptAction::ok("ok");
            }
            if is_planner_prompt(&input.prompt) {
                if back_for_claude.load(Ordering::SeqCst) {
                    return ScriptAction::ok("planned again");
                }
                return ScriptAction {
                    summary: Some(limit_summary(
                        LimitKind::Quota,
                        "claude",
                        Some(ts().plus(Duration::from_secs(3600))),
                    )),
                    ..ScriptAction::default()
                };
            }
            ScriptAction::ok("ok")
        }),
    ));
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("keeper handoff");
            }
            ScriptAction::ok("kept")
        }),
    ));
    let node = two_adapter_node(false, opencode.clone(), claude.clone(), |config| {
        set_ladders(
            config,
            &["claude-code:claude-sonnet-5-5@medium"],
            &["opencode:deepseek/deepseek-flash#max"],
        );
    });
    let store = node.store();
    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("planner tick");
    assert!(
        wait_until(Duration::from_secs(10), || {
            store.limit_for("claude").map(|limit| limit.is_some()).unwrap_or(false)
        }),
        "the planner run records the quota row"
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            role_session(&node, vowp_protocol::SessionRole::Planner)
                .map(|session| session.state == vowp_protocol::SessionState::Blocked)
                .unwrap_or(false)
        }),
        "the limited planner turn finishes blocked"
    );
    node.wait_runs();
    let limit = store.limit_for("claude").expect("limit").expect("quota row");
    assert_eq!(limit.kind, vowp_protocol::LimitRowKind::Quota);
    assert_eq!(limit.resets_at, Some(ts().plus(Duration::from_secs(3600 + 120))));

    // The planner has no available rung: the keeper covers with planner duties.
    node.advance(5 * 60);
    store.kv_set("wake:planner", "1").expect("wake");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    let mut covering = None;
    for _ in 0..200 {
        supervisor.tick().expect("covering tick");
        std::thread::sleep(Duration::from_millis(10));
        covering = opencode
            .starts()
            .into_iter()
            .find(|start| is_keeper_prompt(&start.prompt) && start.prompt.contains("Planner coverage"));
        if covering.is_some() {
            break;
        }
    }
    let covering = covering.expect("the keeper covers the limited planner");
    node.wait_runs();
    let keeper_starts = opencode
        .starts()
        .into_iter()
        .filter(|start| is_keeper_prompt(&start.prompt))
        .collect::<Vec<_>>();
    assert_eq!(keeper_starts.len(), 1, "one covering keeper turn");
    assert!(covering.prompt.contains("Planner coverage"));
    assert!(covering.prompt.contains("Planner digest"));

    // The claude probe succeeds; the planner session switches back and plans again.
    back.store(true, Ordering::SeqCst);
    node.advance(3600 + 121);
    let mut probed = false;
    for _ in 0..600 {
        node.wait_runs();
        supervisor.tick().expect("probe tick");
        if store.limit_for("claude").expect("limit").is_none()
            && completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner).contains(&SwitchReason::ProbeBack)
        {
            probed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(probed, "the probe clears the quota row");
    assert!(
        completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner).contains(&SwitchReason::ProbeBack),
        "the planner session is switched back to rung 0"
    );
    store.kv_set("wake:planner", "1").expect("wake after probe");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    let mut resumed = false;
    for _ in 0..600 {
        supervisor.tick().expect("handback tick");
        std::thread::sleep(Duration::from_millis(10));
        let planner_on_claude = claude
            .starts()
            .into_iter()
            .filter(|start| is_planner_prompt(&start.prompt))
            .count();
        if planner_on_claude >= 2 {
            resumed = true;
            break;
        }
    }
    assert!(resumed, "the planner resumes after hand-back");
}

#[test]
fn limited_planner_reports_coverage_while_a_turn_finishes() {
    let node = two_adapter_node(
        false,
        Arc::new(ok_adapter(HarnessKind::Opencode)),
        Arc::new(ok_adapter(HarnessKind::ClaudeCode)),
        |config| {
            set_ladders(
                config,
                &["claude-code:claude-sonnet-5-5@medium"],
                &["opencode:deepseek/deepseek-flash#max"],
            );
        },
    );
    let store = node.store();
    store
        .upsert_limit(&vowp_protocol::Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: vowp_protocol::LimitRowKind::Quota,
            resets_at: Some(ts().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");
    store.kv_set("wake:planner", "1").expect("wake");
    let status = vowp_core::loops::l4_planner::planner_status(&node.deps, &store, ts(), true).expect("planner status");
    assert!(
        status.provider_limited,
        "the provider is limited even though the previous planner turn is still finishing"
    );
    assert!(
        !status.wanted,
        "the planner is not started again while its turn is open"
    );
}

#[test]
fn board_and_heartbeat_written_each_tick() {
    let node = TestNode::new();
    let job = node.insert_job("board-job", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Pending);
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("tick");
    let board_path = node.deps.paths.root().join("board.json");
    let board: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&board_path).expect("board file")).expect("board json");
    assert_eq!(board["project"], "test");
    assert!(board["jobs_by_state"].is_object());
    assert!(board["slots_total"].as_u64().unwrap_or(0) >= 1);
    let heartbeat = std::fs::read_to_string(node.deps.paths.root().join("heartbeat")).expect("heartbeat");
    assert_eq!(heartbeat, ts().unix_secs().to_string());
    assert_eq!(
        vowp_core::loops::l8::heartbeat_age(&node.deps.paths, ts()),
        Some(Duration::ZERO)
    );
    assert!(node.store().kv_get("supervisor_pid").expect("kv").is_some());

    node.advance(60);
    supervisor.tick().expect("second tick");
    let heartbeat = std::fs::read_to_string(node.deps.paths.root().join("heartbeat")).expect("heartbeat");
    assert_eq!(heartbeat, ts().plus(Duration::from_secs(60)).unix_secs().to_string());
    assert!(vowp_core::loops::l8::SYSTEMD_UNIT.contains("ExecStart=%h/.local/bin/vowp up --foreground"));
    assert!(vowp_core::loops::l8::CRON_CHECK_SH.contains("systemctl --user restart vowp.service"));
    assert!(vowp_core::loops::l8::CRON_CHECK_SH.contains("watchdog.log"));
}

#[test]
fn repeat_guard_trips_on_third_identical_action() {
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if let Some(parent) = fix_parent(&input.prompt) {
                let round = fix_round(&input.prompt).unwrap_or(1);
                let id = format!("{parent}-fix-{round}");
                return ScriptAction::ok("fixed").with_file(
                    format!(".vowp/jobs/{id}/report.md"),
                    report_markdown(&id, &format!("job/{parent}"), "done", None, &[]),
                );
            }
            ScriptAction::ok("worker")
        }),
    ));
    let node = two_adapter_node(true, opencode, Arc::new(ok_adapter(HarnessKind::ClaudeCode)), |_| {});
    let job = node.insert_job(
        "repeat-job",
        JobKind::Build,
        Role::Worker,
        Some(vec![Gate::Cmd(vowp_protocol::CmdGate {
            run: "echo identical-failure >&2; exit 1".to_owned(),
            ..vowp_protocol::CmdGate::default()
        })]),
    );
    node.worktree_with_file(&job, "src/repeat.rs", "pub fn repeat() {}\n");
    node.force_state(&job, JobState::Gating);
    node.store()
        .patch_job(
            &job.id,
            &vowp_protocol::JobPatch {
                loop_: Some(JobLoop {
                    until: Default::default(),
                    max_attempts: 5,
                    on_exhausted: OnExhausted::Escalate,
                }),
                ..vowp_protocol::JobPatch::default()
            },
        )
        .expect("loop");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store.require_job(&job.id).expect("job").state == JobState::Failed
    });
    let store = node.store();
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.attempts, 2, "the guard trips before max_attempts");
    assert!(job.note.contains("no progress: gate x3"), "note: {}", job.note);
    assert!(
        store
            .job(&vowp_protocol::JobId::parse("repeat-job-decision").expect("id"))
            .expect("decision")
            .is_some()
    );
    assert!(
        store
            .list_inbox(None)
            .expect("inbox")
            .iter()
            .any(|item| item.kind == InboxKind::Decision)
    );
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::State),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert!(ledger.iter().any(|entry| entry.summary.contains("repeat guard")));
    let l1 = store.loop_("l1-supervisor").expect("loop").expect("row");
    assert!(l1.last_status.contains("repeat guard"), "status: {}", l1.last_status);
}

#[test]
fn reconcile_leaves_live_session_run_open() {
    let hold = ScriptHold::new();
    let planner_hold = hold.clone();
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(move |input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("planner handoff");
            }
            if is_planner_prompt(&input.prompt) {
                return ScriptAction::ok("planned").with_hold(planner_hold.clone());
            }
            ScriptAction::ok("ok")
        }),
    ));
    let node = two_adapter_node(false, opencode, Arc::new(ok_adapter(HarnessKind::ClaudeCode)), |_| {});
    let store = node.store();
    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first tick");
    assert!(
        hold.wait_started(Duration::from_secs(10)),
        "the planner adapter records its start"
    );
    let run_id = store
        .open_runs()
        .expect("open")
        .into_iter()
        .find(|run| run.role == RunRole::Planner)
        .expect("planner run")
        .id;
    supervisor.tick().expect("tick while the planner is live");
    assert_eq!(
        store.require_run(&run_id).expect("run").ended_at,
        None,
        "reconcile must not orphan a live planner turn"
    );
    hold.release();
    node.wait_runs();
}

#[test]
fn reconcile_leaves_live_probe_run_open() {
    let hold = ScriptHold::new();
    let probe_hold = hold.clone();
    let opencode = Arc::new(ScriptAdapter::always(HarnessKind::Opencode, ScriptAction::ok("unused")));
    let claude = Arc::new(ScriptAdapter::new(
        HarnessKind::ClaudeCode,
        Arc::new(move |input: &vowp_protocol::StartInput| {
            if is_probe_prompt(&input.prompt) {
                return ScriptAction::ok("ok").with_hold(probe_hold.clone());
            }
            ScriptAction::ok("ok")
        }),
    ));
    let node = two_adapter_node(false, opencode, claude, |config| {
        set_ladders(
            config,
            &["claude-code:claude-sonnet-5-5@medium"],
            &["opencode:deepseek/deepseek-flash#max"],
        );
    });
    let store = node.store();
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l4-planner", vowp_protocol::ActiveState::Paused)
        .expect("pause planner");
    store
        .set_loop_state("l5-keeper", vowp_protocol::ActiveState::Paused)
        .expect("pause keeper");
    store
        .upsert_limit(&vowp_protocol::Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: vowp_protocol::LimitRowKind::Quota,
            resets_at: Some(ts().minus(Duration::from_secs(1))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");
    store.kv_set("probe_back:claude", "1").expect("probe back");
    store.kv_set("limit:claude", "quota").expect("provider");
    supervisor.tick().expect("probe tick");
    assert!(
        hold.wait_started(Duration::from_secs(10)),
        "the probe adapter records its start"
    );
    let run_id = store
        .open_runs()
        .expect("open")
        .into_iter()
        .find(|run| run.role == RunRole::Probe)
        .expect("probe run")
        .id;
    supervisor.tick().expect("tick while the probe is live");
    assert_eq!(
        store.require_run(&run_id).expect("run").ended_at,
        None,
        "reconcile must not orphan a live probe"
    );
    hold.release();
    node.wait_runs();
}

#[test]
fn slow_gate_child_brief_stays_in_temp_repo() {
    let node = TestNode::new();
    let job = node.insert_job(
        "slow-gate",
        JobKind::Build,
        Role::Worker,
        Some(vec![Gate::Cmd(vowp_protocol::CmdGate {
            run: "echo gate-failed >&2; exit 1".to_owned(),
            ..vowp_protocol::CmdGate::default()
        })]),
    );
    node.force_state(&job, JobState::Gating);
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store
            .job(&vowp_protocol::JobId::parse("slow-gate-fix-1").expect("child id"))
            .map(|child| child.is_some())
            .unwrap_or(false)
    });
    let brief = node.main_repo.join(".vowp/jobs/slow-gate-fix-1/brief.md");
    assert!(
        brief.is_file(),
        "the child brief is written under the temp project repo: {}",
        brief.display()
    );
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !manifest.join(".vowp").exists(),
        "the source tree must not receive .vowp files"
    );
}
