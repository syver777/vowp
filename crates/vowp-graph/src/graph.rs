//! The job DAG: validation, deterministic topological order, readiness and layers.
//! Spec: `docs/design/crates/vowp-graph.md` §2.
//!
//! A [`Graph`] is immutable once built. Every mutating operation returns a new graph through
//! [`Graph::with_job_replaced`], so callers can keep a consistent snapshot per tick.
//!
//! Edge reading is `src <kind> dst`:
//! * `a needs b` means b runs before a;
//! * `a blocks b` means a runs before b;
//! * `a verifies b` and `a escalates b` mean the subject b runs before its reviewer;
//! * `a fixes b` is the bounded loop edge and imposes no ordering.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

use vowp_protocol::{Edge, EdgeKind, Job, JobId, JobState};

use crate::error::GraphError;

/// A validated job graph with the `needs` relationships indexed.
///
/// Construction checks that ids are unique, every edge endpoint exists, no edge is a self
/// edge, every `Needs` edge is mirrored by [`Job::needs`] (missing edges are added from that
/// list), and the precedence constraints are acyclic. `Fixes` edges never participate in
/// ordering or cycle detection: they are the one loop-forming edge.
#[derive(Debug, Clone)]
pub struct Graph {
    jobs: BTreeMap<JobId, Job>,
    edges: Vec<Edge>,
}

impl Graph {
    /// Validates and indexes a graph.
    ///
    /// Errors are [`GraphError::DuplicateJob`], [`GraphError::UnknownJob`],
    /// [`GraphError::SelfEdge`], [`GraphError::Cycle`] and [`GraphError::EdgeMismatch`].
    /// A `Needs` edge that is missing from the source job's `needs` list is an
    /// `EdgeMismatch`; a `needs` entry without an edge is added automatically.
    pub fn new(jobs: Vec<Job>, mut edges: Vec<Edge>) -> Result<Graph, GraphError> {
        let mut indexed: BTreeMap<JobId, Job> = BTreeMap::new();
        for job in jobs {
            let id = job.id.clone();
            if indexed.insert(id.clone(), job).is_some() {
                return Err(GraphError::DuplicateJob(id));
            }
        }

        for edge in &edges {
            if edge.src == edge.dst {
                return Err(GraphError::SelfEdge(edge.src.clone()));
            }
        }

        for edge in &edges {
            if !indexed.contains_key(&edge.src) {
                return Err(GraphError::UnknownJob(edge.src.clone()));
            }
            if !indexed.contains_key(&edge.dst) {
                return Err(GraphError::UnknownJob(edge.dst.clone()));
            }
        }

        // Every Needs edge must be mirrored by the source job's `needs` list.
        for edge in &edges {
            if edge.kind == EdgeKind::Needs {
                let job = indexed
                    .get(&edge.src)
                    // invariant: checked above that every edge endpoint exists
                    .ok_or_else(|| GraphError::UnknownJob(edge.src.clone()))?;
                if !job.needs.contains(&edge.dst) {
                    return Err(GraphError::EdgeMismatch(edge.src.clone()));
                }
            }
        }

        // Every `needs` entry must have a Needs edge; add the missing ones.
        let mut additions: Vec<Edge> = Vec::new();
        for job in indexed.values() {
            for need in &job.needs {
                if *need == job.id {
                    return Err(GraphError::SelfEdge(job.id.clone()));
                }
                if !indexed.contains_key(need) {
                    return Err(GraphError::UnknownJob(need.clone()));
                }
                let present = edges
                    .iter()
                    .chain(additions.iter())
                    .any(|edge| edge.kind == EdgeKind::Needs && edge.src == job.id && edge.dst == *need);
                if !present {
                    additions.push(Edge {
                        src: job.id.clone(),
                        dst: need.clone(),
                        kind: EdgeKind::Needs,
                    });
                }
            }
        }
        edges.extend(additions);

        if let Some(path) = detect_cycle(&indexed, &edges) {
            return Err(GraphError::Cycle(path));
        }

        Ok(Self { jobs: indexed, edges })
    }

