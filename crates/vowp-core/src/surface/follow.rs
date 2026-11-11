//! Read-only follow mode for a surface that lost the lease.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3–§7.4.
//!
//! [`follow`] is the long-running display used by `vowp chat --follow`: it polls the
//! authoritative session index and prints one `#seq type summary` line per new event
//! until the caller cancels it. [`follow_to`] is the same loop with an explicit writer so
//! tests and the chat handler can capture the output instead of writing to stdout.

use std::io::Write;
use std::time::Duration;

use vowp_protocol::{CancelToken, SessionId};
use vowp_store::Store;

use crate::CoreError;

/// How long the follower sleeps between index polls.
pub const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// How many events one poll asks the index for.
const FOLLOW_BATCH: u32 = 256;

/// Display width of one follower line, matching the chat surface's narrow-terminal rule.
const FOLLOW_WIDTH: usize = 80;

/// Prints new events after `after_seq` until cancelled and returns the last sequence read.
///
/// The index is the single source of order: an event is printed only after the home has
/// assigned its `seq`. A missing session is [`CoreError::NotFound`]; a compacted session
/// keeps its index rows, so following still works after the log file is gone.
pub fn follow(store: &Store, session: &SessionId, after_seq: u64, cancel: &CancelToken) -> Result<u64, CoreError> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    follow_to(store, session, after_seq, cancel, &mut out)
}

/// Follows a session into an explicit writer and returns the last sequence printed.
///
/// Each line is `#<seq> <event-type>  <summary>`, clipped to 80 characters: no colour and
/// no table. The loop stops on cancellation or when the session index cannot be read. A
/// cancelled token returns the last sequence already printed, not an error.
pub fn follow_to(
    store: &Store,
    session: &SessionId,
    after_seq: u64,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<u64, CoreError> {
    if store.session(session)?.is_none() {
        return Err(CoreError::NotFound(format!("session {session}")));
    }
    let mut last = after_seq;
    loop {
        let rows = store.event_rows(session, last, FOLLOW_BATCH)?;
        for row in &rows {
            let prefix = format!("#{} {}  ", row.seq, row.event_type.as_str());
            let room = FOLLOW_WIDTH.saturating_sub(prefix.chars().count());
            let summary = clip_chars(&row.summary, room);
            writeln!(out, "{prefix}{summary}").map_err(|source| CoreError::Io {
                path: format!("follow {session}"),
                source,
            })?;
            last = last.max(row.seq);
        }
        if !rows.is_empty() {
            out.flush().map_err(|source| CoreError::Io {
                path: format!("follow {session}"),
                source,
            })?;
        }
        if cancel.is_cancelled() {
            return Ok(last);
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Clips `text` to at most `width` characters, adding an ellipsis when it must shorten.
fn clip_chars(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}
