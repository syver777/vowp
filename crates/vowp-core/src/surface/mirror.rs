//! Transcript mirroring and session-resume context.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5. [`mirror_once`] reads the app's new
//! transcript entries and appends them to the canonical session log exactly once: the
//! `native_events` index maps an app-side id to the VOWP event written for it, so a repeated
//! pass skips it. The event index row and the map row commit in one database transaction; the
//! log append itself happens first and cannot join it. The mirror checks the map before
//! appending, and callers run one mirror thread per session, which makes that ordering safe.
//!
//! [`resume_context`] builds what the next app needs to continue: the session, its latest
//! capsule, the last turns and the current lease.

use serde::{Deserialize, Serialize};
use vowp_protocol::{
    Actor, AssistantMessage, AssistantReasoning, Capsule, Cost, CostSource, ErrorEvent, ErrorKind, EventBody,
    EventEnvelope, EventSource, EventType, HarnessKind, Lease, Note, OrchestratorApp, Session, SessionId, StopReason,
    Timestamp, ToolCall, ToolResult, TurnEnded, TurnStarted, UserMessage,
};
use vowp_store::Store;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};

use crate::CoreError;
use crate::deps::Deps;

use super::claude_remote::ClaudeRemoteHost;
use super::native::{AppHandle, AppHost, TranscriptEntry, TranscriptKind};
use super::opencode_host::OpencodeHost;

/// The incremental cursor of an app transcript mirror.
///
/// The caller persists it in `~/.vowp/orchestrators/<name>/mirror.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorCursor {
    /// Last app-side id mirrored.
    pub last_native_id: Option<String>,
    /// When the cursor last advanced.
    pub updated_at: Timestamp,
}

/// Loads the cursor from `mirror.json`, or starts from the beginning when it is absent.
pub fn read_cursor(path: &std::path::Path) -> Result<MirrorCursor, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| CoreError::Invalid(format!("mirror.json: {error}")))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MirrorCursor {
            last_native_id: None,
            updated_at: Timestamp::now(),
        }),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Writes the cursor to `mirror.json` with mode 0600.
pub fn write_cursor(path: &std::path::Path, cursor: &MirrorCursor) -> Result<(), CoreError> {
    super::native::write_private_json(path, cursor)
}

/// One mirror pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorReport {
    /// Events appended.
    pub events: u64,
    /// Native ids skipped as duplicates (or entries with no durable VOWP mapping).
    pub duplicates: u64,
    /// Cursor after the pass.
    pub cursor: MirrorCursor,
}

/// Mirrors app entries into the session log once.
///
/// The host is chosen from the handle's app kind, so a supervisor can mirror any app
/// without holding the host object. Tests use [`mirror_from_host`] with a fake host.
pub fn mirror_once(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    app: &AppHandle,
    cursor: &mut MirrorCursor,
) -> Result<MirrorReport, CoreError> {
    store.require_session(session)?;
    match app.kind {
        OrchestratorApp::Opencode => {
            let host = OpencodeHost::default();
            mirror_from_host(deps, store, session, &host, app, cursor)
        }
        OrchestratorApp::ClaudeCode => {
            let host = ClaudeRemoteHost::default();
            mirror_from_host(deps, store, session, &host, app, cursor)
        }
        OrchestratorApp::Chat => {
            let mut report = MirrorReport {
                events: 0,
                duplicates: 0,
                cursor: cursor.clone(),
            };
            report.cursor.updated_at = deps.clock.now();
            *cursor = report.cursor.clone();
            Ok(report)
        }
    }
}

