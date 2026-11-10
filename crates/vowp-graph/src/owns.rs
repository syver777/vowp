//! Ownership globs: conservative overlap, per-path permission and running-job locks.
//! Spec: `docs/design/crates/vowp-graph.md` §4.
//!
//! Patterns are repo-relative and `/`-separated. [`owns_overlap`] is deliberately
//! conservative: it may report overlap for two patterns that cannot actually collide, but
//! never misses a real collision. [`path_allowed`] answers the narrower question asked by the
//! `owns_respected` gate: may this concrete path be written?

use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobSetBuilder};
use vowp_protocol::{Job, JobId, JobKind};

use crate::graph::Graph;

/// Characters that make a glob component non-literal.
const GLOB_META: [char; 6] = ['*', '?', '[', ']', '{', '}'];

/// Returns true when two `owns` sets may write the same path.
///
/// Each pattern is reduced to its literal prefix (the components before the first component
/// that contains a glob metacharacter). Two sets overlap when a prefix of one is an
/// ancestor-or-equal of a prefix of the other. Absolute patterns, patterns with a `..`
/// component and empty patterns have an empty prefix and therefore overlap everything. An
/// empty `owns` set never overlaps anything.
pub fn owns_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }

    a.iter().any(|left| {
        let left = literal_prefix(left);
        b.iter().any(|right| {
            let right = literal_prefix(right);
            prefixes_overlap(&left, &right)
        })
    })
}

/// Returns true when `path` is writable under `owns` or appears in `always_allowed`.
///
/// The path is normalised like an ownership pattern first; absolute paths and paths with a
/// `..` component are refused. Globs use `*`/`?` that do not cross `/` and `**` that does. A
/// pattern without a metacharacter (or with a trailing `/`) is also treated as a directory
/// and gets an implicit `/**` suffix. Invalid globs are ignored, matching nothing.
pub fn path_allowed(owns: &[String], always_allowed: &[String], path: &str) -> bool {
    let Some(path) = normalize_path(path) else {
        return false;
    };

    let mut builder = GlobSetBuilder::new();
    let mut patterns = 0;
    for pattern in owns.iter().chain(always_allowed.iter()) {
        let Some((normalized, trailing_slash)) = normalize_glob_pattern(pattern) else {
            continue;
        };
        if add_glob(&mut builder, &normalized) {
            patterns += 1;
        }
        if (!has_glob_meta(&normalized) || trailing_slash) && add_glob(&mut builder, &format!("{normalized}/**")) {
            patterns += 1;
        }
    }

    if patterns == 0 {
        return false;
    }
    match builder.build() {
        Ok(set) => set.is_match(path),
        Err(_) => false,
    }
}

/// Returns the components of `pattern` before the first component with a glob metacharacter.
///
/// Absolute patterns, patterns with a `..` component and empty patterns return an empty list.
pub fn literal_prefix(pattern: &str) -> Vec<String> {
    let Some(parts) = normalized_components(pattern) else {
        return Vec::new();
    };
    let mut prefix = Vec::new();
    for part in parts {
        if has_glob_meta(&part) {
            break;
        }
        prefix.push(part);
    }
    prefix
}

/// Adds one glob to the set, returning false when the pattern is invalid.
fn add_glob(builder: &mut GlobSetBuilder, pattern: &str) -> bool {
    match GlobBuilder::new(pattern).literal_separator(true).build() {
        Ok(glob) => {
            builder.add(glob);
            true
        }
        Err(_) => false,
    }
}

/// Returns true when one literal prefix is an ancestor-or-equal of the other.
fn prefixes_overlap(a: &[String], b: &[String]) -> bool {
    let shared = a.len().min(b.len());
    a[..shared] == b[..shared]
}

/// Returns true when a component contains any glob metacharacter.
fn has_glob_meta(component: &str) -> bool {
    component.chars().any(|c| GLOB_META.contains(&c))
}

/// Splits a pattern into its normalised components, or `None` when it is global.
///
/// A global pattern is absolute, empty or contains a `..` component.
fn normalized_components(pattern: &str) -> Option<Vec<String>> {
    let trimmed = pattern.trim();
    let mut rest = trimmed;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }

    let replaced = rest.replace('\\', "/");
    let collapsed = collapse_slashes(&replaced);
    let stripped = collapsed.trim_end_matches('/');
    if stripped.is_empty() || stripped.starts_with('/') {
        return None;
    }

    let parts: Vec<String> = stripped
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() || parts.iter().any(|part| part == "..") {
        return None;
    }
    Some(parts)
}

