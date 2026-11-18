//! Adapter-free ASIF report summaries and the import response.
//! Spec: `docs/design/types.md` §16.11 and `docs/design/v0.4/asif-integration.md` §3.
//!
//! The `vowp-asif` adapter owns the `vowp-asif-report-v1` document and the loss
//! classes used for strict filtering. These types are the stable, path-free subset
//! VOWP exposes in `--json` output and the web import response; they deliberately
//! carry no source text, absolute host path or adapter-only field.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session::Session;

/// Record counts reported by an ASIF export or import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AsifCounts {
    /// Source events read.
    pub events_in: u64,
    /// Target events written.
    pub events_out: u64,
    /// Resources written or preserved.
    pub resources: u64,
    /// Capsules read or written.
    pub capsules: u64,
}

/// One declared ASIF information change or omission.
///
/// The adapter's ordering class (`Blocking`, `Integrity`, `Normalization`,
/// `Omission`) is not serialized; it is applied by the core strict filter before
/// this summary is produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AsifLoss {
    /// Machine-readable loss kind.
    pub kind: String,
    /// Stage at which the change occurred: `capture`, `normalization`,
    /// `redaction` or `translation`.
    pub stage: String,
    /// Affected coverage domain, when the adapter names one.
    #[serde(default)]
    pub scope: Option<String>,
    /// Path-free explanation. It never contains source content or secrets.
    pub message: String,
}

/// Stable, adapter-free summary of one ASIF export or import report.
///
/// This is the document printed under `"report"` by `vowp session export
/// --json` and `vowp session import --json`; the full
/// `vowp-asif-report-v1` document is written only through `--report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AsifReport {
    /// Adapter version that performed the translation.
    pub adapter_version: String,
    /// ASIF draft version interpreted by the adapter.
    pub asif_version: String,
    /// Pinned ASIF commit interpreted by the adapter.
    pub asif_pin: String,
    /// Source session id, when the direction records one.
    pub source_session: Option<String>,
    /// Import target session id.
    pub target_session: Option<String>,
    /// Record counts for this translation.
    pub counts: AsifCounts,
    /// Number of declared losses.
    pub loss_count: u64,
    /// Declared losses in stable order.
    pub losses: Vec<AsifLoss>,
    /// VOWP operations that are not captured by the ASIF document.
    pub unsupported: Vec<String>,
}

/// The shared body of `vowp session import --format asif --json` and the web
/// import response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AsifImportResponse {
    /// The new registered session row.
    pub session: Session,
    /// Adapter report summary for the import.
    pub report: AsifReport,
    /// Non-fatal reindex warnings, such as a source project that is absent locally.
    pub warnings: Vec<String>,
}