/// Mirrors one explicit host's entries into the session log once.
///
/// This is the injectable core of [`mirror_once`]; tests pass a fake [`AppHost`].
pub fn mirror_from_host(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    host: &dyn AppHost,
    handle: &AppHandle,
    cursor: &mut MirrorCursor,
) -> Result<MirrorReport, CoreError> {
    store.require_session(session)?;
    let harness = harness_for(host.kind());
    let batch = host.transcript_since(handle, cursor)?;
    let mut report = MirrorReport {
        events: 0,
        duplicates: 0,
        cursor: cursor.clone(),
    };
    let mut turns = next_turn(store, session)?;
    let mut log: Option<SessionLog> = None;

    for entry in &batch.entries {
        report.cursor.last_native_id = Some(entry.native_id.clone());
        if store.native_event(session, harness, &entry.native_id)?.is_some() {
            report.duplicates += 1;
            continue;
        }
        let Some((body, actor)) = body_for(entry, &mut turns) else {
            report.duplicates += 1;
            continue;
        };
        let source = EventSource {
            harness,
            model: entry.model.clone(),
            node: None,
            native_session_id: entry.native_session.clone(),
        };
        let new = NewEvent {
            actor,
            source,
            body,
            raw: None,
            ts: Some(entry.ts),
        };
        if log.is_none() {
            log = Some(SessionLog::open(&deps.paths, session, deps.clock.clone())?);
        }
        // Redact exact values before the log line is written.
        let appended = log
            .as_ref()
            .ok_or_else(|| CoreError::Invalid("session log was not opened".to_owned()))?
            .append(crate::vault::redact_event(deps, new)?)?;
        let row = index_row(&appended);
        // The event index row and the native-id map row commit together. The log line is
        // already on disk; the map is what makes a later pass skip it.
        let inserted = store.transaction(|tx| {
            tx.index_event(&row)?;
            tx.insert_native_event(session, harness, &entry.native_id, &appended.envelope.id, entry.ts)
        })?;
        if !inserted {
            report.duplicates += 1;
            continue;
        }
        report.events += 1;
    }
    if let Some(log) = log {
        log.sync()?;
    }
    report.cursor.updated_at = deps.clock.now();
    *cursor = report.cursor.clone();
    Ok(report)
}

/// Everything a new surface needs to continue a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeContext {
    /// The session row.
    pub session: Session,
    /// Latest capsule, when any.
    pub capsule: Option<Capsule>,
    /// Tail events.
    pub tail: Vec<EventEnvelope>,
    /// Current lease, when one exists.
    pub lease: Option<Lease>,
}

/// Builds the resume context for a surface.
///
/// `tail_turns` is a count of turns, not events: the tail starts at the user message or
/// `turn.started` of the requested number of most recent turns. The tail is capped at 500
/// events and reasoning is dropped, matching the bundle export rule.
pub fn resume_context(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    tail_turns: u32,
) -> Result<ResumeContext, CoreError> {
    let session_row = store.require_session(session)?;
    let capsule = store.latest_capsule(session)?;
    let lease = store
        .lease(session)?
        .filter(|lease| deps.clock.now().since(lease.heartbeat_at) < super::lease::STALE_AFTER);
    let reader = SessionLog::open_read(&deps.paths, session)?;
    let events = reader.read_all()?;
    Ok(ResumeContext {
        session: session_row,
        capsule,
        tail: select_tail(events, tail_turns),
        lease,
    })
}

/// Maps a configured app kind to the harness name stored in the event source.
fn harness_for(kind: OrchestratorApp) -> HarnessKind {
    match kind {
        OrchestratorApp::Opencode => HarnessKind::Opencode,
        OrchestratorApp::ClaudeCode => HarnessKind::ClaudeCode,
        OrchestratorApp::Chat => HarnessKind::Vowp,
    }
}

/// Counts `turn.started` rows so a mirrored turn gets the same number service-written turns use.
fn next_turn(store: &Store, session: &SessionId) -> Result<u32, CoreError> {
    let rows = store.event_rows(session, 0, u32::MAX)?;
    let count = rows
        .iter()
        .filter(|row| row.event_type == EventType::TurnStarted)
        .count();
    Ok(u32::try_from(count).unwrap_or(u32::MAX).saturating_add(1))
}

