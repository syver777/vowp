//! The node snapshot: build, write, read and the four node hooks.
//!
//! The snapshot is derived: deleting it loses nothing. It is written atomically every node
//! tick (`tmp/snapshot.json.tmp`, fsync, rename) and read by `vowp fleet`, `vowp status` and
//! the board. Fleet reads open each project database read-only in turn; no process opens
//! SQLite over the network.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §5 and §13.2.

mod cards;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use vowp_adapters::proc::process_alive;
use vowp_protocol::{
    CapacitySnapshot, FleetQueueRow, InboxItem, InboxState, InboxSummary, OrchestratorCard, ProjectName,
    RunningCapacity, SNAPSHOT_FORMAT, SNAPSHOT_VERSION, SessionSurfaceCard, Snapshot, SupervisorSnapshot, Timestamp,
};
use vowp_store::{NodePaths, SessionFilter, Store, StoreError};

use crate::CoreError;
use crate::deps::Deps;
use crate::vault::Redactor;

/// One gibibyte, the unit of the spec's capacity defaults.
const GIB: u64 = 1024 * 1024 * 1024;

/// Builds the derived snapshot from the node's state.
///
/// Every registered project becomes a card, including archived ones; the reserved `_node`
/// project is not a card. A project whose database cannot be read becomes a paused card with
/// the error as its reason, so one broken project never hides the rest of the fleet.
pub fn build_snapshot(deps: &Deps, store: &Store, now: Timestamp) -> Result<Snapshot, CoreError> {
    let redactor = Redactor::load(deps)?;
    build_snapshot_with_redactor(deps, store, now, &redactor)
}

/// Builds the derived snapshot with a redactor the caller already loaded.
fn build_snapshot_with_redactor(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    redactor: &Redactor,
) -> Result<Snapshot, CoreError> {
    let global_owned = match store.paths() {
        Some(paths) => Some(Store::open_read_only(paths, None)?),
        None => None,
    };
    let global = global_owned.as_ref().unwrap_or(store);
    let projects = global.list_project_entries()?;

    let mut cards = Vec::new();
    let mut project_inbox = Vec::new();
    let mut running_runs = 0_u32;
    let mut preferred_fail_streak = None;
    let mut any_fail_streak = None;
    for project in projects
        .iter()
        .filter(|project| project.name.as_str() != vowp_store::RESERVED_NODE_PROJECT)
    {
        let read = if let Some(paths) = store.paths() {
            match Store::open_read_only(paths, Some(&project.name)) {
                Ok(project_store) => cards::project_read(deps, redactor, &project_store, project, now),
                Err(error) => Err(CoreError::Store(error)),
            }
        } else if store.bound_project() == Some(&project.name) {
            cards::project_read(deps, redactor, store, project, now)
        } else {
            Err(CoreError::Invalid(format!(
                "project {} has no open database",
                project.name
            )))
        };
        match read {
            Ok(read) => {
                running_runs = running_runs.saturating_add(read.card.running.len() as u32);
                any_fail_streak.get_or_insert(read.fail_streak);
                if project.name == deps.config.project.name {
                    preferred_fail_streak = Some(read.fail_streak);
                }
                project_inbox.extend(read.inbox);
                cards.push(read.card);
            }
            Err(error) => cards.push(cards::broken_card(project, &error)),
        }
    }

    let mut inbox_items = global.list_inbox(Some(InboxState::Open))?;
    inbox_items.extend(project_inbox);
    let inbox_items = merge_inbox(inbox_items);
    let inbox = inbox_items
        .iter()
        .take(100)
        .map(|item| InboxSummary {
            id: item.id.clone(),
            project: item.project.clone(),
            job_id: item.job_id.clone(),
            kind: item.kind,
            title: full_title(redactor, &item.title),
            created_at: item.created_at,
            age_secs: now.since(item.created_at).as_secs(),
        })
        .collect();

    let sequence = snapshot_sequence(global)?;
    let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    let fail_streak = node_fail_streak(store, &node_project)?
        .or(preferred_fail_streak)
        .or(any_fail_streak)
        .unwrap_or(0);
    let supervisor = supervisor_snapshot(deps, global, fail_streak)?;
    let limits = global.active_limits(now)?;
    let sessions = session_cards(global, now, redactor)?;
    let orchestrators = orchestrator_cards(deps.paths.root());
    let target = crate::capacity::targets::current_for(deps, store, now)?;
    let capacity = CapacitySnapshot {
        measured: None,
        running: RunningCapacity {
            runs: running_runs,
            ..RunningCapacity::default()
        },
        advised_target: target.advised_workers,
        target,
        brake: crate::capacity::brakes::evaluate_with_config(deps, store, now)?,
        reserve_ram_bytes: GIB,
        min_disk_bytes: 5 * GIB,
    };

    Ok(Snapshot {
        schema: SNAPSHOT_FORMAT.to_owned(),
        version: SNAPSHOT_VERSION,
        sequence,
        generated_at: now,
        home: deps.node.clone(),
        supervisor,
        projects: cards,
        capacity,
        limits,
        inbox,
        sessions,
        orchestrators,
    })
}

