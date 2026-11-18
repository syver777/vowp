//! Merge-queue tests against a real bare origin and clone.
//! Spec: `docs/design/crates/vowp-core.md` §7 and `docs/design/loops.md` §6.

#[path = "loops_support.rs"]
mod h;

use h::*;
use vowp_core::loops::merge_queue::{MergeQueue, MergeStep};
use vowp_protocol::{Job, JobKind, JobState, LedgerEntry, LedgerKind, Role};
use vowp_store::LedgerFilter;

fn merging_job(node: &TestNode, id: &str) -> Job {
    let job = node.insert_job(id, JobKind::Build, Role::Worker, Some(vec![]));
    let worktree = node.worktree_with_file(&job, "src/lib.rs", &format!("pub fn {id}() {{}}\n"));
    let text = report_markdown(id, &format!("job/{id}"), "done", None, &[]);
    let report = worktree.join(&job.report);
    std::fs::create_dir_all(report.parent().expect("parent")).expect("report dir");
    std::fs::write(&report, text).expect("report");
    node.force_state(&job, JobState::Merging);
    node.store().require_job(&job.id).expect("job")
}

/// Commits `file` on the local `main` without pushing it, and returns the new head.
fn commit_local_main(node: &TestNode, file: &str, content: &str) -> String {
    node.commit_file(&node.main_repo, file, content);
    node.git(&node.main_repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Adds and pushes one commit to the bare origin's `main` from a second clone.
fn push_remote_commit(node: &TestNode, file: &str, content: &str) -> String {
    let origin = node.tmp.path().join("origin.git");
    let other = node.tmp.path().join("other");
    node.git(
        node.tmp.path(),
        &["clone", &origin.to_string_lossy(), &other.to_string_lossy()],
    );
    node.git(&other, &["config", "user.name", "test"]);
    node.git(&other, &["config", "user.email", "test@example.com"]);
    std::fs::write(other.join(file), content).expect("remote file");
    node.git(&other, &["add", "-A"]);
    node.git(&other, &["commit", "-m", "remote change"]);
    node.git(&other, &["push", "origin", "main"]);
    node.git(&other, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Returns the merge ledger rows of one job.
fn merge_ledger(node: &TestNode, job: &Job) -> Vec<LedgerEntry> {
    node.store()
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Merge),
            job_id: Some(job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger")
}

#[test]
fn merge_queue_happy_path_ff_push_to_local_bare_origin() {
    let node = TestNode::new_real_git();
    let job = merging_job(&node, "happy");
    let queue = MergeQueue::new(node.deps.clone());
    let step = queue.step(&node.store()).expect("merge step");
    assert_eq!(step, MergeStep::Merged(job.id.clone()));

    let origin = node.tmp.path().join("origin.git");
    let shown = node.git(&origin, &["show", "main:src/lib.rs"]);
    assert!(shown.contains("pub fn happy"));
    let ledger = node
        .store()
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Merge),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].job_id.as_ref(), Some(&job.id));
    assert_eq!(
        ledger[0].detail.get("lockfile_refreshed"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn merge_queue_local_main_ahead_of_origin_still_merges() {
    let node = TestNode::new_real_git();
    // The job branch is cut from main, then local main advances while the job is verified.
    // This is the real race: the branch does not contain the newer local commit.
    let job = merging_job(&node, "local-ahead");
    let local_head = commit_local_main(&node, "local.txt", "local only\n");
    let queue = MergeQueue::new(node.deps.clone());
    let step = queue.step(&node.store()).expect("merge step");
    assert_eq!(
        step,
        MergeStep::Merged(job.id.clone()),
        "the queue must merge onto the local default branch even when it is ahead of origin"
    );

    let origin = node.tmp.path().join("origin.git");
    assert_eq!(node.git(&origin, &["show", "main:local.txt"]), "local only\n");
    assert!(
        node.git(&origin, &["show", "main:src/lib.rs"])
            .contains("pub fn local-ahead"),
        "the job commit reached the origin"
    );
    // The local commit is an ancestor of the pushed origin/main, so it was pushed too.
    node.git(&origin, &["merge-base", "--is-ancestor", &local_head, "main"]);

    let ledger = merge_ledger(&node, &job);
    let row = ledger.last().expect("merge row");
    assert_eq!(
        row.detail.get("main_before"),
        Some(&serde_json::Value::String(local_head)),
        "main_before is the local base head the queue merged onto"
    );
}

#[test]
fn merge_queue_remote_ahead_fast_forwards_local_base_first() {
    let node = TestNode::new_real_git();
    let remote_head = push_remote_commit(&node, "remote.txt", "remote only\n");
    let job = merging_job(&node, "remote-ahead");
    let local_before = node.git(&node.main_repo, &["rev-parse", "main"]).trim().to_owned();
    assert_ne!(local_before, remote_head, "the local base starts behind the remote");

    let queue = MergeQueue::new(node.deps.clone());
    let step = queue.step(&node.store()).expect("merge step");
    assert_eq!(step, MergeStep::Merged(job.id.clone()));

    let local_after = node.git(&node.main_repo, &["rev-parse", "main"]).trim().to_owned();
    node.git(
        &node.main_repo,
        &["merge-base", "--is-ancestor", &remote_head, &local_after],
    );
    let origin = node.tmp.path().join("origin.git");
    assert_eq!(node.git(&origin, &["show", "main:remote.txt"]), "remote only\n");
    assert!(
        node.git(&origin, &["show", "main:src/lib.rs"])
            .contains("pub fn remote-ahead"),
        "the job commit reached the origin"
    );

    let ledger = merge_ledger(&node, &job);
    let row = ledger.last().expect("merge row");
    assert_eq!(
        row.detail.get("main_before"),
        Some(&serde_json::Value::String(remote_head)),
        "main_before is the fast-forwarded local base"
    );
}

#[test]
fn merge_queue_diverged_bases_refuse_without_moving_main() {
    let node = TestNode::new_real_git();
    let local_head = commit_local_main(&node, "local.txt", "local only\n");
    let remote_head = push_remote_commit(&node, "remote.txt", "remote only\n");
    assert_ne!(local_head, remote_head);
    let job = merging_job(&node, "diverged");
    let main_before = node.git(&node.main_repo, &["rev-parse", "main"]).trim().to_owned();
    assert_eq!(main_before, local_head);

    let queue = MergeQueue::new(node.deps.clone());
    let step = queue.step(&node.store()).expect("merge step");
    let MergeStep::Manual { job: refused, detail } = step else {
        panic!("diverged bases must be refused manually: {step:?}");
    };
    assert_eq!(refused, job.id);
    assert!(detail.contains(&local_head), "detail names the local head: {detail}");
    assert!(detail.contains(&remote_head), "detail names the remote head: {detail}");
    assert_eq!(
        node.git(&node.main_repo, &["rev-parse", "main"]).trim(),
        main_before.as_str(),
        "main must not move on divergence"
    );

    let ledger = merge_ledger(&node, &job);
    assert!(
        ledger.iter().any(|entry| entry.detail.get("failure").is_some()),
        "the refusal is recorded as a merge failure: {ledger:?}"
    );
}

#[test]
fn merge_queue_conflict_creates_merge_job() {
    let node = TestNode::new_real_git();
    let first = node.insert_job("first", JobKind::Build, Role::Worker, Some(vec![]));
    node.worktree_with_file(&first, "src/shared.rs", "pub fn value() -> u8 { 1 }\n");
    node.force_state(&first, JobState::Merging);
    let second = node.insert_job("second", JobKind::Build, Role::Worker, Some(vec![]));
    let second_tree = node.worktree_with_file(&second, "src/shared.rs", "pub fn value() -> u8 { 2 }\n");
    node.force_state(&second, JobState::Merging);

    let mut supervisor = node.supervisor();
    // The oldest merging job is merged first.
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&first.id).expect("first").state == JobState::Done
    });
    // Now the second job hits a real rebase conflict.
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&second.id).expect("second").state == JobState::Fixing
    });
    let store = node.store();
    let job = store.require_job(&second.id).expect("second");
    let merge_job = store
        .require_job(&vowp_protocol::JobId::parse("second-merge-1").expect("id"))
        .expect("merge job");
    assert_eq!(merge_job.kind, JobKind::Merge);
    assert_eq!(merge_job.worktree, Some(second_tree.to_string_lossy().into_owned()));
    assert_eq!(merge_job.branch, job.branch);
    assert!(
        store
            .edges_of(&merge_job.id)
            .expect("edges")
            .iter()
            .any(|edge| edge.kind == vowp_protocol::EdgeKind::Fixes && edge.dst == second.id)
    );
    assert!(
        merge_job.owns.iter().any(|own| own == "src/shared.rs"),
        "a merge child owns the conflicted file: {:?}",
        merge_job.owns
    );
}

