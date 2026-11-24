//! The server-sent event streams: one per session, one for the board snapshot, one each for
//! the jobs list, job detail, cost report, inbox page and node page.
//!
//! Every frame is written as it becomes available; the log or snapshot is polled in short
//! blocking batches on `spawn_blocking`, so no store or log handle is held for the lifetime
//! of the response. One session poll reads at most [`STREAM_BATCH`] events, which keeps the
//! per-client buffer bounded even when the writer is far ahead of the reader. A client that
//! outlives a compacted, removed or truncated log receives one `resync` frame with a
//! recoverable cursor and the stream closes. The board, jobs, cost, inbox and nodes streams
//! send snapshot frames and heartbeats only; they have no cursor, send at most one snapshot
//! per poll and never hold a store between polls.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, Sse};
use futures_util::Stream;
use futures_util::stream;
use serde::Serialize;
use serde_json::json;
use tokio::time::{Instant, sleep};
use vowp_protocol::api::{Board, GateResultPage, JobPage, RunPage};
use vowp_protocol::{EventEnvelope, InboxState, Job, JobId, SessionId, Timestamp};
use vowp_store::session_log::SessionLog;
use vowp_store::{JobFilter, NodePaths};

use crate::app::{ApiFailure, ApiState};
use crate::filters::CostQuerySpec;

/// One complete job-detail snapshot streamed by `GET /api/v1/jobs/{id}/stream`.
///
/// The snapshot embeds the exact `Job` document and the exact `RunPage` and
/// `GateResultPage` documents returned by the job sub-resource reads, so the page can
/// replace all three views from one frame. It carries no wall-clock field: a frame is sent
/// only when the job, its newest-attempt gates or its runs actually change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct JobDetailPage {
    /// The job's exact stored row.
    pub(crate) job: Job,
    /// The job's runs in the store's order, oldest first.
    pub(crate) runs: RunPage,
    /// The newest attempt's gate results, ordered by gate index then row id.
    pub(crate) gates: GateResultPage,
}

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

/// Builds the SSE response for the filtered jobs-list snapshot stream.
///
/// The first poll always sends one `jobs` frame carrying the exact `JobPage` the matching
/// `GET /api/v1/jobs` returns. Later polls send a frame only when that page changes, and a
/// heartbeat keeps the connection visibly alive while the list is stable. There is no
/// cursor or resume: each frame is a self-contained page for the same filters.
pub(crate) fn jobs_response(
    state: ApiState,
    filter: JobFilter,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let poll_interval = state.board_poll_interval();
    let heartbeat_interval = state.stream_heartbeat_interval();
    let cursor = JobsStreamState {
        state,
        filter,
        last_jobs: None,
        poll_interval,
        heartbeat_interval,
        next_heartbeat: Instant::now() + heartbeat_interval,
        poll_due: true,
        done: false,
    };
    Sse::new(stream::unfold(cursor, next_jobs_frame))
}

/// State carried between jobs-list SSE frames.
struct JobsStreamState {
    state: ApiState,
    filter: JobFilter,
    last_jobs: Option<JobPage>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    poll_due: bool,
    done: bool,
}

/// Produces the next jobs-list frame, sleeping or polling only when no ready frame exists.
async fn next_jobs_frame(mut state: JobsStreamState) -> Option<(Result<Event, Infallible>, JobsStreamState)> {
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
        let filter = state.filter.clone();
        match tokio::task::spawn_blocking(move || api.jobs(&filter)).await {
            Ok(Ok(page)) => {
                if state.last_jobs.as_ref().is_none_or(|last| last != &page) {
                    state.last_jobs = Some(page.clone());
                    let Some(event) = jobs_event(&page) else {
                        state.done = true;
                        return None;
                    };
                    return Some((Ok(event), state));
                }
            }
            // A list read error ends the stream cleanly; the page reconnects and falls back
            // to its five-second polling until a stream opens again.
            _ => {
                state.done = true;
                return None;
            }
        }
    }
}

/// Encodes one jobs-list snapshot as a `jobs` frame.
fn jobs_event(page: &JobPage) -> Option<Event> {
    serde_json::to_string(page)
        .ok()
        .map(|data| Event::default().event("jobs").data(data))
}

/// Builds the SSE response for one job's detail snapshot stream.
///
/// The first poll always sends one `job` frame carrying a [`JobDetailPage`]. Later polls
/// send a frame only when the job row, its newest-attempt gate results or its runs change,
/// and a heartbeat keeps the connection visibly alive while the detail is stable. There is
/// no cursor or resume: each frame is a self-contained snapshot.
pub(crate) fn job_response(state: ApiState, id: JobId) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let poll_interval = state.board_poll_interval();
    let heartbeat_interval = state.stream_heartbeat_interval();
    let cursor = JobStreamState {
        state,
        id,
        last_job: None,
        poll_interval,
        heartbeat_interval,
        next_heartbeat: Instant::now() + heartbeat_interval,
        poll_due: true,
        done: false,
    };
    Sse::new(stream::unfold(cursor, next_job_frame))
}

/// State carried between job-detail SSE frames.
struct JobStreamState {
    state: ApiState,
    id: JobId,
    last_job: Option<JobDetailPage>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    poll_due: bool,
    done: bool,
}

