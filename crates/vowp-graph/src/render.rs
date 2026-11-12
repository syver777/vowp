//! Deterministic text renderings of a job graph.
//! Spec: `docs/design/crates/vowp-graph.md` §6.
//!
//! Both renderers are pure and stable: jobs are ordered explicitly and no timestamps appear in
//! the output. ASCII shows `needs` edges only and never emits colour; Mermaid shows every kind
//! and ends with deterministic state colours.
//!
//! Mermaid state groups reuse the protocol predicates: `running` is
//! [`JobState::holds_lock`] (`running`, `stalled`, `gating`, `reviewing`, `fixing`, `merging`),
//! `waiting` is `pending`/`ready`, `failed` is `failed`/`parked`, and `done` is
//! [`JobState::is_terminal`] (`done`, `replanned`, `cancelled`). The label already spells out
//! the state, so colour is never the only signal.

use vowp_protocol::{Edge, EdgeKind, Job, JobId, JobState};

use crate::graph::Graph;

/// Maximum number of title characters shown on an ASCII line.
const ASCII_TITLE_CHARS: usize = 48;

/// Renders the graph as one line per job, ordered by layer then id.
///
/// Each line is `L{layer} {id} [{state}] {title}` followed by `  <- ` and the ids this job
/// needs, comma-separated, when it has any. The output ends with a newline.
pub fn render_ascii(g: &Graph) -> String {
    let mut rows: Vec<(usize, &Job)> = g.jobs().map(|job| (g.layer(&job.id), job)).collect();
    rows.sort_by(|(left_layer, left), (right_layer, right)| {
        left_layer.cmp(right_layer).then_with(|| left.id.cmp(&right.id))
    });

    let mut out = String::new();
    for (layer, job) in rows {
        let title: String = job.title.chars().take(ASCII_TITLE_CHARS).collect();
        out.push_str(&format!("L{layer} {} [{}] {title}", job.id, job.state.as_str()));
        let needs = g.needs_of(&job.id);
        if !needs.is_empty() {
            let ids: Vec<String> = needs.iter().map(|need| need.id.to_string()).collect();
            out.push_str("  <- ");
            out.push_str(&ids.join(", "));
        }
        out.push('\n');
    }
    out
}

/// Renders the graph as a Mermaid `graph TD` document.
///
/// Nodes are emitted in id order with the key `id` with `-` replaced by `_` and the label
/// `"id (state)"`; `"` in a label is replaced by `'`. Edges are emitted in
/// `(kind, src, dst)` order. Dependency (`needs`) edges point from the dependency to the
/// dependent; every other kind points from source to destination. Then one `classDef` line
/// per [`StateGroup`] follows in the fixed order `running`, `waiting`, `failed`, `done`, and
/// finally one `class <key> <group>` line per job in id order.
pub fn render_mermaid(g: &Graph) -> String {
    let mut out = String::from("graph TD\n");

    for job in g.jobs() {
        let key = mermaid_key(&job.id);
        let label = format!("{} ({})", job.id, job.state.as_str()).replace('"', "'");
        out.push_str(&format!("    {key}[\"{label}\"]\n"));
    }

    let mut edges: Vec<&Edge> = g.edges().iter().collect();
    edges.sort_by(|a, b| {
        edge_rank(a.kind)
            .cmp(&edge_rank(b.kind))
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });

    for edge in edges {
        let src = mermaid_key(&edge.src);
        let dst = mermaid_key(&edge.dst);
        match edge.kind {
            EdgeKind::Needs => out.push_str(&format!("    {dst} --> {src}\n")),
            EdgeKind::Verifies => out.push_str(&format!("    {src} -.->|verifies| {dst}\n")),
            EdgeKind::Fixes => out.push_str(&format!("    {src} ==>|fixes| {dst}\n")),
            EdgeKind::Escalates => out.push_str(&format!("    {src} -.->|escalates| {dst}\n")),
            EdgeKind::Blocks => out.push_str(&format!("    {src} -->|blocks| {dst}\n")),
        }
    }

    for group in StateGroup::ALL {
        out.push_str(&format!("    classDef {} {}\n", group.name(), group.style()));
    }

    for job in g.jobs() {
        out.push_str(&format!(
            "    class {} {}\n",
            mermaid_key(&job.id),
            StateGroup::of(job.state).name()
        ));
    }

    out
}

/// The visual state groups of the Mermaid rendering, in fixed `classDef` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateGroup {
    /// A worker or verifier holds a slot.
    Running,
    /// Waiting to be dispatched.
    Waiting,
    /// Waiting for a retry or a human.
    Failed,
    /// Finished.
    Done,
}

impl StateGroup {
    /// Every group in the fixed `classDef` output order.
    const ALL: [StateGroup; 4] = [
        StateGroup::Running,
        StateGroup::Waiting,
        StateGroup::Failed,
        StateGroup::Done,
    ];

    /// Returns the group for a job state.
    ///
    /// `Running` is exactly [`JobState::holds_lock`] and `Done` is exactly
    /// [`JobState::is_terminal`]; the other two groups cover the remaining states.
    fn of(state: JobState) -> StateGroup {
        match state {
            JobState::Pending | JobState::Ready => StateGroup::Waiting,
            JobState::Failed | JobState::Parked => StateGroup::Failed,
            JobState::Done | JobState::Replanned | JobState::Cancelled => StateGroup::Done,
            JobState::Running
            | JobState::Stalled
            | JobState::Gating
            | JobState::Reviewing
            | JobState::Fixing
            | JobState::Merging => StateGroup::Running,
        }
    }

    /// Returns the Mermaid class name.
    fn name(self) -> &'static str {
        match self {
            StateGroup::Running => "running",
            StateGroup::Waiting => "waiting",
            StateGroup::Failed => "failed",
            StateGroup::Done => "done",
        }
    }

    /// Returns the readable fill/stroke/text style used by `classDef`.
    fn style(self) -> &'static str {
        match self {
            StateGroup::Running => "fill:#fff4d6,stroke:#b7791f,color:#744210",
            StateGroup::Waiting => "fill:#e8f0fe,stroke:#3b6fb6,color:#1a3d6d",
            StateGroup::Failed => "fill:#fde8e8,stroke:#b42318,color:#7a1a12",
            StateGroup::Done => "fill:#e6f4ea,stroke:#2f7d4f,color:#1d4d31",
        }
    }
}

/// Returns the stable sort rank of an edge kind.
fn edge_rank(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Needs => 0,
        EdgeKind::Verifies => 1,
        EdgeKind::Fixes => 2,
        EdgeKind::Escalates => 3,
        EdgeKind::Blocks => 4,
    }
}

/// Returns the Mermaid node key for a job id.
fn mermaid_key(id: &JobId) -> String {
    id.as_str().replace('-', "_")
}
