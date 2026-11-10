//! L7 inbox tests: answering a parked job's question.
//! Spec: `docs/design/loops.md` §9.

#[path = "failover_support.rs"]
mod f;

use f::*;
use vowp_protocol::{InboxId, InboxItem, InboxKind, InboxState, JobKind, JobState, Role};
use vowp_store::JobFilter;

#[test]
fn inbox_answer_moves_parked_job_to_ready() {
    let node = TestNode::new_real_git();
    let job = node.insert_job("answer-job", JobKind::Build, Role::Worker, Some(vec![]));
    let worktree = node.worktree_with_file(&job, "src/answer.rs", "pub fn answer() {}\n");
    node.force_state(&job, JobState::Parked);
    let store = node.store();
    let inbox_id = InboxId::new();
    store
        .insert_inbox(&InboxItem {
            id: inbox_id.clone(),
            project: Some(node.project.clone()),
            job_id: Some(job.id.clone()),
            kind: InboxKind::Question,
            title: "question: answer-job".to_owned(),
            body: "which answer?".to_owned(),
            options: vec!["resolved".to_owned()],
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: ts(),
            answered_at: None,
        })
        .expect("inbox");
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l2-worker", vowp_protocol::ActiveState::Paused)
        .expect("pause l2");
    store
        .set_loop_state("l4-planner", vowp_protocol::ActiveState::Paused)
        .expect("pause planner");
    store.answer_inbox(&inbox_id, "use yes", "test").expect("answer");
    vowp_core::requests::push(
        &store,
        &vowp_core::requests::Request::AnswerInbox { id: inbox_id.clone() },
    )
    .expect("request");

    supervisor.tick().expect("tick");
    assert!(
        vowp_core::loops::l4_planner::event_wake(&store, "planner").expect("wake"),
        "a new inbox answer is an unseen planner event"
    );
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Ready);
    assert!(job.note.contains("owner answer: use yes"), "note: {}", job.note);
    let answer_path = worktree.join(".vowp/jobs/answer-job/answer-1.md");
    let answer = std::fs::read_to_string(&answer_path).expect("answer file");
    assert_eq!(answer.trim(), "use yes");
    let item = store
        .inbox_item(&InboxId::parse(inbox_id.as_str()).expect("id"))
        .expect("inbox")
        .expect("row");
    assert_eq!(item.state, InboxState::Answered);
    assert_eq!(item.answer.as_deref(), Some("use yes"));

    // A second answer appends a second file and note line.
    let inbox_id = InboxId::new();
    store
        .insert_inbox(&InboxItem {
            id: inbox_id.clone(),
            project: Some(node.project.clone()),
            job_id: Some(job.id.clone()),
            kind: InboxKind::Question,
            title: "question: answer-job again".to_owned(),
            body: "another?".to_owned(),
            options: vec!["resolved".to_owned()],
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: ts(),
            answered_at: None,
        })
        .expect("inbox");
    node.force_state(&job, JobState::Parked);
    store.answer_inbox(&inbox_id, "second", "test").expect("answer");
    vowp_core::requests::push(
        &store,
        &vowp_core::requests::Request::AnswerInbox { id: inbox_id.clone() },
    )
    .expect("request");
    supervisor.tick().expect("tick two");
    assert!(worktree.join(".vowp/jobs/answer-job/answer-2.md").is_file());
    let jobs = store
        .list_jobs(&JobFilter {
            project: Some(node.project.clone()),
            ..JobFilter::default()
        })
        .expect("jobs");
    assert_eq!(jobs.len(), 1);
}
