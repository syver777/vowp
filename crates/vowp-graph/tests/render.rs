//! Golden renderings for a four-job graph plus the title/escaping rules.
//! Spec: `docs/design/crates/vowp-graph.md` §6 and §10.
//!
//! The goldens live inline rather than under `tests/fixtures/`; the compared bytes are the
//! same either way.

use vowp_graph::{Graph, render_ascii, render_mermaid};
use vowp_protocol::{Edge, EdgeKind, Job, JobId, JobKind, JobLoop, JobState, ProjectName, Role, Timestamp};

/// The ASCII golden for the four-job graph.
const ASCII_GOLDEN: &str = concat!(
    "L0 n01-skeleton [done] Cargo workspace skeleton\n",
    "L1 n02a-protocol-foundation [running] vowp-protocol foundation  <- n01-skeleton\n",
    "L1 n03-store [pending] vowp-store  <- n01-skeleton\n",
    "L2 n04-graph [pending] vowp-graph  <- n02a-protocol-foundation\n",
);

/// The Mermaid golden for the four-job graph.
const MERMAID_GOLDEN: &str = concat!(
    "graph TD\n",
    "    n01_skeleton[\"n01-skeleton (done)\"]\n",
    "    n02a_protocol_foundation[\"n02a-protocol-foundation (running)\"]\n",
    "    n03_store[\"n03-store (pending)\"]\n",
    "    n04_graph[\"n04-graph (pending)\"]\n",
    "    n01_skeleton --> n02a_protocol_foundation\n",
    "    n01_skeleton --> n03_store\n",
    "    n02a_protocol_foundation --> n04_graph\n",
    "    n03_store -.->|verifies| n02a_protocol_foundation\n",
    "    n04_graph ==>|fixes| n03_store\n",
    "    n04_graph -.->|escalates| n01_skeleton\n",
    "    n04_graph -->|blocks| n03_store\n",
    "    classDef running fill:#fff4d6,stroke:#b7791f,color:#744210\n",
    "    classDef waiting fill:#e8f0fe,stroke:#3b6fb6,color:#1a3d6d\n",
    "    classDef failed fill:#fde8e8,stroke:#b42318,color:#7a1a12\n",
    "    classDef done fill:#e6f4ea,stroke:#2f7d4f,color:#1d4d31\n",
    "    class n01_skeleton done\n",
    "    class n02a_protocol_foundation running\n",
    "    class n03_store waiting\n",
    "    class n04_graph waiting\n",
);

/// Builds a fixed timestamp.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Builds one job row.
fn job(id: &str, title: &str, state: JobState, needs: &[&str]) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    Job {
        id: id.clone(),
        project,
        title: title.to_owned(),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state,
        priority: 50,
        needs: needs
            .iter()
            .map(|need| JobId::parse(need).expect("test need id is valid"))
            .collect(),
        owns: Vec::new(),
        reads: Vec::new(),
        brief: format!(".vowp/jobs/{id}/brief.md"),
        report: format!(".vowp/jobs/{id}/report.md"),
        gates: Vec::new(),
        loop_: JobLoop::default(),
        branch: Some(format!("job/{id}")),
        worktree: Some(format!("/tmp/wt/{id}")),
        attempts: 0,
        continues: 0,
        hang_restarts: 0,
        parked_reason: None,
        note: String::new(),
        created_by: "test".to_owned(),
        last_progress_at: None,
        state_changed_at: at(1_700_000_000),
        created_at: at(1_700_000_000),
        updated_at: at(1_700_000_000),
    }
}

/// Builds an edge from ids.
fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
    Edge {
        src: JobId::parse(src).expect("test edge src is valid"),
        dst: JobId::parse(dst).expect("test edge dst is valid"),
        kind,
    }
}

/// The four-job graph used by both goldens.
fn golden_graph() -> Graph {
    let jobs = vec![
        job("n01-skeleton", "Cargo workspace skeleton", JobState::Done, &[]),
        job(
            "n02a-protocol-foundation",
            "vowp-protocol foundation",
            JobState::Running,
            &["n01-skeleton"],
        ),
        job("n03-store", "vowp-store", JobState::Pending, &["n01-skeleton"]),
        job(
            "n04-graph",
            "vowp-graph",
            JobState::Pending,
            &["n02a-protocol-foundation"],
        ),
    ];
    let edges = vec![
        edge("n03-store", "n02a-protocol-foundation", EdgeKind::Verifies),
        edge("n04-graph", "n03-store", EdgeKind::Fixes),
        edge("n04-graph", "n01-skeleton", EdgeKind::Escalates),
        edge("n04-graph", "n03-store", EdgeKind::Blocks),
    ];
    Graph::new(jobs, edges).expect("golden graph is valid")
}

