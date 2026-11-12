//! The server-sent event streams: one per session and one for the board snapshot.
//!
//! Every frame is written as it becomes available; the log or board is polled in short
//! blocking batches on `spawn_blocking`, so no store or log handle is held for the lifetime
//! of the response. One session poll reads at most [`STREAM_BATCH`] events, which keeps the
//! per-client buffer bounded even when the writer is far ahead of the reader. A client that
//! outlives a compacted, removed or truncated log receives one `resync` frame with a
//! recoverable cursor and the stream closes. The board stream sends snapshot frames and
//! heartbeats only; it has no cursor and never holds a store between polls.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, Sse};
use futures_util::Stream;
use futures_util::stream;
use serde_json::json;
use tokio::time::{Instant, sleep};
use vowp_protocol::api::Board;
use vowp_protocol::{EventEnvelope, SessionId, Timestamp};
use vowp_store::NodePaths;
use vowp_store::session_log::SessionLog;

use crate::app::ApiState;

/// Production interval between heartbeat frames.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Production interval between session-log polls.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Production interval between board snapshot polls.
pub(crate) const BOARD_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Most events one poll may hand to the frame stream.
pub(crate) const STREAM_BATCH: usize = 256;

/// One poll of the session log.
enum Batch {
    /// Events after the cursor, in sequence order.
    Events(Vec<EventEnvelope>),
    /// The cursor can no longer be served; reconnect after `after_seq`.
    Resync { after_seq: u64 },
}

/// Builds the SSE response for one session cursor.
pub(crate) fn response(
    state: ApiState,
    id: SessionId,
    after_seq: u64,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let cursor = StreamState {
        paths: state.paths().clone(),
        id,
        after_seq,
        pending: VecDeque::new(),
        poll_interval: state.stream_poll_interval(),
        heartbeat_interval: state.stream_heartbeat_interval(),
        next_heartbeat: Instant::now() + state.stream_heartbeat_interval(),
        poll_due: true,
        done: false,
    };
    Sse::new(stream::unfold(cursor, next_frame))
}

/// State carried between SSE frames.
struct StreamState {
    paths: NodePaths,
    id: SessionId,
    after_seq: u64,
    pending: VecDeque<EventEnvelope>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    poll_due: bool,
    done: bool,
}

/// Produces the next frame, sleeping or polling only when no ready frame exists.
async fn next_frame(mut state: StreamState) -> Option<(Result<Event, Infallible>, StreamState)> {
    loop {
        if state.done {
            return None;
        }

        let now = Instant::now();
        if now >= state.next_heartbeat {
            state.next_heartbeat = now + state.heartbeat_interval;
            return Some((Ok(heartbeat_event()), state));
        }
        if let Some(envelope) = state.pending.pop_front() {
            state.after_seq = envelope.seq;
            return match session_event(&envelope) {
                Some(event) => Some((Ok(event), state)),
                None => {
                    state.done = true;
                    Some((Ok(resync_event(state.after_seq)), state))
                }
            };
        }
        if !state.poll_due {
            let wait = state
                .poll_interval
                .min(state.next_heartbeat.saturating_duration_since(now));
            sleep(wait).await;
            state.poll_due = true;
            continue;
        }

        state.poll_due = false;
        let paths = state.paths.clone();
        let id = state.id.clone();
        let after_seq = state.after_seq;
        match tokio::task::spawn_blocking(move || read_batch(&paths, &id, after_seq, STREAM_BATCH)).await {
            Ok(Batch::Events(events)) => {
                // A full batch may have more behind it; poll again without sleeping once it
                // has drained. The queue itself never exceeds one batch.
                state.poll_due = events.len() == STREAM_BATCH;
                state.pending = events.into();
            }
            Ok(Batch::Resync { after_seq }) => {
                state.done = true;
                return Some((Ok(resync_event(after_seq)), state));
            }
            Err(_) => {
                state.done = true;
                return Some((Ok(resync_event(state.after_seq)), state));
            }
        }
    }
}