/// Writes `snapshot.json` atomically and returns after the rename.
///
/// The sequence is read from `kv['snapshot_sequence']`, incremented and stored before the
/// temporary file is renamed, so a crash can leave a gap but never repeats a number.
pub fn write_snapshot(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError> {
    let redactor = Redactor::load(deps)?;
    let mut snapshot = build_snapshot_with_redactor(deps, store, now, &redactor)?;
    let sequence = next_sequence(store)?;
    snapshot.sequence = sequence;

    // Redact the structured document rather than its serialized text: a value containing
    // quotes, backslashes or newlines is JSON-escaped in the file and would not match as
    // an exact substring there. `redact_json_strings` returns before walking the tree when
    // no secret is configured, so a large snapshot costs nothing extra on every tick.
    let mut value = serde_json::to_value(&snapshot).map_err(|error| CoreError::Invalid(error.to_string()))?;
    redact_json_strings(&redactor, &mut value);
    let text = serde_json::to_string_pretty(&value).map_err(|error| CoreError::Invalid(error.to_string()))?;
    let tmp_dir = deps.paths.tmp_dir();
    std::fs::create_dir_all(&tmp_dir).map_err(|source| CoreError::Io {
        path: tmp_dir.display().to_string(),
        source,
    })?;
    let tmp = tmp_dir.join("snapshot.json.tmp");
    let path = deps.paths.root().join("snapshot.json");
    let mut file = File::create(&tmp).map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    file.write_all(text.as_bytes()).map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    file.write_all(b"\n").map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    file.sync_all().map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    drop(file);
    std::fs::rename(&tmp, &path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Reads `snapshot.json`, rejecting a document this build cannot understand.
///
/// Unknown fields inside a known version are ignored. A newer `version`, a different
/// `schema` marker or malformed JSON is an error; a missing file is [`CoreError::NotFound`].
pub fn read_snapshot(paths: &NodePaths) -> Result<Snapshot, CoreError> {
    let path = snapshot_path(paths);
    let text = std::fs::read_to_string(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            CoreError::NotFound(path.display().to_string())
        } else {
            CoreError::Io {
                path: path.display().to_string(),
                source,
            }
        }
    })?;
    let snapshot: Snapshot =
        serde_json::from_str(&text).map_err(|error| CoreError::Invalid(format!("snapshot.json: {error}")))?;
    if snapshot.schema != SNAPSHOT_FORMAT {
        return Err(CoreError::Invalid(format!(
            "snapshot schema {:?}, expected {SNAPSHOT_FORMAT:?}",
            snapshot.schema
        )));
    }
    if snapshot.version > SNAPSHOT_VERSION {
        return Err(CoreError::Invalid(format!(
            "snapshot version {} is newer than supported version {SNAPSHOT_VERSION}",
            snapshot.version
        )));
    }
    Ok(snapshot)
}

/// Runs the four node-level hooks in the tick order.
pub fn node_tick(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError> {
    let _ = crate::capacity::CapacityManager::new(deps.clone()).sample(store, now)?;
    let _ = crate::surface::sweep_leases(deps, store, now)?;
    crate::lifecycle::tick(deps, store, now)?;
    write_snapshot(deps, store, now)
}

/// Returns whether a snapshot is younger than three supervisor ticks.
pub fn snapshot_is_fresh(snapshot: &Snapshot, now: Timestamp, tick_secs: u64) -> bool {
    let window = i64::try_from(tick_secs.max(1)).unwrap_or(i64::MAX).saturating_mul(3);
    now.unix_secs().saturating_sub(snapshot.generated_at.unix_secs()) <= window
}

/// Flattens the snapshot's per-project queue cards into fleet rows.
pub fn queue_rows(snapshot: &Snapshot) -> Vec<FleetQueueRow> {
    let mut rows = Vec::new();
    for project in &snapshot.projects {
        for job in &project.queue {
            rows.push(FleetQueueRow {
                project: project.name.clone(),
                job: job.job.clone(),
                title: job.title.clone(),
                kind: job.kind,
                role: job.role,
                priority: job.priority,
                state: job.state,
                age_secs: job.age_secs,
                waiting_reason: job.waiting_reason.clone(),
            });
        }
    }
    rows
}

/// Lists the registered, non-reserved project names in registry order.
pub fn project_names(store: &Store) -> Result<Vec<ProjectName>, CoreError> {
    Ok(store
        .list_projects()?
        .into_iter()
        .filter(|project| project.name.as_str() != vowp_store::RESERVED_NODE_PROJECT)
        .map(|project| project.name)
        .collect())
}

/// Merges node-wide and per-project inbox rows, oldest first.
///
/// With `include_answered` false only open items are returned; otherwise every state is
/// included. Each database is opened read-only in turn, never two at once.
pub fn collect_inbox(store: &Store, include_answered: bool) -> Result<Vec<InboxItem>, CoreError> {
    let global_owned = match store.paths() {
        Some(paths) => Some(Store::open_read_only(paths, None)?),
        None => None,
    };
    let global = global_owned.as_ref().unwrap_or(store);
    let state = (!include_answered).then_some(InboxState::Open);
    let mut items = global.list_inbox(state)?;
    if let Some(paths) = store.paths() {
        for project in project_names(global)? {
            match Store::open_read_only(paths, Some(&project)) {
                Ok(project_store) => items.extend(project_store.list_inbox(state)?),
                Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(CoreError::Store(error)),
            }
        }
    } else if store.bound_project().is_some() {
        items.extend(store.list_inbox(state)?);
    }
    Ok(merge_inbox(items))
}

/// Returns the path of the snapshot file.
fn snapshot_path(paths: &NodePaths) -> std::path::PathBuf {
    paths.root().join("snapshot.json")
}

/// Reads the stored sequence, or zero on a fresh home.
fn snapshot_sequence(store: &Store) -> Result<u64, CoreError> {
    Ok(store
        .kv_get("snapshot_sequence")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

/// Reads and increments the stored sequence.
fn next_sequence(store: &Store) -> Result<u64, CoreError> {
    let sequence = snapshot_sequence(store)?.saturating_add(1);
    store.kv_set("snapshot_sequence", &sequence.to_string())?;
    Ok(sequence)
}

/// Builds the supervisor card from the lock, heartbeat and pause marker.
fn supervisor_snapshot(deps: &Deps, store: &Store, fail_streak: u32) -> Result<SupervisorSnapshot, CoreError> {
    let lock = std::fs::read_to_string(deps.paths.root().join("supervisor.lock")).ok();
    let pid = lock
        .as_deref()
        .and_then(|text| text.lines().next())
        .and_then(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| process_alive(*pid));
    let started_at = if pid.is_some() {
        lock.as_deref()
            .and_then(|text| text.lines().nth(1))
            .and_then(|line| Timestamp::parse(line.trim()).ok())
    } else {
        None
    };
    let paused = store.kv_get("paused")?.as_deref() == Some("1");
    Ok(SupervisorSnapshot {
        running: pid.is_some(),
        pid,
        started_at,
        last_tick: read_heartbeat(&deps.paths.root().join("heartbeat")),
        tick_secs: deps.config.node.tick_secs.max(1),
        paused,
        fail_streak,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

/// Reads the `_node` supervisor loop's fail streak when it exists.
///
/// Until the node supervisor exists this is `None`; the caller then uses the current
/// project's `l1-supervisor` row (or the first readable project's row).
fn node_fail_streak(store: &Store, node_project: &ProjectName) -> Result<Option<u32>, CoreError> {
    let loop_ = if let Some(paths) = store.paths() {
        match Store::open_read_only(paths, Some(node_project)) {
            Ok(node_store) => node_store.loop_("l1-supervisor")?,
            Err(StoreError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(CoreError::Store(error)),
        }
    } else if store.bound_project() == Some(node_project) {
        store.loop_("l1-supervisor")?
    } else {
        return Ok(None);
    };
    Ok(loop_.map(|loop_| loop_.fail_streak))
}

/// Reads the heartbeat's first line as Unix seconds.
fn read_heartbeat(path: &Path) -> Option<Timestamp> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    if lines.any(|line| line.trim() == "stopped") {
        return None;
    }
    first.parse::<i64>().ok().map(Timestamp::from_unix_secs)
}

/// Builds the recent-session cards.
fn session_cards(store: &Store, now: Timestamp, redactor: &Redactor) -> Result<Vec<SessionSurfaceCard>, CoreError> {
    let cutoff = now.minus(Duration::from_secs(7 * 86_400));
    let mut sessions = store.list_sessions(&SessionFilter::default())?;
    sessions.retain(|session| session.updated_at >= cutoff);
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    sessions.truncate(200);
    Ok(sessions
        .into_iter()
        .map(|session| SessionSurfaceCard {
            id: session.id,
            project: session.project,
            job_id: session.job_id,
            role: session.role,
            title: full_title(redactor, &session.title),
            state: session.state,
            executor: session.executor.as_ref().map(ToString::to_string),
            node: session.node,
            active_surface: None,
            last_event_at: session.updated_at,
        })
        .collect())
}

/// Reads `orchestrators/<name>/orchestrator.json` cards, sorted by name.
fn orchestrator_cards(root: &Path) -> Vec<OrchestratorCard> {
    let dir = root.join("orchestrators");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join("orchestrator.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(card) = serde_json::from_str::<OrchestratorCard>(&text) {
            cards.push(card);
        }
    }
    cards.sort_by(|left, right| left.name.cmp(&right.name));
    cards
}

/// Deduplicates inbox rows by id and sorts them oldest first.
fn merge_inbox(items: Vec<InboxItem>) -> Vec<InboxItem> {
    let mut by_id: BTreeMap<String, InboxItem> = BTreeMap::new();
    for item in items {
        by_id.insert(item.id.as_str().to_owned(), item);
    }
    let mut items: Vec<InboxItem> = by_id.into_values().collect();
    items.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    items
}

/// Redacts every string inside one structured JSON value without touching object keys.
///
/// This covers snapshot fields the card builders copy verbatim (paused reasons, limits,
/// orchestrator cards) as well as the text they already summarize. Working on the value
/// tree means a secret containing JSON-special characters still matches its raw form.
///
/// Returns whether the tree was traversed; an empty redactor (no configured secrets)
/// returns immediately instead of cloning every string in the snapshot.
fn redact_json_strings(redactor: &Redactor, value: &mut serde_json::Value) -> bool {
    if redactor.is_empty() {
        return false;
    }
    redact_json_value(redactor, value);
    true
}

/// Recursively redacts the string values of one JSON value.
fn redact_json_value(redactor: &Redactor, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact(text),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(redactor, item);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                redact_json_value(redactor, item);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Redacts one title or question, keeping the full text.
///
/// The 200-character body cap does not apply to titles and questions.
fn full_title(redactor: &Redactor, text: &str) -> String {
    redactor.redact(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_redactor_skips_snapshot_string_traversal() {
        let redactor = Redactor::default();
        assert!(redactor.is_empty());
        let original = serde_json::json!({
            "title": "plain title",
            "nested": ["still here", {"count": 2}, true, null],
        });
        let mut value = original.clone();
        assert!(
            !redact_json_strings(&redactor, &mut value),
            "an empty redactor must not enter the traversal"
        );
        assert_eq!(value, original, "the document is left exactly as built");
    }
}