/// The Mermaid class definitions in their fixed output order.
const MERMAID_CLASS_DEFS: [&str; 4] = [
    "    classDef running fill:#fff4d6,stroke:#b7791f,color:#744210",
    "    classDef waiting fill:#e8f0fe,stroke:#3b6fb6,color:#1a3d6d",
    "    classDef failed fill:#fde8e8,stroke:#b42318,color:#7a1a12",
    "    classDef done fill:#e6f4ea,stroke:#2f7d4f,color:#1d4d31",
];

#[test]
fn render_ascii_golden() {
    let ascii = render_ascii(&golden_graph());
    assert_eq!(ascii, ASCII_GOLDEN);
    for syntax in ["classDef", "class ", "fill:", "stroke:", "color:"] {
        assert!(!ascii.contains(syntax), "ASCII must not emit {syntax:?}: {ascii:?}");
    }
}

#[test]
fn render_mermaid_golden() {
    assert_eq!(render_mermaid(&golden_graph()), MERMAID_GOLDEN);
}

#[test]
fn render_mermaid_classes_cover_every_group() {
    let jobs = vec![
        job("a-running", "Running job", JobState::Running, &[]),
        job("b-waiting", "Waiting job", JobState::Ready, &[]),
        job("c-failed", "Failed job", JobState::Failed, &[]),
        job("d-done", "Done job", JobState::Done, &[]),
    ];
    let edges = vec![edge("d-done", "a-running", EdgeKind::Verifies)];
    let graph = Graph::new(jobs, edges).expect("state graph is valid");
    let mermaid = render_mermaid(&graph);

    let nodes_and_edges: Vec<&str> = mermaid
        .lines()
        .take_while(|line| !line.starts_with("    classDef "))
        .collect();
    assert_eq!(
        nodes_and_edges,
        [
            "graph TD",
            "    a_running[\"a-running (running)\"]",
            "    b_waiting[\"b-waiting (ready)\"]",
            "    c_failed[\"c-failed (failed)\"]",
            "    d_done[\"d-done (done)\"]",
            "    d_done -.->|verifies| a_running",
        ],
        "node and edge lines changed"
    );

    let class_defs: Vec<&str> = mermaid
        .lines()
        .filter(|line| line.starts_with("    classDef "))
        .collect();
    assert_eq!(class_defs, MERMAID_CLASS_DEFS, "classDef order is not fixed");

    let assignments: Vec<&str> = mermaid.lines().filter(|line| line.starts_with("    class ")).collect();
    assert_eq!(
        assignments,
        [
            "    class a_running running",
            "    class b_waiting waiting",
            "    class c_failed failed",
            "    class d_done done",
        ],
        "per-job class assignments changed"
    );
}

#[test]
fn render_mermaid_maps_every_state_to_its_group() {
    let states = [
        JobState::Pending,
        JobState::Ready,
        JobState::Running,
        JobState::Stalled,
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
        JobState::Done,
        JobState::Failed,
        JobState::Parked,
        JobState::Replanned,
        JobState::Cancelled,
    ];
    let jobs: Vec<Job> = states
        .iter()
        .enumerate()
        .map(|(index, state)| {
            let id = format!("s{index:02}-{}", state.as_str());
            job(&id, "State job", *state, &[])
        })
        .collect();
    let graph = Graph::new(jobs, Vec::new()).expect("state graph is valid");
    let mermaid = render_mermaid(&graph);
    let assignments: Vec<&str> = mermaid.lines().filter(|line| line.starts_with("    class ")).collect();

    assert_eq!(
        assignments,
        [
            "    class s00_pending waiting",
            "    class s01_ready waiting",
            "    class s02_running running",
            "    class s03_stalled running",
            "    class s04_gating running",
            "    class s05_reviewing running",
            "    class s06_fixing running",
            "    class s07_merging running",
            "    class s08_done done",
            "    class s09_failed failed",
            "    class s10_parked failed",
            "    class s11_replanned done",
            "    class s12_cancelled done",
        ],
        "state-to-group mapping changed"
    );
}

#[test]
fn render_ascii_truncates_long_titles() {
    let job = job("long-title", &"x".repeat(60), JobState::Pending, &[]);
    let graph = Graph::new(vec![job], Vec::new()).expect("graph is valid");
    let line = render_ascii(&graph);
    assert!(line.contains(&format!("[pending] {}", "x".repeat(48))));
    assert!(!line.contains(&"x".repeat(49)));

    let mermaid = render_mermaid(&graph);
    assert!(mermaid.contains("long_title[\"long-title (pending)\"]"));
    assert!(mermaid.ends_with('\n'));
}