    /// Returns the job with this id, if any.
    pub fn job(&self, id: &JobId) -> Option<&Job> {
        self.jobs.get(id)
    }

    /// Returns every job ordered by id.
    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    /// Returns the stored edges in insertion order (`needs` edges added from job rows come last).
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Returns the jobs this job needs, ordered by id.
    pub fn needs_of(&self, id: &JobId) -> Vec<&Job> {
        let mut ids: BTreeSet<&JobId> = BTreeSet::new();
        for edge in &self.edges {
            if edge.kind == EdgeKind::Needs && edge.src == *id {
                ids.insert(&edge.dst);
            }
        }
        ids.into_iter().filter_map(|need| self.jobs.get(need)).collect()
    }

    /// Returns the jobs that need this job, ordered by id.
    pub fn dependents_of(&self, id: &JobId) -> Vec<&Job> {
        let mut ids: BTreeSet<&JobId> = BTreeSet::new();
        for edge in &self.edges {
            if edge.kind == EdgeKind::Needs && edge.dst == *id {
                ids.insert(&edge.src);
            }
        }
        ids.into_iter().filter_map(|src| self.jobs.get(src)).collect()
    }

    /// Returns the edges of `kind` whose source is `id`.
    pub fn edges_from(&self, id: &JobId, kind: EdgeKind) -> Vec<&Edge> {
        self.edges
            .iter()
            .filter(|edge| edge.kind == kind && edge.src == *id)
            .collect()
    }

    /// Returns the edges of `kind` whose destination is `id`.
    pub fn edges_to(&self, id: &JobId, kind: EdgeKind) -> Vec<&Edge> {
        self.edges
            .iter()
            .filter(|edge| edge.kind == kind && edge.dst == *id)
            .collect()
    }

    /// Returns the destination of a fix job's `Fixes` edge.
    pub fn fix_parent(&self, fix_job: &JobId) -> Option<&JobId> {
        self.edges
            .iter()
            .find(|edge| edge.kind == EdgeKind::Fixes && edge.src == *fix_job)
            .map(|edge| &edge.dst)
    }

