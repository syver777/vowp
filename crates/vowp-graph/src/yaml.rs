//! Stable YAML export and import of a job graph plan.
//! Spec: `docs/design/crates/vowp-graph.md` §7.
//!
//! Export writes only plan data: ids, titles, kinds, roles, profiles, priorities, needs,
//! ownership/read globs, gates and loops. Runtime rows (state, counters, branch/worktree,
//! timestamps, parked reason, notes) are deliberately absent. Import validates the document
//! like [`Graph::new`] does and returns [`NewJob`] rows ready for the store.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_yaml_ng::Value;
use vowp_protocol::{CmdGate, Edge, EdgeKind, Gate, JobId, JobKind, JobLoop, LoopUntil, NewJob, OnExhausted, Role};

use crate::error::GraphError;
use crate::graph::Graph;

/// Default command-gate timeout, matching [`CmdGate::default`].
const DEFAULT_GATE_TIMEOUT: u64 = 1800;

/// The validated contents of a graph YAML document.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedGraph {
    /// Jobs to insert, in document order.
    pub jobs: Vec<NewJob>,
    /// Non-`needs` edges; dependency edges come from each job's `needs` list.
    pub edges: Vec<Edge>,
}

/// Serialises a graph plan as stable YAML.
///
/// Jobs are sorted by id, edges by `(kind, src, dst)` and every list uses flow style so the
/// document stays compact. Fields that only exist at runtime are not written.
pub fn export_yaml(g: &Graph) -> Result<String, GraphError> {
    let mut out = String::new();
    out.push_str("version: 1\n");

    let jobs: Vec<_> = g.jobs().collect();
    if jobs.is_empty() {
        out.push_str("jobs: []\n");
    } else {
        out.push_str("jobs:\n");
        for job in jobs {
            let mut needs = job.needs.clone();
            needs.sort();
            out.push_str(&format!("  - id: {}\n", scalar(job.id.as_str())));
            out.push_str(&format!("    title: {}\n", scalar(&job.title)));
            out.push_str(&format!("    kind: {}\n", kind_str(job.kind)));
            out.push_str(&format!("    role: {}\n", role_str(job.role)));
            out.push_str(&format!("    profile: {}\n", scalar(&job.profile)));
            out.push_str(&format!("    priority: {}\n", job.priority));
            out.push_str(&format!("    needs: {}\n", flow_ids(&needs)));
            out.push_str(&format!("    owns: {}\n", flow_strings(&job.owns)));
            out.push_str(&format!("    reads: {}\n", flow_strings(&job.reads)));
            out.push_str(&format!("    gates: {}\n", flow_gates(&job.gates)));
            out.push_str(&format!(
                "    loop: {{until: {}, max_attempts: {}, on_exhausted: {}}}\n",
                until_str(job.loop_.until),
                job.loop_.max_attempts,
                on_exhausted_str(job.loop_.on_exhausted),
            ));
        }
    }

    let mut edges: Vec<&Edge> = g.edges().iter().filter(|edge| edge.kind != EdgeKind::Needs).collect();
    edges.sort_by(|a, b| {
        edge_rank(a.kind)
            .cmp(&edge_rank(b.kind))
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });
    if edges.is_empty() {
        out.push_str("edges: []\n");
    } else {
        out.push_str("edges:\n");
        for edge in edges {
            out.push_str(&format!(
                "  - {{src: {}, dst: {}, kind: {}}}\n",
                scalar(edge.src.as_str()),
                scalar(edge.dst.as_str()),
                edge_kind_str(edge.kind),
            ));
        }
    }

    Ok(out)
}

