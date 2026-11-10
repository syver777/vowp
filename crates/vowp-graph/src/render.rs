//! Deterministic text renderings of a job graph.
//! Spec: `docs/design/crates/vowp-graph.md` §6.
//!
//! Both renderers are pure and stable: jobs are ordered explicitly and no timestamps or
//! colours appear in the output. ASCII shows `needs` edges only; Mermaid shows every kind.

use vowp_protocol::{Edge, EdgeKind, Job, JobId};

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
/// dependent; every other kind points from source to destination.
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

    out
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