/// Converts one transcript entry into a canonical event plus its actor.
fn body_for(entry: &TranscriptEntry, turns: &mut u32) -> Option<(EventBody, Actor)> {
    let body = match entry.kind {
        TranscriptKind::UserMessage => (
            EventBody::UserMessage(UserMessage {
                text: entry.text.clone()?,
                attachments: Vec::new(),
                note: None,
            }),
            Actor::User,
        ),
        TranscriptKind::AssistantMessage => (
            EventBody::AssistantMessage(AssistantMessage {
                text: entry.text.clone()?,
            }),
            Actor::Assistant,
        ),
        TranscriptKind::Reasoning => (
            EventBody::AssistantReasoning(AssistantReasoning {
                text: entry.text.clone()?,
            }),
            Actor::Assistant,
        ),
        TranscriptKind::ToolCall => {
            let call_id = entry.call_id.clone().unwrap_or_else(|| entry.native_id.clone());
            (
                EventBody::ToolCall(ToolCall {
                    call_id,
                    name: entry.tool.clone().unwrap_or_else(|| "tool".to_owned()),
                    input: entry.input.clone().unwrap_or(serde_json::Value::Null),
                }),
                Actor::Assistant,
            )
        }
        TranscriptKind::ToolResult => {
            let call_id = entry.call_id.clone().unwrap_or_else(|| entry.native_id.clone());
            (
                EventBody::ToolResult(ToolResult {
                    call_id,
                    ok: entry.ok.unwrap_or(true),
                    output: entry.output.clone(),
                    output_ref: None,
                    exit_code: None,
                }),
                Actor::Tool,
            )
        }
        TranscriptKind::TurnStarted => {
            let turn = *turns;
            *turns = turns.saturating_add(1);
            (EventBody::TurnStarted(TurnStarted { turn }), Actor::System)
        }
        TranscriptKind::TurnEnded => {
            let turn = turns.saturating_sub(1).max(1);
            let stop_reason = match entry.text.as_deref() {
                Some("interrupted") => StopReason::Interrupted,
                Some("failed") => StopReason::Error,
                Some("crash") => StopReason::Crash,
                _ => StopReason::EndTurn,
            };
            (
                EventBody::TurnEnded(TurnEnded {
                    turn,
                    stop_reason,
                    duration_ms: None,
                }),
                Actor::System,
            )
        }
        TranscriptKind::Cost => {
            let tokens = entry.tokens.unwrap_or_default();
            let cost = match entry.usd {
                Some(usd) => Cost::receipt(usd, tokens),
                None => Cost {
                    usd: None,
                    tokens,
                    source: CostSource::Unknown,
                },
            };
            (
                EventBody::Cost(vowp_protocol::CostEvent { cost, run_id: None }),
                Actor::System,
            )
        }
        TranscriptKind::Error => (
            EventBody::Error(ErrorEvent {
                kind: ErrorKind::AdapterFailed,
                message: entry
                    .text
                    .clone()
                    .or_else(|| entry.output.clone())
                    .unwrap_or_else(|| "app reported an error".to_owned()),
                retryable: false,
            }),
            Actor::System,
        ),
        TranscriptKind::Note => (
            EventBody::Note(Note {
                text: entry
                    .text
                    .clone()
                    .or_else(|| entry.output.clone())
                    .or_else(|| entry.path.clone())
                    .unwrap_or_default(),
            }),
            Actor::System,
        ),
    };
    Some(body)
}

/// Takes the events belonging to the last `turns` turns.
fn select_tail(events: Vec<EventEnvelope>, turns: u32) -> Vec<EventEnvelope> {
    if turns == 0 {
        return Vec::new();
    }
    let requested = usize::try_from(turns).unwrap_or(usize::MAX);
    let mut markers: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_type == EventType::TurnStarted)
        .map(|(index, _)| index)
        .collect();
    if markers.is_empty() {
        markers = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_type == EventType::UserMessage)
            .map(|(index, _)| index)
            .collect();
    }
    let start = if markers.len() >= requested {
        markers[markers.len() - requested]
    } else {
        0
    };
    let mut tail: Vec<EventEnvelope> = events
        .into_iter()
        .skip(start)
        .filter(|event| event.event_type != EventType::AssistantReasoning)
        .collect();
    if tail.len() > 500 {
        tail.drain(0..tail.len() - 500);
    }
    tail
}
