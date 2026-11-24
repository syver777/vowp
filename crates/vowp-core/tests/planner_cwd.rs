//! Planner/keeper cwd recovery tests.
//! Spec: `docs/design/loops.md` §7 (planner/keeper cwd and session recovery).
//!
//! The incident these tests pin: a symlinked worktree root whose child was deleted made
//! `check_cwd` report the child as outside the allow-list, and L4 retried the same stale
//! role session every tick. A missing leaf inside the root must resolve as `cwd_missing`,
//! and L1 must retire the stale role session once so a fresh session starts the turn.

#[path = "failover_support.rs"]
mod f;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use f::*;
use vowp_core::CoreError;
use vowp_core::session::service::SessionService;
use vowp_protocol::{
    CancelToken, Executor, ExecutorRef, HarnessKind, LedgerKind, NewSession, RunPurpose, RunRole, SessionId,
    SessionRole, SessionState, TurnRequest,
};
use vowp_store::LedgerFilter;

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor")
}

fn create_role_session(node: &TestNode, role: SessionRole, cwd: PathBuf) -> SessionId {
    SessionService::new(node.deps.clone())
        .create_session(NewSession {
            project: Some(node.project.clone()),
            job_id: None,
            role,
            title: format!("{role:?} cwd test"),
            cwd,
            parent: None,
            executor: Some(opencode()),
        })
        .expect("create session")
}

fn turn(session: &SessionId) -> TurnRequest {
    TurnRequest {
        session_id: session.clone(),
        executor: ExecutorRef {
            executor: opencode(),
            node: None,
        },
        purpose: RunPurpose::Plan,
        role: RunRole::Planner,
        job_id: None,
        prompt: "You are the planner. Plan the work.".to_owned(),
        profile: Some("planner".to_owned()),
        rung: 0,
        window: None,
        timeout: None,
    }
}

/// Returns the planner run that is currently open.
fn open_planner_run(node: &TestNode) -> vowp_protocol::Run {
    node.store()
        .open_runs()
        .expect("open runs")
        .into_iter()
        .find(|run| run.role == RunRole::Planner)
        .expect("an open planner run")
}

/// Counts ledger notes that name `session`.
fn retirement_notes(node: &TestNode, session: &SessionId) -> usize {
    node.store()
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            ..LedgerFilter::default()
        })
        .expect("ledger")
        .iter()
        .filter(|entry| entry.session_id.as_ref() == Some(session) && entry.summary.contains(session.as_str()))
        .count()
}