/// Normalises a glob pattern but keeps it relative/absolute as written.
///
/// Returns the pattern and whether it ended with `/` before that slash was stripped.
fn normalize_glob_pattern(pattern: &str) -> Option<(String, bool)> {
    let trimmed = pattern.trim();
    let mut rest = trimmed;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }

    let replaced = rest.replace('\\', "/");
    let collapsed = collapse_slashes(&replaced);
    let trailing_slash = collapsed.ends_with('/');
    let stripped = collapsed.trim_end_matches('/');
    if stripped.is_empty() {
        return None;
    }
    Some((stripped.to_owned(), trailing_slash))
}

/// Normalises a repo-relative file path, or returns `None` when it must be refused.
fn normalize_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    let mut rest = trimmed;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }

    let replaced = rest.replace('\\', "/");
    let collapsed = collapse_slashes(&replaced);
    let stripped = collapsed.trim_end_matches('/');
    if stripped.is_empty() || stripped.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = stripped.split('/').filter(|part| !part.is_empty()).collect();
    if parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Replaces runs of `/` with one slash, preserving a leading slash.
fn collapse_slashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut previous_slash = false;
    for ch in value.chars() {
        if ch == '/' {
            if !previous_slash {
                out.push('/');
            }
            previous_slash = true;
        } else {
            out.push(ch);
            previous_slash = false;
        }
    }
    out
}

/// The locks held by jobs whose state includes ownership.
#[derive(Debug, Default, Clone)]
pub struct OwnsLocks {
    /// Job id -> (its patterns, the fix parent when it is a fix/merge child).
    locks: BTreeMap<JobId, (Vec<String>, Option<JobId>)>,
}

impl OwnsLocks {
    /// Collects every job that currently holds a lock.
    pub fn from_running(jobs: &[Job], graph: &Graph) -> OwnsLocks {
        let mut locks = BTreeMap::new();
        for job in jobs.iter().filter(|job| job.state.holds_lock()) {
            locks.insert(job.id.clone(), (job.owns.clone(), fix_group(job, graph)));
        }
        Self { locks }
    }

    /// Tries to take `job`'s lock.
    ///
    /// Returns the ids of the holders it conflicts with without changing the lock table.
    /// A fix or merge job shares its `fixes` parent's group, so the two never conflict with
    /// each other.
    pub fn try_acquire(&mut self, job: &Job, graph: &Graph) -> Result<(), Vec<JobId>> {
        let conflicts = self.conflicts(job, graph);
        if conflicts.is_empty() {
            self.locks
                .insert(job.id.clone(), (job.owns.clone(), fix_group(job, graph)));
            Ok(())
        } else {
            Err(conflicts)
        }
    }

    /// Releases the lock held by `id`, if any.
    pub fn release(&mut self, id: &JobId) {
        self.locks.remove(id);
    }

    /// Returns the holders whose patterns overlap `job`'s patterns.
    pub fn conflicts(&self, job: &Job, graph: &Graph) -> Vec<JobId> {
        let group = group_of(job, graph);
        self.locks
            .iter()
            .filter(|(id, (owns, fix_parent))| {
                let holder_group = fix_parent.as_ref().unwrap_or(id);
                holder_group != &group && owns_overlap(&job.owns, owns)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Returns every lock holder in id order.
    pub fn holders(&self) -> Vec<JobId> {
        self.locks.keys().cloned().collect()
    }
}

/// Returns the fix parent of a fix/merge job, if the graph has one.
fn fix_group(job: &Job, graph: &Graph) -> Option<JobId> {
    if matches!(job.kind, JobKind::Fix | JobKind::Merge) {
        graph.fix_parent(&job.id).cloned()
    } else {
        None
    }
}

/// Returns the id of the lock group a job belongs to.
fn group_of(job: &Job, graph: &Graph) -> JobId {
    fix_group(job, graph).unwrap_or_else(|| job.id.clone())
}
