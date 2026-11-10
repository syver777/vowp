//! `vowp inbox` integration tests.

mod common;

use std::sync::Arc;

use vowp_core::deps::{Deps, FixedRng, SystemGit, SystemRunner};
use vowp_core::requests;
use vowp_protocol::{InboxState, JobState, NodeName, ParkedReason};
use vowp_store::{NodePaths, StateChange};
use vowp_tmux::fake::FakeWindows;

/// Builds the dependencies the request resolver needs.
fn deps(env: &common::TestEnv) -> Deps {
    let paths = NodePaths::new(env.home_path());
    let adapters = vowp_core::AdapterRegistry::from_config(&env.config).expect("adapters");
    Deps {
        paths: paths.clone(),
        config: Arc::new(env.config.clone()),
        node: NodeName::parse("local").expect("node"),
        clock: Arc::new(vowp_protocol::SystemClock),
        log: Arc::new(vowp_protocol::NullLog),
        adapters: Arc::new(adapters),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

#[test]
fn inbox_answer_unparks_job_on_tick() {
    let env = common::TestEnv::new();
    let job = env.insert_job("job-a", "Job A", vowp_protocol::JobKind::Build, &[]);

    let (code, stdout, stderr) = env.run(&[
        "inbox",
        "ask",
        "Need owner input",
        "--job",
        "job-a",
        "--kind",
        "approval",
        "--option",
        "yes",
        "--option",
        "no",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let item_id = stdout.trim().to_owned();
    let store = env.store();
    let item_id_parsed = vowp_protocol::InboxId::parse(&item_id).expect("inbox id");
    let item = store
        .inbox_item(&item_id_parsed)
        .expect("lookup")
        .expect("item inserted");
    assert_eq!(item.kind, vowp_protocol::InboxKind::Approval);
    assert_eq!(item.options, vec!["yes".to_owned(), "no".to_owned()]);
    let parked = store.require_job(&job.id).expect("job");
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(parked.parked_reason, Some(ParkedReason::Question));

    let (code, _, stderr) = env.run(&["inbox", "answer", &item_id, "--approve", "--by", "owner"]);
    assert_eq!(code, 0, "{stderr}");
    let answered = store.inbox_item(&item_id_parsed).expect("lookup").expect("item");
    assert_eq!(answered.state, InboxState::Answered);
    assert_eq!(answered.answer.as_deref(), Some("approve"));
    assert_eq!(answered.answered_by.as_deref(), Some("owner"));

    // The answer is queued, exactly as the supervisor's tick consumes it.
    let deps = deps(&env);
    let queued = requests::drain(&store).expect("drain requests");
    assert_eq!(queued.len(), 1);
    for (_, request) in queued {
        requests::apply_direct(&deps, &store, &request).expect("apply request");
    }
    let resolved = store.require_job(&job.id).expect("job");
    assert_eq!(resolved.state, JobState::Ready);
    assert_eq!(resolved.parked_reason, None);
}

#[test]
fn inbox_second_answer_is_conflict() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["inbox", "ask", "Question"]);
    assert_eq!(code, 0, "{stderr}");
    let id = stdout.trim().to_owned();
    let (code, _, stderr) = env.run(&["inbox", "answer", &id, "first"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run(&["inbox", "answer", &id, "second"]);
    assert_eq!(code, 4, "{stderr}");
}

#[test]
fn inbox_dismiss_does_not_resolve_a_job() {
    let env = common::TestEnv::new();
    let job = env.insert_job("job-b", "Job B", vowp_protocol::JobKind::Build, &[]);
    let store = env.store();
    store
        .set_job_state(
            &job.id,
            JobState::Parked,
            &StateChange {
                note: None,
                parked_reason: Some(ParkedReason::Question),
            },
        )
        .expect("park");
    let (code, stdout, stderr) = env.run(&["inbox", "ask", "FYI", "--job", "job-b", "--kind", "notice"]);
    assert_eq!(code, 0, "{stderr}");
    let id = stdout.trim().to_owned();
    let (code, _, stderr) = env.run(&["inbox", "dismiss", &id]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Parked);
    assert_eq!(
        store
            .inbox_item(&vowp_protocol::InboxId::parse(&id).expect("inbox id"))
            .expect("lookup")
            .expect("item")
            .state,
        InboxState::Dismissed
    );
}
