//! Job window lifecycle: terminal transitions close the view, startup and the tick reconcile
//! leftovers, and `vowp gc` lists and collects orphan `job-<id>` windows.
//!
//! Every test uses the hermetic `FakeWindows` manager from `vowp-tmux`; no test starts tmux,
//! reads the real home or touches the network.

#[path = "loops_support.rs"]
mod h;

use h::*;
use vowp_core::lifecycle::{self, GcOptions, gc_with_window};
use vowp_core::requests::{Request, push};
use vowp_protocol::{CatalogClass, Job, JobKind, JobState, Role};
use vowp_tmux::{LayoutSpec, WindowManager, WindowSpec, session_name_for_project};

fn sleep_spec() -> WindowSpec {
    WindowSpec {
        argv: vec!["sleep".to_owned(), "30".to_owned()],
        ..WindowSpec::default()
    }
}

fn layout() -> LayoutSpec {
    let argv = |name: &str| vec!["sleep".to_owned(), name.to_owned()];
    LayoutSpec {
        board: argv("board"),
        supervisor: argv("supervisor"),
        planner: argv("planner"),
        keeper: argv("keeper"),
        inbox: argv("inbox"),
        cwd: None,
    }
}

fn session(node: &TestNode) -> String {
    session_name_for_project(node.project.as_str())
}

fn job(node: &TestNode, id: &str, state: JobState) -> Job {
    let job = node.insert_job(id, JobKind::Build, Role::Worker, Some(Vec::new()));
    node.force_state(&job, state);
    job
}

fn open_job_window(node: &TestNode, job: &Job) {
    node.windows
        .new_window(&session(node), &format!("job-{}", job.id), &sleep_spec())
        .expect("job window");
}

fn setup_layout(node: &TestNode) {
    node.windows
        .ensure_layout(node.project.as_str(), &layout())
        .expect("layout");
}

fn killed_names(node: &TestNode) -> Vec<String> {
    let session = session(node);
    node.windows
        .killed()
        .into_iter()
        .filter(|(killed_session, _)| killed_session == &session)
        .map(|(_, name)| name)
        .collect()
}

#[test]
fn cancel_closes_the_job_window_in_the_same_tick() {
    let node = TestNode::new();
    let running = job(&node, "cancel-me", JobState::Running);
    setup_layout(&node);
    open_job_window(&node, &running);
    let store = node.store();
    push(
        &store,
        &Request::CancelJob {
            job: running.id.clone(),
        },
    )
    .expect("cancel request");

    let mut supervisor = node.supervisor();
    let report = supervisor.tick().expect("tick");

    assert!(
        report
            .transitions
            .iter()
            .any(|(id, from, to)| id == &running.id && *from == JobState::Running && *to == JobState::Cancelled),
        "cancel transition missing: {:?}",
        report.transitions
    );
    assert_eq!(killed_names(&node), vec!["job-cancel-me".to_owned()]);
    assert!(
        node.windows
            .window_exists(&session(&node), "supervisor")
            .expect("fixed window"),
        "the supervisor window must stay"
    );
}

#[test]
fn tick_reconcile_closes_only_inactive_job_windows_and_is_idempotent() {
    let node = TestNode::new();
    let active = job(&node, "still-running", JobState::Running);
    let done = job(&node, "finished", JobState::Done);
    let cancelled = job(&node, "cancelled", JobState::Cancelled);
    let failed = job(&node, "failed", JobState::Failed);
    let parked = job(&node, "parked", JobState::Parked);
    let replanned = job(&node, "replanned", JobState::Replanned);
    setup_layout(&node);
    for job in [&active, &done, &cancelled, &failed, &parked, &replanned] {
        open_job_window(&node, job);
    }
    // A foreign untagged window with a job-like name must survive the whole pass.
    node.windows.untagged_window(&session(&node), "job-foreign");

    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first tick");

    let mut killed = killed_names(&node);
    killed.sort();
    let mut expected = vec![
        "job-cancelled".to_owned(),
        "job-failed".to_owned(),
        "job-finished".to_owned(),
        "job-parked".to_owned(),
        "job-replanned".to_owned(),
    ];
    expected.sort();
    assert_eq!(killed, expected, "only inactive jobs leave the active set");
    assert!(
        node.windows
            .window_exists(&session(&node), "job-still-running")
            .expect("running window"),
        "a running job keeps its window"
    );
    assert!(
        node.windows
            .window_exists(&session(&node), "job-foreign")
            .expect("foreign window"),
        "an untagged foreign window is never touched"
    );
    for name in ["board", "supervisor", "planner", "keeper", "inbox"] {
        assert!(
            node.windows.window_exists(&session(&node), name).expect("fixed window"),
            "the {name} window must stay"
        );
    }

    let before = killed_names(&node).len();
    supervisor.tick().expect("second tick");
    assert_eq!(
        killed_names(&node).len(),
        before,
        "a second pass has no windows left to close"
    );
}

