//! Core drills: crash/continue, hang restart, merge conflict/lockfile, gate fix loop,
//! orphan reconcile and the secret-scan push block.
//! Spec: `docs/design/loops.md` §13.

#[path = "loops_support.rs"]
mod h;

use std::sync::Arc;
use std::time::Duration;

use h::*;
use vowp_protocol::{
    Cost, Gate, JobKind, JobLoop, JobState, LedgerKind, OnExhausted, Role, Run, RunId, RunOutcome, RunPurpose, RunRole,
};
use vowp_store::{JobFilter, LedgerFilter};

/// Returns the job id from an L2 start prompt.
fn start_prompt_job(prompt: &str) -> Option<String> {
    let start = prompt.find("for job ")? + "for job ".len();
    let rest = &prompt[start..];
    let end = rest.find(" of project")?;
    Some(rest[..end].to_owned())
}

/// Returns the parent id from a fix prompt.
fn fix_prompt_parent(prompt: &str) -> Option<String> {
    let rest = prompt.strip_prefix("Job ")?;
    let end = rest.find(" failed")?;
    Some(rest[..end].to_owned())
}

#[test]
fn drill_crash_continue() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::crash("killed after one event"),
    ));
    let node = TestNode::with_adapter(false, adapter);
    let job = node.insert_job("crash-job", JobKind::Build, Role::Worker, Some(vec![]));
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("dispatch");
    node.wait_runs();
    let store = node.store();
    let first = store.last_run_of_job(&job.id).expect("run").expect("first run");
    assert_eq!(first.outcome, Some(RunOutcome::Crash));

    node.tick_settled(&mut supervisor);
    let after_first = store.require_job(&job.id).expect("job");
    assert_eq!(after_first.state, JobState::Running);
    assert_eq!(after_first.continues, 1);
    let runs = store.runs_of_job(&job.id).expect("runs");
    assert_eq!(runs.last().expect("last").purpose, RunPurpose::Continue);

    node.tick_settled(&mut supervisor);
    node.tick_settled(&mut supervisor);
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Stalled);
    assert_eq!(store.kv_get("wake:keeper").expect("kv"), Some("1".to_owned()));
}

#[test]
fn drill_stall_hang() {
    let adapter = Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("silent").with_delay(Duration::from_secs(3)),
    ));
    let node = TestNode::with_adapter(false, adapter);
    let job = node.insert_job("hang-drill", JobKind::Build, Role::Worker, Some(vec![]));
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("dispatch");
    let store = node.store();
    node.advance(9 * 60);
    supervisor.tick().expect("stall");
    let after_one = store.require_job(&job.id).expect("job");
    assert_eq!(after_one.hang_restarts, 1);
    assert_eq!(after_one.state, JobState::Running);
    let runs = store.runs_of_job(&job.id).expect("runs");
    let starts = runs.iter().filter(|run| run.purpose == RunPurpose::Start).count();
    assert!(starts >= 2, "a hung run is restarted with a fresh Start");

    for _ in 0..3 {
        node.advance(9 * 60);
        supervisor.tick().expect("stall");
    }
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Stalled);
    assert_eq!(job.hang_restarts, 3);
}

#[test]
fn drill_merge_conflict() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            let Some(round) = fix_round(&input.prompt) else {
                return ScriptAction::ok("nothing to do");
            };
            let merge_id = format!("second-merge-{round}");
            let script = "git fetch origin && (git rebase origin/main || true) && \
                          printf 'pub fn value() -> u8 { 3 }\\n' > src/shared.rs && \
                          git add src/shared.rs && GIT_EDITOR=true git rebase --continue"
                .to_owned();
            ScriptAction::ok("resolved")
                .with_command(["sh", "-c", &script])
                .with_file(
                    format!(".vowp/jobs/{merge_id}/report.md"),
                    report_markdown(&merge_id, "job/second", "done", None, &[]),
                )
        }),
    ));
    let node = TestNode::with_adapter(true, adapter.clone());
    let first = node.insert_job("first", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&first, "src/shared.rs", "pub fn value() -> u8 { 1 }\n");
    node.force_state(&first, JobState::Merging);
    let second = node.insert_job("second", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&second, "src/shared.rs", "pub fn value() -> u8 { 2 }\n");
    node.force_state(&second, JobState::Merging);

    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&second.id).expect("second").state == JobState::Done
    });
    let store = node.store();
    let merge_jobs = store
        .list_jobs(&JobFilter {
            kind: Some(JobKind::Merge),
            ..JobFilter::default()
        })
        .expect("merge jobs");
    assert_eq!(merge_jobs.len(), 1);
    assert!(
        merge_jobs[0].owns.iter().any(|own| own == "src/shared.rs"),
        "the merge child owns the conflicted file: {:?}",
        merge_jobs[0].owns
    );
    let merge_prompts = adapter
        .starts()
        .into_iter()
        .filter(|start| start.prompt.contains("Job second failed"))
        .collect::<Vec<_>>();
    assert!(!merge_prompts.is_empty());
    assert!(
        merge_prompts
            .iter()
            .any(|start| start.prompt.contains("rebase conflict in src/shared.rs")),
        "the merge prompt must list the conflicted file: {merge_prompts:?}"
    );
    let origin = node.tmp.path().join("origin.git");
    let shown = node.git(&origin, &["show", "main:src/shared.rs"]);
    assert!(
        shown.contains("{ 3 }"),
        "the resolved conflict must reach main: {shown}"
    );
    assert_eq!(store.require_job(&first.id).expect("first").state, JobState::Done);
}