#[test]
fn merge_queue_cargo_lock_regenerated_not_merged() {
    let node = TestNode::new_real_git();
    node.commit_file(
        &node.main_repo.clone(),
        "Cargo.toml",
        "[package]\nname = \"locktest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
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
    node.tick_until(&mut supervisor, 200, |store| {
        store.require_job(&second.id).expect("second").state == JobState::Done
    });
    let store = node.store();
    let job = store.require_job(&second.id).expect("second");
    assert_eq!(job.state, JobState::Done);
    let merge_jobs = store
        .list_jobs(&vowp_store::JobFilter {
            kind: Some(JobKind::Merge),
            ..vowp_store::JobFilter::default()
        })
        .expect("merge jobs");
    assert!(merge_jobs.is_empty(), "lockfile-only must not create a merge job");
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Merge),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let second_row = ledger
        .iter()
        .find(|entry| entry.job_id.as_ref() == Some(&second.id))
        .expect("second merge row");
    assert_eq!(
        second_row.detail.get("lockfile_refreshed"),
        Some(&serde_json::Value::Bool(true))
    );
    let origin = node.tmp.path().join("origin.git");
    // The queue takes the main version of a lockfile conflict (a real cargo update
    // would then re-add the branch's dependency set).
    let shown = node.git(&origin, &["show", "main:Cargo.lock"]);
    assert!(shown.contains("locktest"), "cargo regenerated the lockfile: {shown}");
    let subject = node.git(&origin, &["log", "-1", "--format=%s", "main"]);
    assert_eq!(subject.trim(), "chore: refresh lockfile (merge queue)");
}

