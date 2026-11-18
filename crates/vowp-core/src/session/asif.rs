//! ASIF 0.4 export and import through the pinned `vowp-asif` adapter.
//! Spec: `docs/design/v0.4/asif-integration.md` §3 and §8 Job A.
//!
//! The adapter is the only implementation that maps a VOWP session directory to
//! ASIF and back. This module adds the VOWP policy around it: private temporary
//! directories, the report source/target check, the strict loss filter, the
//! staged-log pre-check, the final rename and reindex, and path-free errors.
//! It never emits a continuation object, never starts a turn and never creates
//! a lease.

mod export;
mod import;
mod report;

use std::path::{Path, PathBuf};

use vowp_asif::Report;
use vowp_protocol::{AsifReport, SessionId};
use vowp_store::NodePaths;

use crate::error::CoreError;

pub use export::export_asif;
pub use import::import_asif;

/// Upper bound accepted for one ASIF archive, from the adapter transport.
///
/// The web upload route and the CLI stdin path use the same limit; it is the
/// adapter's `MAX_ARCHIVE_BYTES`, not a VOWP-specific cap.
pub const MAX_ASIF_ARCHIVE_BYTES: u64 = vowp_asif::MAX_ARCHIVE_BYTES as u64;

/// Options for [`export_asif`].
#[derive(Debug, Clone, Default)]
pub struct AsifExportOptions {
    /// Exact literals replaced by `<redacted>` in every text field.
    pub redact: Vec<String>,
    /// Keep host-path-looking strings for a trusted local export.
    pub keep_host_paths: bool,
    /// Refuse `Blocking` and `Integrity` losses before the output is renamed.
    pub strict: bool,
    /// Pass to the adapter: recoverable integrity/ambiguity problems become losses.
    pub allow_loss: bool,
    /// Keep the full `vowp-asif-report-v1` document at this path.
    pub report_path: Option<PathBuf>,
}

impl AsifExportOptions {
    /// Builds the default options: safe host paths, declared losses allowed,
    /// no report sidecar.
    pub fn new() -> Self {
        Self::default()
    }
}

/// A finished ASIF export.
#[derive(Debug, Clone, PartialEq)]
pub struct AsifExport {
    /// The requested output path.
    pub output: PathBuf,
    /// Exact package bytes written.
    pub bytes: u64,
    /// Adapter-free report summary.
    pub report: AsifReport,
}

/// Options for [`import_asif`].
#[derive(Debug, Clone)]
pub struct AsifImportOptions {
    /// Working directory written to `session.created`.
    pub cwd: PathBuf,
    /// Branch to import when the document has more than one.
    pub branch: Option<String>,
    /// Refuse `Blocking` and `Integrity` losses before the session is visible.
    pub strict: bool,
    /// Pass to the adapter: recoverable integrity/ambiguity problems become losses.
    pub allow_loss: bool,
    /// Target session id; a fresh ULID is chosen when absent.
    pub session_id: Option<SessionId>,
    /// Keep the full `vowp-asif-report-v1` document at this path.
    pub report_path: Option<PathBuf>,
}

impl AsifImportOptions {
    /// Builds default options for one local import into `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            branch: None,
            strict: false,
            allow_loss: false,
            session_id: None,
            report_path: None,
        }
    }
}

/// A finished ASIF import.
#[derive(Debug, Clone, PartialEq)]
pub struct AsifImport {
    /// The new session row.
    pub session: vowp_protocol::Session,
    /// Adapter-free report summary.
    pub report: AsifReport,
    /// Expected post-rename reindex warnings (project/job left null).
    pub warnings: Vec<String>,
}

/// Creates one private work directory under `<home>/tmp`.
pub(crate) fn work_dir(paths: &NodePaths) -> Result<PathBuf, CoreError> {
    use std::os::unix::fs::DirBuilderExt;

    let tmp = paths.tmp_dir();
    std::fs::create_dir_all(&tmp).map_err(|source| asif_io("create the ASIF temporary directory", source))?;
    let work = tmp.join(format!("asif-{}", ulid::Ulid::generate()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&work)
        .map_err(|source| asif_io("create a private ASIF work directory", source))?;
    Ok(work)
}

/// Removes a work directory, ignoring an already-absent path.
pub(crate) fn remove_work_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

/// Builds a path-free ASIF error with an explicit stable code and class.
pub(crate) fn asif_error(
    code: impl Into<String>,
    message: impl Into<String>,
    class: vowp_protocol::ErrorKind,
) -> CoreError {
    CoreError::Asif {
        code: code.into(),
        message: message.into(),
        class,
    }
}

/// Builds a path-free internal ASIF I/O error.
pub(crate) fn asif_io(operation: &str, source: std::io::Error) -> CoreError {
    asif_error(
        "IO",
        format!("cannot {operation}: {source}"),
        vowp_protocol::ErrorKind::Internal,
    )
}

/// Writes the full adapter report through a private temporary file and rename.
pub(crate) fn write_report(report: &Report, path: &Path) -> Result<(), CoreError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    let text = report.to_json_string().map_err(|error| {
        asif_error(
            "REPORT_JSON",
            format!("cannot serialize the ASIF report: {error}"),
            vowp_protocol::ErrorKind::Internal,
        )
    })?;
    let Some(file_name) = path.file_name() else {
        return Err(asif_error(
            "REPORT_PATH",
            "the ASIF report path must name a file",
            vowp_protocol::ErrorKind::InvalidInput,
        ));
    };
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(|source| asif_io("create the ASIF report directory", source))?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.asif-{}.tmp",
        file_name.to_string_lossy(),
        ulid::Ulid::generate()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| asif_io("create the ASIF report temporary file", source))?;
        file.write_all(text.as_bytes())
            .map_err(|source| asif_io("write the ASIF report", source))?;
        file.sync_all()
            .map_err(|source| asif_io("sync the ASIF report", source))?;
        std::fs::rename(&temporary, path).map_err(|source| asif_io("replace the ASIF report file", source))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