/// Parses and validates a graph YAML document.
///
/// The version must be 1, ids must be unique and known, edges must not be self edges, every
/// `needs` edge must be mirrored by the source job's `needs` list, gates must validate and
/// the precedence constraints must be acyclic (`fixes` is ignored). `Needs` edges are folded
/// into the jobs' `needs` lists and omitted from [`ImportedGraph::edges`].
pub fn import_yaml(yaml: &str) -> Result<ImportedGraph, GraphError> {
    let document: GraphDocument = serde_yaml_ng::from_str(yaml).map_err(|err| GraphError::Yaml(err.to_string()))?;
    if document.version != 1 {
        return Err(GraphError::Yaml(format!(
            "unsupported graph version {}",
            document.version
        )));
    }

    let jobs: Vec<NewJob> = document
        .jobs
        .into_iter()
        .map(RawJob::try_into_new_job)
        .collect::<Result<_, _>>()?;

    let mut ids: BTreeSet<JobId> = BTreeSet::new();
    for job in &jobs {
        if !ids.insert(job.id.clone()) {
            return Err(GraphError::DuplicateJob(job.id.clone()));
        }
    }

    for job in &jobs {
        if let Some(gates) = &job.gates {
            for gate in gates {
                gate.validate()?;
            }
        }
        for need in &job.needs {
            if *need == job.id {
                return Err(GraphError::SelfEdge(job.id.clone()));
            }
            if !ids.contains(need) {
                return Err(GraphError::UnknownJob(need.clone()));
            }
        }
    }

    let mut edges = Vec::new();
    for edge in &document.edges {
        if edge.src == edge.dst {
            return Err(GraphError::SelfEdge(edge.src.clone()));
        }
        if !ids.contains(&edge.src) {
            return Err(GraphError::UnknownJob(edge.src.clone()));
        }
        if !ids.contains(&edge.dst) {
            return Err(GraphError::UnknownJob(edge.dst.clone()));
        }
        if edge.kind == EdgeKind::Needs {
            let Some(source) = jobs.iter().find(|job| job.id == edge.src) else {
                return Err(GraphError::UnknownJob(edge.src.clone()));
            };
            if !source.needs.contains(&edge.dst) {
                return Err(GraphError::EdgeMismatch(edge.src.clone()));
            }
        } else {
            edges.push(edge.clone());
        }
    }

    if let Some(path) = detect_cycle(&jobs, &document.edges) {
        return Err(GraphError::Cycle(path));
    }

    Ok(ImportedGraph { jobs, edges })
}

/// The on-disk document envelope.
#[derive(Debug, Deserialize)]
struct GraphDocument {
    version: u32,
    #[serde(default)]
    jobs: Vec<RawJob>,
    #[serde(default)]
    edges: Vec<Edge>,
}

/// A job as written, with gates kept as raw YAML values.
///
/// `Gate` is an externally tagged enum; serde_yaml_ng would only accept YAML tags such as
/// `!cmd`, while the graph format (and the rest of VOWP) writes the map form
/// `{cmd: {...}}`. Keeping gates raw lets [`parse_gate`] accept the documented shape.
#[derive(Debug, Deserialize)]
struct RawJob {
    id: JobId,
    title: String,
    kind: JobKind,
    role: Role,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    needs: Vec<JobId>,
    #[serde(default)]
    owns: Vec<String>,
    #[serde(default)]
    reads: Vec<String>,
    #[serde(default)]
    gates: Option<Vec<Value>>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default, rename = "loop")]
    loop_: Option<JobLoop>,
    #[serde(default)]
    brief_text: Option<String>,
}

impl RawJob {
    /// Converts one raw job row, parsing its gates.
    fn try_into_new_job(self) -> Result<NewJob, GraphError> {
        let gates = match self.gates {
            Some(values) => Some(values.into_iter().map(parse_gate).collect::<Result<Vec<_>, _>>()?),
            None => None,
        };
        Ok(NewJob {
            id: self.id,
            title: self.title,
            kind: self.kind,
            role: self.role,
            profile: self.profile,
            needs: self.needs,
            owns: self.owns,
            reads: self.reads,
            gates,
            priority: self.priority,
            loop_: self.loop_,
            brief_text: self.brief_text,
        })
    }
}