#[test]
fn merge_queue_dirty_main_parks_job_manual() {
    let node = TestNode::new_real_git();
    let job = merging_job(&node, "dirty-main");
    std::fs::write(node.main_repo.join("dirty.txt"), "uncommitted\n").expect("dirty file");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&job.id).expect("job").state == JobState::Parked
    });
    let store = node.store();
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.parked_reason, Some(vowp_protocol::ParkedReason::Manual));
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Merge),
            job_id: Some(job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger");
    assert!(ledger.iter().any(|entry| entry.detail.get("failure").is_some()));
    assert!(
        store
            .list_inbox(None)
            .expect("inbox")
            .iter()
            .any(|item| item.job_id.as_ref() == Some(&job.id))
    );
}

#[test]
fn merge_queue_check_failure_returns_job_to_fixing() {
    let adapter = std::sync::Arc::new(ScriptAdapter::always(
        vowp_protocol::HarnessKind::Opencode,
        ScriptAction::ok("fixing checks"),
    ));
    let adapter_for_checks = adapter.clone();
    let node = TestNode::with_adapter_config(true, adapter, |config| {
        config.merge.check_cmds = vec!["false".to_owned()];
    });
    let job = merging_job(&node, "checks-fail");
    let mut supervisor = node.supervisor();
    node.tick_until(&mut supervisor, 120, |store| {
        store.require_job(&job.id).expect("job").state == JobState::Fixing
    });
    let store = node.store();
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Fixing);
    assert_eq!(job.attempts, 1);
    let fix = store
        .require_job(&vowp_protocol::JobId::parse("checks-fail-fix-1").expect("id"))
        .expect("fix job");
    assert_eq!(fix.kind, JobKind::Fix);
    let prompts = adapter_for_checks.starts();
    assert!(
        prompts
            .iter()
            .any(|start| { start.prompt.contains("Job checks-fail failed") && start.prompt.contains("false") }),
        "the fix prompt must carry the failed merge check: {prompts:?}"
    );
}
