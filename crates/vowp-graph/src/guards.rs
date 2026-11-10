//! Progress guards: stall detection and repeated-action tripwires.
//! Spec: `docs/design/crates/vowp-graph.md` §8 and `docs/design/loops.md` §12.
//!
//! Both guards are pure: the caller supplies timestamps and fingerprints. [`StallJudge`]
//! decides whether a live run has stopped making progress; [`RepeatGuard`] trips when the
//! same action is observed with the same evidence too many times in a row.

use std::time::Duration;

use vowp_protocol::Timestamp;

pub use crate::machine::{attempts_left, is_exhausted};

/// Stall thresholds in whole minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallThresholds {
    /// Minutes without a new session event or commit before a run counts as stalled.
    pub stall_minutes: u32,
    /// Stricter threshold for a run that has produced no output at all.
    pub silent_minutes: u32,
}

impl Default for StallThresholds {
    fn default() -> Self {
        Self {
            stall_minutes: 20,
            silent_minutes: 8,
        }
    }
}

/// One observation of a live run used by [`StallJudge`].
#[derive(Debug, Clone)]
pub struct StallInput {
    /// Instant the judgment is made.
    pub now: Timestamp,
    /// When the current run started.
    pub run_started_at: Timestamp,
    /// Newest session event of the run, if any.
    pub last_event_at: Option<Timestamp>,
    /// Newest commit on the job branch, if any.
    pub last_commit_at: Option<Timestamp>,
    /// Number of bytes the run's output file holds.
    pub output_bytes: u64,
}

/// What a stall check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallVerdict {
    /// The run is still making progress, or has not been idle long enough.
    Healthy,
    /// The run crossed its idle threshold.
    Stalled {
        /// Whole minutes of idleness at the time of the judgment.
        idle_minutes: u32,
        /// True when the run has produced no output at all.
        silent: bool,
    },
}

/// Stateless stall judge; see [`StallJudge::judge`].
pub struct StallJudge;

impl StallJudge {
    /// Judges one run against the thresholds.
    ///
    /// Progress is the newest of the run start, last session event and last commit. The idle
    /// time is `now - progress` floored to whole minutes (never negative). A run with zero
    /// output bytes uses [`StallThresholds::silent_minutes`]; every other run uses
    /// [`StallThresholds::stall_minutes`]. The verdict is [`StallVerdict::Stalled`] when the
    /// idle time reaches the threshold.
    pub fn judge(input: &StallInput, th: &StallThresholds) -> StallVerdict {
        let mut progress = input.run_started_at;
        if let Some(event) = input.last_event_at {
            progress = progress.max(event);
        }
        if let Some(commit) = input.last_commit_at {
            progress = progress.max(commit);
        }

        let idle_minutes = idle_minutes(input.now.since(progress));
        let silent = input.output_bytes == 0;
        let threshold = if silent { th.silent_minutes } else { th.stall_minutes };

        if idle_minutes >= threshold {
            StallVerdict::Stalled { idle_minutes, silent }
        } else {
            StallVerdict::Healthy
        }
    }
}

/// Converts an elapsed duration to whole minutes, saturating at [`u32::MAX`].
fn idle_minutes(elapsed: Duration) -> u32 {
    u32::try_from(elapsed.as_secs() / 60).unwrap_or(u32::MAX)
}

/// The result of observing one action fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatVerdict {
    /// The observation is below the repetition bound.
    Ok,
    /// The same action and fingerprint repeated `count` times in a row.
    Tripped {
        /// Length of the current identical run.
        count: u32,
    },
}

/// Trips when the same `(key, fingerprint)` is observed `max` times in a row.
#[derive(Debug, Clone)]
pub struct RepeatGuard {
    max: u32,
    last: Option<(String, String)>,
    count: u32,
}

impl RepeatGuard {
    /// Creates a guard that trips at `max` identical observations.
    pub fn new(max: u32) -> Self {
        Self {
            max,
            last: None,
            count: 0,
        }
    }

    /// Records one observation.
    ///
    /// A different key or fingerprint starts a fresh run at one. The third identical
    /// observation of a `new(3)` guard returns [`RepeatVerdict::Tripped`] with `count: 3`.
    pub fn observe(&mut self, key: &str, fingerprint: &str) -> RepeatVerdict {
        match &self.last {
            Some((last_key, last_fingerprint)) if last_key == key && last_fingerprint == fingerprint => {
                self.count = self.count.saturating_add(1);
            }
            _ => {
                self.last = Some((key.to_owned(), fingerprint.to_owned()));
                self.count = 1;
            }
        }

        if self.count >= self.max {
            RepeatVerdict::Tripped { count: self.count }
        } else {
            RepeatVerdict::Ok
        }
    }

    /// Forgets the previous observation, so the next one starts a fresh run.
    pub fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }
}

impl Default for RepeatGuard {
    fn default() -> Self {
        Self::new(3)
    }
}