/// Parses one externally tagged gate mapping such as `{cmd: {run: "...", expect_exit: 0}}`.
fn parse_gate(value: Value) -> Result<Gate, GraphError> {
    let Value::Mapping(mapping) = value else {
        return Err(GraphError::Yaml(
            "a gate must be a mapping with one gate kind".to_owned(),
        ));
    };
    if mapping.len() != 1 {
        return Err(GraphError::Yaml(
            "a gate mapping must have exactly one gate kind".to_owned(),
        ));
    }
    let Some((key, payload)) = mapping.into_iter().next() else {
        return Err(GraphError::Yaml("a gate mapping is empty".to_owned()));
    };
    let Some(kind) = key.as_str() else {
        return Err(GraphError::Yaml("a gate kind must be a string".to_owned()));
    };
    match kind {
        "report" => Ok(Gate::Report(from_value(payload)?)),
        "cmd" => Ok(Gate::Cmd(from_value(payload)?)),
        "owns_respected" => Ok(Gate::OwnsRespected(from_value(payload)?)),
        "pushed" => Ok(Gate::Pushed(from_value(payload)?)),
        "review" => Ok(Gate::Review(from_value(payload)?)),
        "script" => Ok(Gate::Script(from_value(payload)?)),
        "deploy" => Ok(Gate::Deploy(from_value(payload)?)),
        "secret_scan" => Ok(Gate::SecretScan(from_value(payload)?)),
        other => Err(GraphError::Yaml(format!("unknown gate kind {other:?}"))),
    }
}

/// Deserialises a YAML value into a protocol payload.
fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, GraphError> {
    serde_yaml_ng::from_value(value).map_err(|err| GraphError::Yaml(err.to_string()))
}

/// Renders one string as a YAML plain scalar when safe, otherwise as a double-quoted scalar.
fn scalar(value: &str) -> String {
    if is_plain_scalar(value) {
        value.to_owned()
    } else {
        quote_scalar(value)
    }
}

/// Returns true when `value` can be written without quotes and still parses as a string.
fn is_plain_scalar(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let Some(first) = value.chars().next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || matches!(first, '_' | '.' | '/')) {
        return false;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        return false;
    }
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off"
    )
}

/// Double-quotes a YAML scalar and escapes characters that cannot appear literally.
fn quote_scalar(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Renders a list of strings as a flow sequence.
fn flow_strings(values: &[String]) -> String {
    let rendered: Vec<String> = values.iter().map(|value| scalar(value)).collect();
    format!("[{}]", rendered.join(", "))
}

/// Renders a list of job ids as a flow sequence.
fn flow_ids(values: &[JobId]) -> String {
    let rendered: Vec<String> = values.iter().map(|value| scalar(value.as_str())).collect();
    format!("[{}]", rendered.join(", "))
}

/// Renders a gate list as a flow sequence.
fn flow_gates(gates: &[Gate]) -> String {
    let rendered: Vec<String> = gates.iter().map(gate_flow).collect();
    format!("[{}]", rendered.join(", "))
}

/// Renders one gate as a flow mapping.
fn gate_flow(gate: &Gate) -> String {
    match gate {
        Gate::Report(report) => format!("{{report: {{fields: {}}}}}", flow_strings(&report.fields)),
        Gate::Cmd(cmd) => format!("{{cmd: {}}}", cmd_flow(cmd)),
        Gate::OwnsRespected(enabled) => format!("{{owns_respected: {enabled}}}"),
        Gate::Pushed(pushed) => {
            let mut fields = vec![format!("remote: {}", scalar(&pushed.remote))];
            if let Some(branch) = &pushed.branch {
                fields.push(format!("branch: {}", scalar(branch)));
            }
            format!("{{pushed: {{{}}}}}", fields.join(", "))
        }
        Gate::Review(review) => format!("{{review: {{by: {}}}}}", role_str(review.by)),
        Gate::Script(cmd) => format!("{{script: {}}}", cmd_flow(cmd)),
        Gate::Deploy(deploy) => {
            let mut fields = vec![
                format!("run: {}", scalar(&deploy.run)),
                format!("probe: {}", cmd_flow(&deploy.probe)),
            ];
            if let Some(env) = &deploy.env {
                fields.push(format!("env: {}", scalar(env)));
            }
            format!("{{deploy: {{{}}}}}", fields.join(", "))
        }
        Gate::SecretScan(enabled) => format!("{{secret_scan: {enabled}}}"),
    }
}

/// Renders a command gate as a flow mapping, omitting default-valued optional fields.
fn cmd_flow(cmd: &CmdGate) -> String {
    let mut fields = vec![
        format!("run: {}", scalar(&cmd.run)),
        format!("expect_exit: {}", cmd.expect_exit),
    ];
    if let Some(pattern) = &cmd.expect_pattern {
        fields.push(format!("expect_pattern: {}", scalar(pattern)));
    }
    if cmd.timeout_secs != DEFAULT_GATE_TIMEOUT {
        fields.push(format!("timeout_secs: {}", cmd.timeout_secs));
    }
    if let Some(cwd) = &cmd.cwd {
        fields.push(format!("cwd: {}", scalar(cwd)));
    }
    format!("{{{}}}", fields.join(", "))
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

/// Returns the snake_case YAML name of a job kind.
fn kind_str(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Plan => "plan",
        JobKind::Spec => "spec",
        JobKind::Build => "build",
        JobKind::Test => "test",
        JobKind::Verify => "verify",
        JobKind::Fix => "fix",
        JobKind::Merge => "merge",
        JobKind::Docs => "docs",
        JobKind::Research => "research",
        JobKind::Promote => "promote",
        JobKind::Release => "release",
        JobKind::Decision => "decision",
    }
}

/// Returns the snake_case YAML name of a role.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::Planner => "planner",
        Role::Keeper => "keeper",
        Role::Worker => "worker",
        Role::Verifier => "verifier",
    }
}

