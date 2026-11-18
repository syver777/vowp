//! One ASIF import: adapter staging, staged-log pre-check, rename and reindex.

use std::collections::BTreeSet;
use std::path::Path;

use vowp_asif::import::{ImportOptions, import as adapter_import};
use vowp_asif::{ReportDirection, codes};
use vowp_protocol::{Capsule, ErrorKind, EventBody, EventEnvelope, SessionId};
use vowp_store::reindex::reindex;
use vowp_store::session_files::AdaptersMap;
use vowp_store::{NodePaths, Store};

use crate::error::CoreError;

use super::report::{fatal_loss, strict_refusal, summary};
use super::{AsifImport, AsifImportOptions, asif_error, asif_io, remove_work_dir, work_dir, write_report};

/// Imports one ASIF package, standalone document or directory into a new session.
///
/// The adapter validates and writes a complete session directory below
/// `<home>/tmp`; this function checks the report target, applies the VOWP strict
/// filter, pre-checks the staged log with the same rules the indexer applies,
/// renames the directory into `sessions/<id>/` and reindexes it. Every failure
/// before the rename leaves no session directory, row or report.
pub fn import_asif(
    paths: &NodePaths,
    store: &Store,
    input: &Path,
    opts: &AsifImportOptions,
) -> Result<AsifImport, CoreError> {
    let target = opts.session_id.clone().unwrap_or_else(SessionId::new);
    if store.session(&target)?.is_some() || paths.session_dir(&target).symlink_metadata().is_ok() {
        return Err(target_exists(&target));
    }

    let work = work_dir(paths)?;
    let staged = work.join(target.as_str());
    let temporary_report = work.join("report.json");

    let result = (|| {
        let adapter_report = adapter_import(
            input,
            &staged,
            &ImportOptions {
                cwd: Some(opts.cwd.clone()),
                session_id: Some(target.to_string()),
                branch: opts.branch.clone(),
                allow_unsupported: false,
                strict: false,
                allow_loss: opts.allow_loss,
                report_path: Some(temporary_report.clone()),
            },
        )?;

        if adapter_report.direction != ReportDirection::Import
            || adapter_report.target_session_id.as_deref() != Some(target.as_str())
        {
            return Err(asif_error(
                "TARGET_SESSION_MISMATCH",
                "the adapter report does not describe the requested target session",
                ErrorKind::Internal,
            ));
        }
        if opts.strict
            && let Some(loss) = fatal_loss(&adapter_report)
        {
            return Err(strict_refusal("import", loss));
        }

        check_staged_session(&staged, &target, store)?;

        if store.session(&target)?.is_some() || paths.session_dir(&target).symlink_metadata().is_ok() {
            return Err(target_exists(&target));
        }
        std::fs::create_dir_all(paths.sessions_dir())
            .map_err(|source| asif_io("create the sessions directory", source))?;
        if let Err(source) = std::fs::rename(&staged, paths.session_dir(&target)) {
            return Err(if paths.session_dir(&target).symlink_metadata().is_ok() {
                target_exists(&target)
            } else {
                asif_io("move the imported session into place", source)
            });
        }

        let index_report = match reindex(store, paths, Some(&target)) {
            Ok(report) => report,
            Err(error) => {
                remove_session_dir(paths, &target);
                return Err(error.into());
            }
        };
        let warnings = match reindex_warnings(&target, &index_report) {
            Ok(warnings) => warnings,
            Err(error) => {
                remove_session_dir(paths, &target);
                return Err(error);
            }
        };

        if let Some(report_path) = &opts.report_path {
            write_report(&adapter_report, report_path)?;
        }

        Ok(AsifImport {
            session: store.require_session(&target)?,
            report: summary(&adapter_report),
            warnings,
        })
    })();

    remove_work_dir(&work);
    result
}

/// Rejects a target that already exists as a row or as any path entry.
fn target_exists(target: &SessionId) -> CoreError {
    asif_error(
        "TARGET_EXISTS",
        format!("session {target} already exists; ASIF import never overwrites or merges"),
        ErrorKind::Conflict,
    )
}

/// Removes a just-renamed session directory after a reindex failure.
fn remove_session_dir(paths: &NodePaths, target: &SessionId) {
    let _ = std::fs::remove_dir_all(paths.session_dir(target));
}