/// Produces the next job-detail frame, sleeping or polling only when no ready frame exists.
async fn next_job_frame(mut state: JobStreamState) -> Option<(Result<Event, Infallible>, JobStreamState)> {
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
        let id = state.id.clone();
        match tokio::task::spawn_blocking(move || api.job_detail_page(&id)).await {
            Ok(Ok(page)) => {
                if state.last_job.as_ref().is_none_or(|last| last != &page) {
                    state.last_job = Some(page.clone());
                    let Some(event) = job_event(&page) else {
                        state.done = true;
                        return None;
                    };
                    return Some((Ok(event), state));
                }
            }
            // A detail read error ends the stream cleanly; the page reconnects and falls
            // back to its five-second polling until a stream opens again.
            _ => {
                state.done = true;
                return None;
            }
        }
    }
}

/// Encodes one job-detail snapshot as a `job` frame.
fn job_event(page: &JobDetailPage) -> Option<Event> {
    serde_json::to_string(page)
        .ok()
        .map(|data| Event::default().event("job").data(data))
}

/// Builds the SSE response for the filtered cost-report snapshot stream.
///
/// The first poll always sends one `cost` frame carrying the exact `CostReport` the matching
/// `GET /api/v1/cost` returns. Later polls send a frame only when that report changes, and a
/// heartbeat keeps the connection visibly alive while it is stable. There is no cursor or
/// resume: each frame is a self-contained report for the same filters.
pub(crate) fn cost_response(
    state: ApiState,
    spec: CostQuerySpec,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    snapshot_response(state, "cost", move |api: ApiState| api.cost(spec))
}

/// Builds the SSE response for the filtered inbox snapshot stream.
///
/// The first poll always sends one `inbox` frame carrying the exact `InboxPage` the matching
/// `GET /api/v1/inbox` returns for `state`. Later polls send a frame only when that page
/// changes, and a heartbeat keeps the connection visibly alive while it is stable. There is
/// no cursor or resume: each frame is a self-contained page for the same state.
pub(crate) fn inbox_response(
    state: ApiState,
    filter: InboxState,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    snapshot_response(state, "inbox", move |api: ApiState| api.inbox(filter))
}

/// Builds the SSE response for the nodes snapshot stream.
///
/// The first poll always sends one `nodes` frame carrying the exact `NodePage` the matching
/// `GET /api/v1/nodes` returns. Later polls send a frame only when that page changes, and a
/// heartbeat keeps the connection visibly alive while it is stable. There is no cursor or
/// resume: each frame is a self-contained page.
pub(crate) fn nodes_response(state: ApiState) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    snapshot_response(state, "nodes", |api: ApiState| api.nodes())
}

/// Builds an SSE response for one self-contained snapshot document.
///
/// `collect` runs on `spawn_blocking` once per poll, so no store is held across an await and
/// a slow client only waits for the next snapshot. The first poll always sends a frame; later
/// polls send one only when the document changes. A collect or encode error ends the stream
/// cleanly and the page reconnects and falls back to polling.
fn snapshot_response<T, F>(
    state: ApiState,
    event: &'static str,
    collect: F,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send>
where
    T: Clone + PartialEq + Serialize + Send + 'static,
    F: Clone + Fn(ApiState) -> Result<T, ApiFailure> + Send + 'static,
{
    let poll_interval = state.board_poll_interval();
    let heartbeat_interval = state.stream_heartbeat_interval();
    let cursor = SnapshotStreamState {
        state,
        event,
        collect,
        last_snapshot: None,
        poll_interval,
        heartbeat_interval,
        next_heartbeat: Instant::now() + heartbeat_interval,
        poll_due: true,
        done: false,
    };
    Sse::new(stream::unfold(cursor, next_snapshot_frame::<T, F>))
}

/// State carried between one snapshot stream's SSE frames.
struct SnapshotStreamState<T, F> {
    state: ApiState,
    event: &'static str,
    collect: F,
    last_snapshot: Option<T>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    poll_due: bool,
    done: bool,
}

/// Produces the next snapshot frame, sleeping or polling only when no ready frame exists.
async fn next_snapshot_frame<T, F>(
    mut state: SnapshotStreamState<T, F>,
) -> Option<(Result<Event, Infallible>, SnapshotStreamState<T, F>)>
where
    T: Clone + PartialEq + Serialize + Send + 'static,
    F: Clone + Fn(ApiState) -> Result<T, ApiFailure> + Send + 'static,
{
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
        let collect = state.collect.clone();
        match tokio::task::spawn_blocking(move || collect(api)).await {
            Ok(Ok(snapshot)) => {
                if state.last_snapshot.as_ref().is_none_or(|last| last != &snapshot) {
                    state.last_snapshot = Some(snapshot.clone());
                    let Some(event) = snapshot_event(state.event, &snapshot) else {
                        state.done = true;
                        return None;
                    };
                    return Some((Ok(event), state));
                }
            }
            // A collect error ends the stream cleanly; the page reconnects and falls back to
            // its five-second polling until a stream opens again.
            _ => {
                state.done = true;
                return None;
            }
        }
    }
}

/// Encodes one snapshot as a named frame.
fn snapshot_event<T: Serialize>(event: &'static str, snapshot: &T) -> Option<Event> {
    serde_json::to_string(snapshot)
        .ok()
        .map(|data| Event::default().event(event).data(data))
}
