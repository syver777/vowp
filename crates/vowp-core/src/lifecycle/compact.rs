//! Closed-session compaction: replace the full log and blobs with a capsule and a summary.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10.2. The session index rows stay; only the
//! large files go. A capsule file and row remain authoritative history, and `summary.md` makes a
//! compacted session readable without the log.

use std::path::Path;

use vowp_protocol::{
    CAPSULE_VERSION, Capsule, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, EventType, Executor,
    ExecutorRef, HarnessKind, JobState, SessionId, SessionRole, SessionState, Timestamp,
};
use vowp_store::Store;
use vowp_store::session_files::write_capsule_file;

use crate::CoreError;
use crate::deps::Deps;

/// The result of compacting one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Session that was compacted.
    pub session: Option<SessionId>,
    /// Bytes freed by removing the full log and blobs.
    pub bytes_freed: u64,
    /// Capsule that now carries the history.
    pub capsule_id: Option<CapsuleId>,
    /// Whether the session was skipped (not closed, already compacted, or open work).
    pub skipped: bool,
}

/// Counts and turn bounds collected from the session event index.
#[derive(Debug, Clone, Copy, Default)]
struct EventStats {
    events: u64,
    first_event: Option<Timestamp>,
    last_event: Option<Timestamp>,
    first_turn: Option<Timestamp>,
    last_turn: Option<Timestamp>,
    turns: u32,
}

/// Replaces a closed session's log with its capsule and summary.
///
/// The session must be `closed`, or its job must be `done`; orchestrator sessions are never
/// compacted. A session whose log is already gone, or whose `compacted_at` is set, is skipped.
/// Passing gate-evidence blobs are kept even when the rest of the blob directory is removed.
pub fn compact_session(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    now: Timestamp,
) -> Result<CompactReport, CoreError> {
    let row = store.require_session(session)?;
    if row.role == Some(SessionRole::Orchestrator) {
        return Ok(CompactReport {
            session: Some(session.clone()),
            skipped: true,
            ..CompactReport::default()
        });
    }
    if store.lifecycle_compacted_at(session)?.is_some() {
        return Ok(CompactReport {
            session: Some(session.clone()),
            capsule_id: row.last_capsule_id.clone(),
            skipped: true,
            ..CompactReport::default()
        });
    }

    let log = deps.paths.log_path(session);
    if !log.is_file() {
        return Ok(CompactReport {
            session: Some(session.clone()),
            capsule_id: row.last_capsule_id.clone(),
            skipped: true,
            ..CompactReport::default()
        });
    }

    let job_done = match &row.job_id {
        Some(job_id) => match store.job(job_id) {
            Ok(job) => job.is_some_and(|job| job.state == JobState::Done),
            Err(vowp_store::StoreError::NoProject) => false,
            Err(error) => return Err(error.into()),
        },
        None => false,
    };
    if row.state != SessionState::Closed && !job_done {
        return Ok(CompactReport {
            session: Some(session.clone()),
            capsule_id: row.last_capsule_id.clone(),
            skipped: true,
            ..CompactReport::default()
        });
    }

    let stats = event_stats(store, session)?;
    let existing = store.latest_capsule(session)?;
    let capsule = match existing {
        Some(capsule) => capsule,
        None => {
            let capsule = final_capsule(&row, stats, now)?;
            let path = write_capsule_file(&deps.paths, &capsule)?;
            store.insert_capsule(&capsule, &path.display().to_string())?;
            capsule
        }
    };
    let capsule_id = capsule.capsule_id.clone();

    let summary = render_summary(&row.id, &row.title, &capsule, stats, now);
    let summary_path = deps.paths.session_dir(session).join("summary.md");
    write_summary(&summary_path, summary)?;

    let mut bytes_freed = remove_blobs(deps, store, session)?;
    bytes_freed = bytes_freed.saturating_add(remove_file_if_present(&log)?);
    let lock = deps.paths.lock_path(session);
    // The lock is best effort: a missing or already-removed lock must not stop compaction.
    let _ = remove_file_if_present(&lock);

    store.set_session_compacted(session, now)?;
    Ok(CompactReport {
        session: Some(session.clone()),
        bytes_freed,
        capsule_id: Some(capsule_id),
        skipped: false,
    })
}