#[test]
fn drill_merge_lockfile() {
    let node = TestNode::new_real_git();
    node.commit_file(
        &node.main_repo.clone(),
        "Cargo.toml",
        "[package]\nname = \"lockdrill\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    node.commit_file(&node.main_repo.clone(), "src/lib.rs", "pub fn lock() {}\n");
    node.git(&node.main_repo.clone(), &["push", "origin", "main"]);
    let first = node.insert_job("lock-a", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&first, "Cargo.lock", "version = 3\n\"a\" = 1\n");
    node.force_state(&first, JobState::Merging);
    let second = node.insert_job("lock-b", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&second, "Cargo.lock", "version = 3\n\"b\" = 2\n");
    node.force_state(&second, JobState::Merging);

    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&second.id).expect("second").state == JobState::Done
    });
    let store = node.store();
    let merge_jobs = store
        .list_jobs(&JobFilter {
            kind: Some(JobKind::Merge),
            ..JobFilter::default()
        })
        .expect("merge jobs");
    assert!(merge_jobs.is_empty());
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Merge),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let row = ledger
        .iter()
        .find(|entry| entry.job_id.as_ref() == Some(&second.id))
        .expect("second merge row");
    assert_eq!(
        row.detail.get("lockfile_refreshed"),
        Some(&serde_json::Value::Bool(true))
    );
    let origin = node.tmp.path().join("origin.git");
    let subject = node.git(&origin, &["log", "-1", "--format=%s", "main"]);
    assert_eq!(subject.trim(), "chore: refresh lockfile (merge queue)");
}

