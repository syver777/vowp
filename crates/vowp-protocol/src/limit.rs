//! Provider limits: what kind of limit was hit and the `limits` table row that records it.
//! Spec: `docs/design/types.md` §4.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::executor::HarnessKind;
use crate::time::Timestamp;

/// The kind of limit reported in a `limit` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// A rolling provider quota window was exhausted.
    Quota,
    /// The account ran out of credits.
    Credits,
    /// A request rate limit was hit.
    Rate,
    /// The model context window was exhausted.
    Context,
    /// Authentication failed or expired.
    Auth,
}

/// The `limits` table row kind. [`LimitRowKind::Unavailable`] is used only in the table
/// (repeated unknown failures), never in events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitRowKind {
    /// A rolling provider quota window was exhausted.
    Quota,
    /// The account ran out of credits.
    Credits,
    /// A request rate limit was hit.
    Rate,
    /// The model context window was exhausted.
    Context,
    /// Authentication failed or expired.
    Auth,
    /// Three consecutive unknown failures made the provider temporarily unavailable.
    Unavailable,
}

/// One row of the `limits` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Limit {
    /// Limits key: the provider name, or a CLI harness entry name.
    pub provider: String,
    /// Harness that reported the limit, when known.
    pub harness: Option<HarnessKind>,
    /// Limit class.
    pub kind: LimitRowKind,
    /// When the limit lifts; `None` means a human must resolve it.
    pub resets_at: Option<Timestamp>,
    /// Redacted human-readable detail (never the raw secret-bearing output).
    pub detail: String,
    /// When the limit was first observed.
    pub since: Timestamp,
    /// When the rung was last probed for recovery.
    pub last_probe_at: Option<Timestamp>,
    /// Consecutive failed probes.
    pub probe_fail_streak: u32,
    /// Temporary concurrency cap, when the provider reports one.
    pub concurrency_cap: Option<u32>,
    /// When the concurrency cap expires.
    pub cap_until: Option<Timestamp>,
}