/// Ticks until `hold` records a start.
///
/// The previous turn's run row can close just before its session thread marks itself
/// finished, so one tick is not enough to prove the planner is due again. Ticking until the
/// adapter records the start keeps the test free of that thread-reaping race.
fn tick_until_started(node: &TestNode, supervisor: &mut vowp_core::loops::Supervisor, hold: &ScriptHold) -> bool {
    for _ in 0..200 {
        supervisor.tick().expect("tick");
        if hold.wait_started(Duration::from_millis(100)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    node.deps.log.log(
        vowp_protocol::Level::Error,
        "test",
        "planner turn did not start within 200 ticks",
    );
    false
}

/// Creates a planner session in `stale_cwd`, then deletes that leaf so only its parent exists.
fn stale_planner(node: &TestNode, stale_cwd: &Path) -> SessionId {
    std::fs::create_dir_all(stale_cwd).expect("stale worktree");
    let session = create_role_session(node, SessionRole::Planner, stale_cwd.to_path_buf());
    std::fs::remove_dir_all(stale_cwd).expect("delete worktree");
    assert!(!stale_cwd.exists(), "the recorded cwd is gone");
    session
}

#[test]
fn deleted_child_of_symlinked_worktree_root_reports_cwd_missing() {
    let node = TestNode::new();
    let root = node.tmp.path().join("worktrees");
    let link = node.tmp.path().join("worktrees-link");
    std::os::unix::fs::symlink(&root, &link).expect("symlink to the worktree root");
    let cwd = link.join("deleted-job");
    std::fs::create_dir_all(&cwd).expect("worktree");
    let session = create_role_session(&node, SessionRole::Planner, cwd.clone());
    std::fs::remove_dir_all(&cwd).expect("delete worktree");

    // The leaf is gone but its deepest existing ancestor canonicalizes to the allowed root,
    // so the refusal is the actionable `cwd_missing`, not "outside the allow-list".
    let error = SessionService::new(node.deps.clone())
        .run_turn(turn(&session), &CancelToken::default())
        .expect_err("a missing leaf inside the root is not a runnable cwd");
    assert!(matches!(error, CoreError::CwdMissing(_)), "got {error:?}");
    assert!(error.to_string().contains("cwd_missing"), "{error}");
    assert!(
        error.to_string().contains(&cwd.display().to_string()),
        "the error names the missing directory: {error}"
    );
    assert!(node.adapter.starts().is_empty(), "no adapter is started");
    assert!(node.store().open_runs().expect("open runs").is_empty());
}

#[test]
fn symlink_escape_out_of_the_worktree_root_is_refused() {
    let node = TestNode::new();
    let root = node.tmp.path().join("worktrees");
    let outside = node.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside directory");
    let escape = root.join("escape");
    std::os::unix::fs::symlink(&outside, &escape).expect("symlink out of the root");

    for cwd in [escape.clone(), escape.join("missing")] {
        let session = create_role_session(&node, SessionRole::Planner, cwd);
        let error = SessionService::new(node.deps.clone())
            .run_turn(turn(&session), &CancelToken::default())
            .expect_err("a symlink out of the root must be refused");
        assert!(matches!(error, CoreError::Refused(_)), "got {error:?}");
        assert!(error.to_string().contains("outside the allow-list"), "{error}");
    }
    assert!(node.adapter.starts().is_empty(), "no adapter is started");
}

#[test]
fn symlinked_worktree_root_with_deleted_child_starts_planner_turn() {
    let node = TestNode::new();
    let root = node.tmp.path().join("worktrees");
    let link = node.tmp.path().join("worktrees-link");
    std::os::unix::fs::symlink(&root, &link).expect("symlink to the worktree root");

    // The recorded cwd reaches the allowed root through the symlink, but its leaf is gone.
    let stale = stale_planner(&node, &link.join("deleted-job"));

    let store = node.store();
    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("planner tick");

    assert!(
        wait_until(Duration::from_secs(10), || {
            node.adapter
                .starts()
                .iter()
                .any(|start| is_planner_prompt(&start.prompt))
        }),
        "the planner turn starts despite the deleted child of the symlinked root"
    );
    node.wait_runs();

    let start = node
        .adapter
        .starts()
        .into_iter()
        .find(|start| is_planner_prompt(&start.prompt))
        .expect("planner start");
    assert_eq!(
        start.cwd, node.main_repo,
        "the fresh session runs in the project checkout"
    );
    let stale_row = store.session(&stale).expect("session").expect("stale session row");
    assert_eq!(
        stale_row.state,
        SessionState::Closed,
        "the stale session is retired, not retried"
    );
}

#[test]
fn traversal_outside_the_worktree_root_is_refused() {
    let node = TestNode::new();
    let root = node.tmp.path().join("worktrees");
    let outside = node.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside directory");

    // A real traversal out of the root: every component exists, so it canonicalizes
    // outside the allow-list and must be refused.
    let existing = create_role_session(&node, SessionRole::Planner, root.join("..").join("outside"));
    let error = SessionService::new(node.deps.clone())
        .run_turn(turn(&existing), &CancelToken::default())
        .expect_err("a traversal out of the worktree root must be refused");
    assert!(matches!(error, CoreError::Refused(_)), "got {error:?}");
    assert!(error.to_string().contains("outside the allow-list"), "{error}");

    // A traversal through a missing component cannot be resolved against an existing
    // ancestor; it is refused rather than guessed.
    let missing = create_role_session(
        &node,
        SessionRole::Planner,
        root.join("ghost").join("..").join("outside"),
    );
    let error = SessionService::new(node.deps.clone())
        .run_turn(turn(&missing), &CancelToken::default())
        .expect_err("an unresolved traversal must be refused");
    assert!(matches!(error, CoreError::Refused(_)), "got {error:?}");
    assert!(error.to_string().contains("outside the allow-list"), "{error}");

    assert!(
        node.store().open_runs().expect("open runs").is_empty(),
        "a refused cwd never creates a run"
    );
    assert!(node.adapter.starts().is_empty(), "no adapter is started");
}

#[test]
fn stale_role_session_is_retired_once_and_not_retried() {
    let first = ScriptHold::new();
    let second = ScriptHold::new();
    let queue = script_queue(vec![
        ScriptAction::ok("planned").with_hold(first.clone()),
        ScriptAction::ok("planned").with_hold(second.clone()),
    ]);
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(move |_| next_action(&queue)),
    ));
    let node = two_adapter_node(
        false,
        opencode.clone(),
        Arc::new(ScriptAdapter::always(HarnessKind::ClaudeCode, ScriptAction::ok("ok"))),
        |_| {},
    );
    let stale = stale_planner(&node, &node.tmp.path().join("worktrees").join("gone-job"));
    let store = node.store();

    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first planner tick");
    assert!(first.wait_started(Duration::from_secs(10)), "first turn starts");
    first.release();
    node.wait_runs();
    assert_eq!(retirement_notes(&node, &stale), 1, "one note names the stale session");

    // The next due turn reuses the fresh session, so the stale one is not retired again.
    node.advance(5 * 60);
    store.kv_set("wake:planner", "1").expect("wake");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    assert!(
        tick_until_started(&node, &mut supervisor, &second),
        "second turn starts"
    );
    second.release();
    node.wait_runs();
    assert_eq!(
        retirement_notes(&node, &stale),
        1,
        "the closed session is skipped on later ticks"
    );
    assert_eq!(
        store.session(&stale).expect("session").expect("row").state,
        SessionState::Closed
    );
}

#[test]
fn fresh_session_is_used_on_the_next_tick() {
    let first = ScriptHold::new();
    let second = ScriptHold::new();
    let queue = script_queue(vec![
        ScriptAction::ok("planned").with_hold(first.clone()),
        ScriptAction::ok("planned").with_hold(second.clone()),
    ]);
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(move |_| next_action(&queue)),
    ));
    let node = two_adapter_node(
        false,
        opencode.clone(),
        Arc::new(ScriptAdapter::always(HarnessKind::ClaudeCode, ScriptAction::ok("ok"))),
        |_| {},
    );
    let stale = stale_planner(&node, &node.tmp.path().join("worktrees").join("gone-job"));
    let store = node.store();

    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first planner tick");
    assert!(first.wait_started(Duration::from_secs(10)), "first turn starts");
    let fresh = open_planner_run(&node)
        .session_id
        .expect("the first planner run has a session");
    assert_ne!(fresh, stale, "the stale session is not reused");
    first.release();
    node.wait_runs();

    node.advance(5 * 60);
    store.kv_set("wake:planner", "1").expect("wake");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    assert!(
        tick_until_started(&node, &mut supervisor, &second),
        "second turn starts"
    );
    let next = open_planner_run(&node)
        .session_id
        .expect("the second planner run has a session");
    assert_eq!(next, fresh, "the fresh session runs the next tick too");
    second.release();
    node.wait_runs();
}