/// Returns the snake_case YAML name of a loop stop condition.
fn until_str(until: LoopUntil) -> &'static str {
    match until {
        LoopUntil::Gates => "gates",
    }
}

/// Returns the snake_case YAML name of an exhausted action.
fn on_exhausted_str(action: OnExhausted) -> &'static str {
    match action {
        OnExhausted::Escalate => "escalate",
        OnExhausted::Park => "park",
        OnExhausted::Fail => "fail",
    }
}

/// Returns the snake_case YAML name of an edge kind.
fn edge_kind_str(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Needs => "needs",
        EdgeKind::Verifies => "verifies",
        EdgeKind::Fixes => "fixes",
        EdgeKind::Escalates => "escalates",
        EdgeKind::Blocks => "blocks",
    }
}

/// Colour of a job during the depth-first cycle search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Grey,
    Black,
}

/// Returns one cycle path (`first == last`) over the imported precedence edges, if any.
fn detect_cycle(jobs: &[NewJob], edges: &[Edge]) -> Option<Vec<JobId>> {
    let mut adjacency: BTreeMap<JobId, BTreeSet<JobId>> =
        jobs.iter().map(|job| (job.id.clone(), BTreeSet::new())).collect();

    for job in jobs {
        for need in &job.needs {
            if let Some(next) = adjacency.get_mut(need) {
                next.insert(job.id.clone());
            }
        }
    }
    for edge in edges {
        let (from, to) = match edge.kind {
            EdgeKind::Needs | EdgeKind::Verifies | EdgeKind::Escalates => (&edge.dst, &edge.src),
            EdgeKind::Blocks => (&edge.src, &edge.dst),
            EdgeKind::Fixes => continue,
        };
        if let Some(next) = adjacency.get_mut(from) {
            next.insert(to.clone());
        }
    }

    let mut color: BTreeMap<JobId, Color> = jobs.iter().map(|job| (job.id.clone(), Color::White)).collect();
    let mut stack = Vec::new();
    for job in jobs {
        if color.get(&job.id) == Some(&Color::White) {
            if let Some(path) = depth_first(&job.id, &adjacency, &mut color, &mut stack) {
                return Some(path);
            }
        }
    }
    None
}

/// Depth-first visit that returns the cycle path when it reaches a grey node.
fn depth_first(
    node: &JobId,
    adjacency: &BTreeMap<JobId, BTreeSet<JobId>>,
    color: &mut BTreeMap<JobId, Color>,
    stack: &mut Vec<JobId>,
) -> Option<Vec<JobId>> {
    color.insert(node.clone(), Color::Grey);
    stack.push(node.clone());

    if let Some(nexts) = adjacency.get(node) {
        for next in nexts {
            match color.get(next).copied().unwrap_or(Color::White) {
                Color::White => {
                    if let Some(path) = depth_first(next, adjacency, color, stack) {
                        return Some(path);
                    }
                }
                Color::Grey => {
                    let start = stack.iter().position(|candidate| candidate == next).unwrap_or(0);
                    let mut path = stack[start..].to_vec();
                    path.push(next.clone());
                    return Some(path);
                }
                Color::Black => {}
            }
        }
    }

    stack.pop();
    color.insert(node.clone(), Color::Black);
    None
}