#[test]
fn drill_gate_fail_fix_loop() {
    let adapter = Arc::new(ScriptAdapter::new(
        vowp_protocol::HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if let Some(verify_job) = verify_prompt_job(&input.prompt) {
                return ScriptAction::ok("verified").with_file(
                    format!(".vowp/jobs/{verify_job}/report.md"),
                    report_markdown(&verify_job, "job/gate-job", "done", Some("pass"), &[]),
                );
            }
            if let Some(parent) = fix_prompt_parent(&input.prompt) {
                let round = fix_round(&input.prompt).unwrap_or(1);
                let id = format!("{parent}-fix-{round}");
                let mut action = ScriptAction::ok("fix round").with_file(
                    format!(".vowp/jobs/{id}/report.md"),
                    report_markdown(&id, &format!("job/{parent}"), "done", None, &[]),
                );
                if round >= 2 {
                    action = action.with_file("state.txt", "ready\n");
                }
                return action;
            }
            let id = start_prompt_job(&input.prompt).unwrap_or_else(|| "gate-job".to_owned());
            ScriptAction::ok("worked").with_file(
                format!(".vowp/jobs/{id}/report.md"),
                report_markdown(&id, &format!("job/{id}"), "done", None, &[]),
            )
        }),
    ));
    let node = TestNode::with_adapter(true, adapter.clone());
    let job = node.insert_job(
        "gate-job",
        JobKind::Build,
        Role::Worker,
        Some(vec![
            Gate::Report(vowp_protocol::ReportGate::default()),
            Gate::Cmd(vowp_protocol::CmdGate {
                run: "echo gate-output-marker >&2; grep -q '^ready$' state.txt".to_owned(),
                ..vowp_protocol::CmdGate::default()
            }),
            Gate::Review(vowp_protocol::ReviewGate::default()),
        ]),
    );
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 60, |store| {
        store.require_job(&job.id).expect("job").state == JobState::Done
    });
    let store = node.store();
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.attempts, 2);
    let fixes = store
        .list_jobs(&JobFilter {
            kind: Some(JobKind::Fix),
            ..JobFilter::default()
        })
        .expect("fix jobs");
    assert_eq!(fixes.len(), 2);
    let fix_prompts = adapter
        .starts()
        .into_iter()
        .filter(|start| start.prompt.contains("Job gate-job failed"))
        .collect::<Vec<_>>();
    assert!(!fix_prompts.is_empty());
    assert!(fix_prompts.iter().any(|start| {
        start.prompt.contains("grep -q '^ready$' state.txt") && start.prompt.contains("gate-output-marker")
    }));

    // A job with zero allowed rounds escalates through T18 instead.
    let exhausted = node.insert_job(
        "exhaust-job",
        JobKind::Build,
        Role::Worker,
        Some(vec![
            Gate::Report(vowp_protocol::ReportGate::default()),
            Gate::Cmd(vowp_protocol::CmdGate {
                run: "echo gate-output-marker >&2; grep -q '^ready$' state.txt".to_owned(),
                ..vowp_protocol::CmdGate::default()
            }),
        ]),
    );
    store
        .patch_job(
            &exhausted.id,
            &vowp_protocol::JobPatch {
                loop_: Some(JobLoop {
                    until: Default::default(),
                    max_attempts: 0,
                    on_exhausted: OnExhausted::Escalate,
                }),
                ..vowp_protocol::JobPatch::default()
            },
        )
        .expect("patch loop");
    node.tick_until(&mut supervisor, 30, |store| {
        store.require_job(&exhausted.id).expect("job").state == JobState::Failed
    });
    assert!(
        store
            .job(&vowp_protocol::JobId::parse("exhaust-job-decision").expect("id"))
            .expect("decision")
            .is_some()
    );
    assert!(
        store
            .list_inbox(None)
            .expect("inbox")
            .iter()
            .any(|item| item.kind == vowp_protocol::InboxKind::Decision)
    );
}

#[test]
fn drill_orphan_reconcile() {
    let node = TestNode::new();
    let job = node.insert_job("orphan-drill", JobKind::Build, Role::Worker, Some(vec![]));
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
        executor: Some(vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: Some(999_998),
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: ts(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("run");
    let mut supervisor = node.supervisor();
    let report = supervisor.startup().expect("startup");
    assert_eq!(report.orphaned_runs, 1);
    assert_eq!(
        store.require_run(&run.id).expect("run").outcome,
        Some(RunOutcome::Crash)
    );
    supervisor.tick().expect("tick");
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.continues, 1);
}

#[test]
fn drill_secret_scan_blocks_push() {
    let node = TestNode::new_real_git();
    let token = format!("{}{}", "ghp_", "0123456789abcdefghijklmnopqrstuvwxyz");
    let job = node.insert_job("secret-job", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&job, "src/config.rs", &format!("const TOKEN: &str = \"{token}\";\n"));
    node.force_state(&job, JobState::Merging);
    let before = node.git(&node.tmp.path().join("origin.git"), &["rev-parse", "main"]);

    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&job.id).expect("job").state == JobState::Fixing
    });
    let store = node.store();
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Fixing);
    let after = node.git(&node.tmp.path().join("origin.git"), &["rev-parse", "main"]);
    assert_eq!(before, after, "nothing may be pushed");

    let results = store.gate_results_for(&job.id, job.attempts).expect("gate results");
    // Merge checks are recorded with gate_index -1 on the pre-merge attempt.
    let merge_attempt = job.attempts.saturating_sub(1);
    let mut results_all = store.gate_results_for(&job.id, merge_attempt).expect("gate results");
    results_all.extend(results);
    let serialised = serde_json::to_string(&results_all).expect("json");
    assert!(serialised.contains("github_token"), "evidence records the rule name");
    assert!(!serialised.contains(&token), "the matched secret text is never stored");
    let ledger = store
        .ledger_since(&LedgerFilter {
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let ledger_text = serde_json::to_string(&ledger).expect("json");
    assert!(
        !ledger_text.contains(&token),
        "the ledger never stores the matched text"
    );
}