/// Reads the event index in pages and collects count and turn bounds.
fn event_stats(store: &Store, session: &SessionId) -> Result<EventStats, CoreError> {
    let mut stats = EventStats::default();
    let mut after = 0_u64;
    loop {
        let rows = store.event_rows(session, after, 1_000)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            stats.events = stats.events.saturating_add(1);
            stats.first_event = Some(stats.first_event.map_or(row.ts, |first| first.min(row.ts)));
            stats.last_event = Some(stats.last_event.map_or(row.ts, |last| last.max(row.ts)));
            if row.event_type == EventType::TurnStarted {
                stats.first_turn = Some(stats.first_turn.map_or(row.ts, |first| first.min(row.ts)));
            }
            if row.event_type == EventType::TurnEnded {
                stats.last_turn = Some(stats.last_turn.map_or(row.ts, |last| last.max(row.ts)));
                stats.turns = stats.turns.saturating_add(1);
            }
            after = row.seq;
        }
    }
    Ok(stats)
}

/// Builds the final `job_end` capsule when the session has none.
fn final_capsule(row: &vowp_protocol::Session, stats: EventStats, now: Timestamp) -> Result<Capsule, CoreError> {
    let executor = row.executor.clone().unwrap_or_else(|| Executor {
        harness: HarnessKind::Vowp,
        model: "gc".to_owned(),
        effort: None,
        variant: None,
    });
    let capsule = Capsule {
        version: CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: row.id.clone(),
        created_at: now,
        at_seq: row.last_seq.max(1),
        trigger: CapsuleTrigger::JobEnd,
        from: ExecutorRef {
            executor,
            node: Some(row.node.clone()),
        },
        intent: if row.title.trim().is_empty() {
            format!("Session {} (closed and compacted by gc)", row.id)
        } else {
            row.title.clone()
        },
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState {
            done: vec!["Session closed; gc compacted the full log and blobs.".to_owned()],
            ..CapsuleState::default()
        },
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry {
            turns: stats.turns,
            ..CapsuleTelemetry::default()
        },
    };
    capsule.validate()?;
    Ok(capsule)
}

/// Writes `summary.md` through a temporary file and an atomic rename.
fn write_summary(path: &Path, text: String) -> Result<(), CoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "summary.md".to_owned());
    let tmp = parent.join(format!(".{name}.tmp-{}", ulid::Ulid::generate()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp).map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        file.sync_all().map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        std::fs::rename(&tmp, path).map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Deletes a file, returning the bytes freed; a missing file is zero.
fn remove_file_if_present(path: &Path) -> Result<u64, CoreError> {
    let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(CoreError::Io {
            path: path.display().to_string(),
            source: error,
        }),
    }
}

/// Removes the session's blobs except those referenced by passing gate evidence.
fn remove_blobs(deps: &Deps, store: &Store, session: &SessionId) -> Result<u64, CoreError> {
    let protected: std::collections::BTreeSet<String> = store
        .lifecycle_passing_gate_blobs()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let dir = deps.paths.blobs_dir(session);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut freed = 0_u64;
    let mut remaining = 0_u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Err(CoreError::Io {
                    path: dir.display().to_string(),
                    source: error,
                });
            }
        };
        let path = entry.path();
        let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
        if !is_file {
            continue;
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if protected.contains(name) {
            remaining = remaining.saturating_add(1);
            continue;
        }
        freed = freed.saturating_add(remove_file_if_present(&path)?);
    }
    if remaining == 0 {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(freed)
}

/// Renders `summary.md`: capsule markdown plus turn bounds and event count.
fn render_summary(session: &SessionId, title: &str, capsule: &Capsule, stats: EventStats, now: Timestamp) -> String {
    let title = if title.trim().is_empty() {
        format!("Session {session}")
    } else {
        title.to_owned()
    };
    let first = stats
        .first_turn
        .or(stats.first_event)
        .map(|ts| ts.as_rfc3339())
        .unwrap_or_else(|| "none recorded".to_owned());
    let last = stats
        .last_turn
        .or(stats.last_event)
        .map(|ts| ts.as_rfc3339())
        .unwrap_or_else(|| "none recorded".to_owned());
    format!(
        "# {title}\n\n\
         - Session: `{session}`\n\
         - Compacted: {}\n\
         - First turn: {first}\n\
         - Last turn: {last}\n\
         - Events: {}\n\
         - Capsule: `{}`\n\n\
         {}",
        now.as_rfc3339(),
        stats.events,
        capsule.capsule_id,
        capsule.render_markdown()
    )
}