/// Pre-checks the staged log, capsules and adapters file before the rename.
///
/// The rules match the ones the indexer applies: every line parses, the first
/// event is `session.created`, sequence numbers are contiguous from 1, the tail
/// is clean and every event id is globally unique.
fn check_staged_session(staged: &Path, target: &SessionId, store: &Store) -> Result<(), CoreError> {
    let log_path = staged.join("log.jsonl");
    if !log_path.is_file() {
        return Err(invalid_staged("the adapter produced no log.jsonl"));
    }
    let bytes = std::fs::read(&log_path).map_err(|source| asif_io("read the staged session log", source))?;
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        return Err(invalid_staged("the staged session log has no clean final line"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid_staged("the staged session log is not UTF-8"))?;

    let mut seen = BTreeSet::new();
    let mut first = true;
    for (index, line) in text.split_terminator('\n').enumerate() {
        if line.trim().is_empty() {
            return Err(invalid_staged("the staged session log contains an empty line"));
        }
        let event: EventEnvelope = serde_json::from_str(line)
            .map_err(|_| invalid_staged("the staged session log contains an invalid event"))?;
        if event.session_id != *target {
            return Err(invalid_staged("the staged session log belongs to another session id"));
        }
        if event.seq != index as u64 + 1 {
            return Err(invalid_staged(
                "the staged session log has a gap or duplicate sequence number",
            ));
        }
        if !seen.insert(event.id.clone()) {
            return Err(asif_error(
                codes::EVENT_ID_DUPLICATE,
                "the staged session log contains a duplicate event id",
                ErrorKind::Conflict,
            ));
        }
        if let Some(existing) = store.event_row_by_id(&event.id)? {
            return Err(asif_error(
                codes::EVENT_ID_DUPLICATE,
                format!(
                    "event id {} already belongs to session {}",
                    event.id, existing.session_id
                ),
                ErrorKind::Conflict,
            ));
        }
        let body = event
            .body()
            .map_err(|_| invalid_staged("the staged session log contains an invalid event payload"))?;
        if first && !matches!(body, EventBody::SessionCreated(_)) {
            return Err(invalid_staged(
                "the staged session log does not start with session.created",
            ));
        }
        first = false;
    }
    if first {
        return Err(invalid_staged("the staged session log is empty"));
    }

    check_staged_capsules(staged, target)?;
    check_staged_adapters(staged)?;
    Ok(())
}

/// Checks every staged capsule file parses, validates and names the target session.
fn check_staged_capsules(staged: &Path, target: &SessionId) -> Result<(), CoreError> {
    let capsules = staged.join("capsules");
    let entries = match std::fs::read_dir(&capsules) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(asif_io("read the staged capsules directory", source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| asif_io("read the staged capsules directory", source))?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|source| asif_io("read a staged capsule file", source))?;
        let capsule: Capsule =
            serde_json::from_slice(&bytes).map_err(|_| invalid_staged("a staged capsule file is not valid JSON"))?;
        capsule
            .validate()
            .map_err(|_| invalid_staged("a staged capsule file is not a valid capsule"))?;
        if capsule.session_id != *target {
            return Err(invalid_staged("a staged capsule file belongs to another session id"));
        }
    }
    Ok(())
}

/// Checks a staged `adapters.json` parses before the indexer reads it.
fn check_staged_adapters(staged: &Path) -> Result<(), CoreError> {
    let path = staged.join("adapters.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice::<AdaptersMap>(&bytes)
                .map_err(|_| invalid_staged("the staged adapters.json is invalid"))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(asif_io("read the staged adapters.json", source)),
    }
}

/// Accepts only the expected project/job foreign-key warnings from reindex.
fn reindex_warnings(target: &SessionId, report: &vowp_store::reindex::ReindexReport) -> Result<Vec<String>, CoreError> {
    if report.sessions != 1 {
        return Err(asif_error(
            "IMPORT_INDEX",
            "the imported session was not indexed exactly once",
            ErrorKind::Internal,
        ));
    }
    let mut warnings = Vec::new();
    for problem in &report.problems {
        if expected_foreign_key_warning(target, problem) {
            warnings.push(problem.clone());
        } else {
            return Err(asif_error(
                "IMPORT_INDEX",
                "the imported session produced an unexpected reindex problem",
                ErrorKind::Internal,
            ));
        }
    }
    Ok(warnings)
}

/// True for the two documented "left null" foreign-key warnings.
fn expected_foreign_key_warning(target: &SessionId, problem: &str) -> bool {
    let prefix = format!("session {target}: ");
    let Some(rest) = problem.strip_prefix(&prefix) else {
        return false;
    };
    (rest.starts_with("project ") || rest.starts_with("job ")) && rest.ends_with("; left null")
}

/// Builds the path-free invalid-input error for a staged-log problem.
fn invalid_staged(message: &str) -> CoreError {
    asif_error("IMPORT_INVALID", message, ErrorKind::InvalidInput)
}
