//! Adapter report to protocol summary conversion and the VOWP strict-loss filter.

use vowp_asif::report::{CoverageScope, Loss, LossClass, LossStage, Report};
use vowp_protocol::{AsifCounts, AsifLoss, AsifReport, ErrorKind};

use crate::error::CoreError;

use super::asif_error;

/// Converts one adapter report into the shared, adapter-free summary.
pub(crate) fn summary(report: &Report) -> AsifReport {
    AsifReport {
        adapter_version: report.adapter.version.clone(),
        asif_version: report.asif.version.clone(),
        asif_pin: report.asif.pin.clone(),
        source_session: report.source.session_id.clone(),
        target_session: report.target_session_id.clone(),
        counts: AsifCounts {
            events_in: report.counts.events_in,
            events_out: report.counts.events_out,
            resources: report.counts.resources,
            capsules: report.counts.capsules,
        },
        loss_count: report.losses.len() as u64,
        losses: report.losses.iter().map(loss_summary).collect(),
        unsupported: report.unsupported.clone(),
    }
}

/// Returns the first loss class VOWP strict mode refuses.
pub(crate) fn fatal_loss(report: &Report) -> Option<&Loss> {
    report
        .losses
        .iter()
        .find(|loss| matches!(loss.class, LossClass::Blocking | LossClass::Integrity))
}

/// Builds the stable path-free refusal used by VOWP strict mode.
///
/// Only the loss kind and class are named; the adapter explanation is not copied
/// into the error because a CLI or web error may be printed outside the report.
pub(crate) fn strict_refusal(direction: &str, loss: &Loss) -> CoreError {
    let class = match loss.class {
        LossClass::Blocking => "blocking",
        LossClass::Integrity => "integrity",
        LossClass::Normalization => "normalization",
        LossClass::Omission => "omission",
    };
    asif_error(
        "STRICT_LOSS",
        format!(
            "strict ASIF {direction} refused because loss `{}` has class {class}",
            loss.kind
        ),
        ErrorKind::Refused,
    )
}

/// Maps one adapter loss to its stable summary.
fn loss_summary(loss: &Loss) -> AsifLoss {
    AsifLoss {
        kind: loss.kind.clone(),
        stage: stage_text(loss.stage).to_owned(),
        scope: loss.scope.map(scope_text).map(str::to_owned),
        message: loss.explanation.clone(),
    }
}

/// Stable lower-case name for a loss stage.
fn stage_text(stage: LossStage) -> &'static str {
    match stage {
        LossStage::Capture => "capture",
        LossStage::Normalization => "normalization",
        LossStage::Redaction => "redaction",
        LossStage::Translation => "translation",
    }
}

/// Stable lower-case name for a coverage scope.
fn scope_text(scope: CoverageScope) -> &'static str {
    scope.as_str()
}