/// Reads the next bounded batch from one session log.
///
/// The reader is opened per poll and dropped before the batch is returned. A log that was
/// truncated behind the cursor, a gap at the front of the log, a compacted-away log or any
/// read error is reported as a resync at the newest sequence still readable before the gap,
/// falling back to the client's cursor when nothing is readable.
fn read_batch(paths: &NodePaths, id: &SessionId, after_seq: u64, limit: usize) -> Batch {
    let Ok(reader) = SessionLog::open_read(paths, id) else {
        return Batch::Resync { after_seq };
    };
    let Ok(iter) = reader.iter() else {
        return Batch::Resync { after_seq };
    };

    let mut first_seq = None;
    let mut last_seq = 0_u64;
    let mut events = Vec::new();
    for item in iter {
        let Ok(indexed) = item else {
            return Batch::Resync { after_seq };
        };
        let seq = indexed.envelope.seq;
        if first_seq.is_none() {
            first_seq = Some(seq);
        }
        last_seq = seq;
        if seq > after_seq {
            events.push(indexed.envelope);
            if events.len() >= limit {
                break;
            }
        }
    }

    if events.is_empty() {
        if last_seq < after_seq {
            return Batch::Resync { after_seq: last_seq };
        }
        if let Some(first_seq) = first_seq {
            if first_seq > after_seq.saturating_add(1) {
                return Batch::Resync {
                    after_seq: first_seq.saturating_sub(1),
                };
            }
        }
        return Batch::Events(events);
    }

    let next_seq = events[0].seq;
    if next_seq > after_seq.saturating_add(1) {
        return Batch::Resync {
            after_seq: next_seq.saturating_sub(1),
        };
    }
    Batch::Events(events)
}

/// Encodes one stored envelope as a `session` frame.
fn session_event(envelope: &EventEnvelope) -> Option<Event> {
    serde_json::to_string(envelope)
        .ok()
        .map(|data| Event::default().event("session").data(data))
}

/// Builds one heartbeat frame with the current wall-clock time.
fn heartbeat_event() -> Event {
    let at = Timestamp::now().as_rfc3339();
    Event::default()
        .event("heartbeat")
        .data(json!({ "at": at }).to_string())
}

/// Builds the single recovery frame sent before the stream closes.
fn resync_event(after_seq: u64) -> Event {
    Event::default()
        .event("resync")
        .data(json!({ "after_seq": after_seq }).to_string())
}

/// Builds the SSE response for the board snapshot stream.
///
/// The first poll always sends one `board` frame. Later polls send a frame only when the
/// snapshot changed apart from its wall-clock `at`, and a heartbeat keeps the connection
/// visibly alive while the board is stable.
pub(crate) fn board_response(state: ApiState) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let poll_interval = state.board_poll_interval();
    let heartbeat_interval = state.stream_heartbeat_interval();
    let cursor = BoardStreamState {
        state,
        last_board: None,
        poll_interval,
        heartbeat_interval,
        next_heartbeat: Instant::now() + heartbeat_interval,
        poll_due: true,
        done: false,
    };
    Sse::new(stream::unfold(cursor, next_board_frame))
}

/// State carried between board SSE frames.
struct BoardStreamState {
    state: ApiState,
    last_board: Option<Board>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    poll_due: bool,
    done: bool,
}

/// Produces the next board frame, sleeping or polling only when no ready frame exists.
async fn next_board_frame(mut state: BoardStreamState) -> Option<(Result<Event, Infallible>, BoardStreamState)> {
    loop {
        if state.done {
            return None;
        }

        let now = Instant::now();
        if now >= state.next_heartbeat {
            state.next_heartbeat = now + state.heartbeat_interval;
            return Some((Ok(heartbeat_event()), state));
        }
        if !state.poll_due {
            let wait = state
                .poll_interval
                .min(state.next_heartbeat.saturating_duration_since(now));
            sleep(wait).await;
            state.poll_due = true;
            continue;
        }

        state.poll_due = false;
        let api = state.state.clone();
        match tokio::task::spawn_blocking(move || api.board()).await {
            Ok(Ok(board)) => {
                if state.last_board.as_ref().is_none_or(|last| board_changed(last, &board)) {
                    state.last_board = Some(board.clone());
                    let Some(event) = board_event(&board) else {
                        state.done = true;
                        return None;
                    };
                    return Some((Ok(event), state));
                }
            }
            // A board collect error ends the stream cleanly; the page reconnects and falls
            // back to its five-second polling until a stream opens again.
            _ => {
                state.done = true;
                return None;
            }
        }
    }
}

/// Returns whether `current` differs from `previous` apart from the snapshot time.
///
/// The board's `at` moves on every collect. It is liveness, not content: the heartbeat
/// carries liveness without replacing the board, so a moving `at` alone sends no frame.
fn board_changed(previous: &Board, current: &Board) -> bool {
    let mut previous = previous.clone();
    previous.at = current.at;
    previous != *current
}

/// Encodes one board snapshot as a `board` frame.
fn board_event(board: &Board) -> Option<Event> {
    serde_json::to_string(board)
        .ok()
        .map(|data| Event::default().event("board").data(data))
}
