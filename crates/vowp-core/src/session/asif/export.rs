//! One ASIF export: adapter call, source check, strict filter and atomic rename.

use std::path::Path;

use vowp_asif::ReportDirection;
use vowp_asif::export::{ExportFormat, ExportOptions, export as adapter_export};
use vowp_protocol::{ErrorKind, SessionId};
use vowp_store::NodePaths;

use crate::error::CoreError;

use super::report::{fatal_loss, strict_refusal, summary};
use super::{AsifExport, AsifExportOptions, asif_error, asif_io, remove_work_dir, work_dir, write_report};

/// Exports `session` as one deterministic `.asif.zip` package to `out`.
///
/// The adapter writes into a private directory under `<home>/tmp`; the report's
/// source session is checked, the VOWP strict filter runs on the in-memory loss
/// classes, and only then is the package renamed to `out`. A failure leaves an
/// existing `out` untouched and writes no report sidecar unless `--report` asked
/// for one and the export succeeded.
pub fn export_asif(
    paths: &NodePaths,
    session: &SessionId,
    out: &Path,
    opts: &AsifExportOptions,
) -> Result<AsifExport, CoreError> {
    if !paths.session_dir(session).is_dir() {
        return Err(asif_error(
            "SESSION_NOT_FOUND",
            format!("session {session} has no session directory"),
            ErrorKind::NotFound,
        ));
    }
    if opts.report_path.as_deref() == Some(out) {
        return Err(asif_error(
            "REPORT_PATH",
            "the ASIF report path must differ from the package output",
            ErrorKind::InvalidInput,
        ));
    }

    let work = work_dir(paths)?;
    let temporary_package = work.join("session.asif.zip");
    let temporary_report = work.join("report.json");

    let result = (|| {
        let adapter_report = adapter_export(
            &paths.session_dir(session),
            &temporary_package,
            &ExportOptions {
                format: ExportFormat::Zip,
                redact: opts.redact.clone(),
                keep_host_paths: opts.keep_host_paths,
                strict: false,
                allow_loss: opts.allow_loss,
                report_path: Some(temporary_report.clone()),
                ..ExportOptions::default()
            },
        )?;

        if adapter_report.direction != ReportDirection::Export
            || adapter_report.source.session_id.as_deref() != Some(session.as_str())
        {
            return Err(asif_error(
                "SOURCE_SESSION_MISMATCH",
                "the adapter report does not describe the requested source session",
                ErrorKind::Internal,
            ));
        }
        if opts.strict
            && let Some(loss) = fatal_loss(&adapter_report)
        {
            return Err(strict_refusal("export", loss));
        }

        let bytes = std::fs::metadata(&temporary_package)
            .map(|metadata| metadata.len())
            .map_err(|source| asif_io("inspect the exported ASIF package", source))?;
        if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| asif_io("create the ASIF output directory", source))?;
        }
        std::fs::rename(&temporary_package, out)
            .map_err(|source| asif_io("replace the ASIF output package", source))?;
        if let Some(report_path) = &opts.report_path {
            write_report(&adapter_report, report_path)?;
        }

        Ok(AsifExport {
            output: out.to_path_buf(),
            bytes,
            report: summary(&adapter_report),
        })
    })();

    remove_work_dir(&work);
    result
}