    /// Returns the deterministic topological order of the graph.
    ///
    /// Kahn's algorithm runs over `Needs`, `Blocks`, `Verifies` and `Escalates` in their
    /// precedence direction; `Fixes` imposes nothing. Ties are broken by priority
    /// (descending) and then id (ascending).
    pub fn topo_order(&self) -> Vec<JobId> {
        let mut adjacency: BTreeMap<JobId, BTreeSet<JobId>> =
            self.jobs.keys().map(|id| (id.clone(), BTreeSet::new())).collect();
        let mut indegree: BTreeMap<JobId, usize> = self.jobs.keys().map(|id| (id.clone(), 0)).collect();

        for edge in &self.edges {
            let Some((from, to)) = precedence(edge) else {
                continue;
            };
            let inserted = adjacency
                .get_mut(from)
                .map(|set| set.insert(to.clone()))
                .unwrap_or(false);
            if inserted {
                if let Some(degree) = indegree.get_mut(to) {
                    *degree += 1;
                }
            }
        }

        let mut queue = BinaryHeap::new();
        for (id, degree) in &indegree {
            if *degree == 0 {
                if let Some(job) = self.jobs.get(id) {
                    queue.push(QueueItem {
                        priority: job.priority,
                        id: id.clone(),
                    });
                }
            }
        }

        let mut order = Vec::with_capacity(self.jobs.len());
        while let Some(item) = queue.pop() {
            order.push(item.id.clone());
            if let Some(nexts) = adjacency.get(&item.id) {
                for next in nexts {
                    if let Some(degree) = indegree.get_mut(next) {
                        *degree -= 1;
                        if *degree == 0 {
                            if let Some(job) = self.jobs.get(next) {
                                queue.push(QueueItem {
                                    priority: job.priority,
                                    id: next.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
        order
    }

    /// Returns the pending jobs that can start now, in dispatch order.
    ///
    /// A job is ready when it is [`JobState::Pending`], every job it needs is
    /// [`JobState::Done`], and every job that blocks it is terminal. The result is sorted by
    /// priority (descending), creation time (ascending) and id (ascending).
    pub fn ready_jobs(&self) -> Vec<JobId> {
        let mut ready: Vec<&Job> = self
            .jobs
            .values()
            .filter(|job| job.state == JobState::Pending && self.is_ready(job))
            .collect();
        ready.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        ready.into_iter().map(|job| job.id.clone()).collect()
    }

    /// Returns the longest `Needs` path length from any root.
    ///
    /// A job without needs is layer 0. Unknown ids are layer 0.
    pub fn layer(&self, id: &JobId) -> usize {
        let mut memo = HashMap::new();
        self.layer_of(id, &mut memo)
    }

    fn layer_of(&self, id: &JobId, memo: &mut HashMap<JobId, usize>) -> usize {
        if let Some(layer) = memo.get(id) {
            return *layer;
        }
        let mut layer = 0;
        for need in self.needs_of(id) {
            layer = layer.max(self.layer_of(&need.id, memo) + 1);
        }
        memo.insert(id.clone(), layer);
        layer
    }

    /// Returns a new graph with `job` replacing the stored job of the same id.
    ///
    /// An id that is not already in the graph is appended, so the method doubles as a
    /// functional upsert. The result is revalidated exactly like [`Graph::new`]. `job.needs`
    /// is authoritative: missing `Needs` edges are added and stale ones make the update fail
    /// with [`GraphError::EdgeMismatch`].
    pub fn with_job_replaced(&self, job: Job) -> Result<Graph, GraphError> {
        let mut jobs: Vec<Job> = self.jobs.values().cloned().collect();
        match jobs.iter_mut().find(|existing| existing.id == job.id) {
            Some(existing) => *existing = job,
            None => jobs.push(job),
        }
        Graph::new(jobs, self.edges.clone())
    }

    /// Returns true when every prerequisite of `job` is satisfied.
    fn is_ready(&self, job: &Job) -> bool {
        for need in &job.needs {
            match self.jobs.get(need) {
                Some(prerequisite) if prerequisite.state == JobState::Done => {}
                _ => return false,
            }
        }
        for edge in &self.edges {
            if edge.kind == EdgeKind::Blocks && edge.dst == job.id {
                match self.jobs.get(&edge.src) {
                    Some(blocker) if blocker.state.is_terminal() => {}
                    _ => return false,
                }
            }
        }
        true
    }
}

/// Returns the precedence direction of an edge as `(before, after)`, or `None` for `Fixes`.
fn precedence(edge: &Edge) -> Option<(&JobId, &JobId)> {
    match edge.kind {
        EdgeKind::Needs | EdgeKind::Verifies | EdgeKind::Escalates => Some((&edge.dst, &edge.src)),
        EdgeKind::Blocks => Some((&edge.src, &edge.dst)),
        EdgeKind::Fixes => None,
    }
}

/// A priority-queue entry that pops highest priority first, then the smallest id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueItem {
    priority: i32,
    id: JobId,
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.cmp(&other.priority).then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Colour of a job during the depth-first cycle search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Grey,
    Black,
}

/// Returns one cycle path (`first == last`) over the precedence edges, if one exists.
fn detect_cycle(jobs: &BTreeMap<JobId, Job>, edges: &[Edge]) -> Option<Vec<JobId>> {
    let mut adjacency: BTreeMap<JobId, BTreeSet<JobId>> = jobs.keys().map(|id| (id.clone(), BTreeSet::new())).collect();
    for edge in edges {
        let Some((from, to)) = precedence(edge) else {
            continue;
        };
        if let Some(next) = adjacency.get_mut(from) {
            next.insert(to.clone());
        }
    }

    let mut color: BTreeMap<JobId, Color> = jobs.keys().map(|id| (id.clone(), Color::White)).collect();
    let mut stack = Vec::new();
    for id in jobs.keys() {
        if color.get(id) == Some(&Color::White) {
            if let Some(path) = depth_first(id, &adjacency, &mut color, &mut stack) {
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
                    // invariant: a grey node is on the current stack
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
