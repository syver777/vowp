//! The project-registry view used by the node supervisor and `vowp project`.
//!
//! The registry is backed by the real `main.projects` columns.
//!
//! These helpers are thin views over [`vowp_store::Store`]'s `ProjectEntry` API
//! (`project_entry`, `list_project_entries`, `project_by_path`, `set_project_state`,
//! `set_project_weight`, `remove_project_record`). The state, weight, pause reason and
//! archive timestamp live in their own columns and are the same rows the fleet snapshot
//! reads; there is no second copy in `config_json`.

use std::path::Path;

use vowp_protocol::{ProjectEntry, ProjectName, ProjectState};
use vowp_store::Store;

use crate::CoreError;

/// Returns one registry entry, or `None` when the row does not exist.
pub fn project_entry(store: &Store, name: &ProjectName) -> Result<Option<ProjectEntry>, CoreError> {
    Ok(store.project_entry(name)?)
}

/// Lists every registry entry, ordered by name.
pub fn project_entries(store: &Store) -> Result<Vec<ProjectEntry>, CoreError> {
    Ok(store.list_project_entries()?)
}

/// Returns the entry whose recorded checkout path is `path`.
///
/// Registration stores the root that git reports, which on macOS resolves the
/// `/var/...` temp root to `/private/var/...`. Resolve the lookup path the same way
/// before querying, and fall back to the lexical path when the checkout no longer
/// exists or is still recorded under its old spelling.
pub fn project_by_path(store: &Store, path: &Path) -> Result<Option<ProjectEntry>, CoreError> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(entry) = store.project_by_path(&canonical)? {
        return Ok(Some(entry));
    }
    if canonical.as_path() != path {
        return Ok(store.project_by_path(path)?);
    }
    Ok(None)
}

/// Sets one project's state and paused reason in its registry row.
pub fn set_project_state(
    store: &Store,
    name: &ProjectName,
    state: ProjectState,
    reason: &str,
) -> Result<ProjectEntry, CoreError> {
    Ok(store.set_project_state(name, state, reason)?)
}

/// Sets one project's fairness weight (`1..=100`).
pub fn set_project_weight(store: &Store, name: &ProjectName, weight: u8) -> Result<(), CoreError> {
    store.set_project_weight(name, weight)?;
    Ok(())
}

/// Removes one project's registry row, keeping its database and history.
pub fn remove_project_record(store: &Store, name: &ProjectName) -> Result<(), CoreError> {
    if name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
        return Err(CoreError::Invalid(
            "cannot remove the reserved _node project".to_owned(),
        ));
    }
    store.remove_project_record(name)?;
    Ok(())
}