#[test]
fn startup_closes_only_inactive_job_windows_and_keeps_fixed_windows() {
    let node = TestNode::new();
    let active = job(&node, "active", JobState::Running);
    let done = job(&node, "done", JobState::Done);
    let failed = job(&node, "failed", JobState::Failed);
    let parked = job(&node, "parked", JobState::Parked);
    setup_layout(&node);
    for job in [&active, &done, &failed, &parked] {
        open_job_window(&node, job);
    }

    let mut supervisor = node.supervisor();
    supervisor.startup().expect("startup");

    let mut killed = killed_names(&node);
    killed.sort();
    assert_eq!(
        killed,
        vec!["job-done".to_owned(), "job-failed".to_owned(), "job-parked".to_owned()],
        "startup closes only the windows whose job left the active set"
    );
    assert!(
        node.windows
            .window_exists(&session(&node), "job-active")
            .expect("active window"),
        "the running job keeps its window"
    );
    for name in ["board", "supervisor", "planner", "keeper", "inbox"] {
        assert!(
            node.windows.window_exists(&session(&node), name).expect("fixed window"),
            "the {name} window must stay"
        );
    }
}

#[test]
fn gc_dry_run_lists_orphan_job_windows_and_apply_closes_them() {
    let node = TestNode::new();
    let done = job(&node, "done", JobState::Done);
    let running = job(&node, "running", JobState::Running);
    setup_layout(&node);
    open_job_window(&node, &done);
    open_job_window(&node, &running);
    let store = node.store();

    let dry = lifecycle::gc(
        &node.deps,
        &store,
        &GcOptions {
            now: Some(ts()),
            ..GcOptions::default()
        },
    )
    .expect("dry run");
    let class = dry
        .classes
        .iter()
        .find(|class| class.class == CatalogClass::JobWindow)
        .expect("job_window class");
    assert_eq!(class.files, 1, "{class:?}");
    assert_eq!(class.bytes, 0, "a window carries no file bytes");
    assert_eq!(class.sample_paths.len(), 1, "{class:?}");
    assert!(
        class.sample_paths[0].contains("job-done"),
        "the orphan window is listed: {class:?}"
    );
    assert!(
        node.windows
            .window_exists(&session(&node), "job-done")
            .expect("dry run window"),
        "a dry run must not close anything"
    );

    let outcome = gc_with_window(
        &node.deps,
        &store,
        &GcOptions {
            apply: true,
            now: Some(ts()),
            ..GcOptions::default()
        },
        None,
    )
    .expect("apply");
    assert_eq!(outcome.closed_windows, 1, "{outcome:?}");
    assert!(outcome.errors.is_empty(), "{outcome:?}");
    assert!(
        !node
            .windows
            .window_exists(&session(&node), "job-done")
            .expect("collected window"),
        "apply closes the orphan"
    );
    assert!(
        node.windows
            .window_exists(&session(&node), "job-running")
            .expect("live window"),
        "a running job's window is not an orphan"
    );
    for name in ["board", "supervisor", "planner", "keeper", "inbox"] {
        assert!(
            node.windows.window_exists(&session(&node), name).expect("fixed window"),
            "the {name} window must stay"
        );
    }
}

#[test]
fn long_id_job_keeps_its_hashed_window_across_ticks_and_gc() {
    let node = TestNode::new();
    // Incident shape: a 57-char id plus `-fix-1` makes `job-<id>` exceed the 64-char name cap.
    let long_id = format!("{}-fix-1", "a".repeat(57));
    let active = job(&node, &long_id, JobState::Running);
    let done_id = format!("{}-fix-2", "a".repeat(57));
    let done = job(&node, &done_id, JobState::Done);
    setup_layout(&node);
    let active_name = vowp_tmux::window_name_for_job(active.id.as_str());
    let done_name = vowp_tmux::window_name_for_job(done.id.as_str());
    assert_ne!(active_name, format!("job-{}", active.id), "the id must be hashed");
    assert_ne!(active_name, done_name);
    for name in [&active_name, &done_name] {
        node.windows
            .new_window(&session(&node), name, &sleep_spec())
            .expect("long job window");
    }

    let mut supervisor = node.supervisor();
    supervisor.tick().expect("first tick");
    supervisor.tick().expect("second tick");

    assert_eq!(
        killed_names(&node),
        vec![done_name.clone()],
        "only the done job's window closes"
    );
    assert!(
        node.windows
            .window_exists(&session(&node), &active_name)
            .expect("active window"),
        "an active long-id job keeps its window"
    );

    let dry = lifecycle::gc(
        &node.deps,
        &node.store(),
        &GcOptions {
            now: Some(ts()),
            ..GcOptions::default()
        },
    )
    .expect("gc dry run");
    let listed: Vec<&String> = dry.classes.iter().flat_map(|class| class.sample_paths.iter()).collect();
    assert!(
        listed.iter().all(|path| !path.contains(&active_name)),
        "gc must not list the active long-id window: {listed:?}"
    );
}
