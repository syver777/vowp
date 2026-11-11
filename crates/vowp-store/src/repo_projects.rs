//! Repository methods for the project registry (`main.projects`).
//!
//! The registry row is the global record of one project: its checkout path, git metadata,
//! state, fairness weight and pause/archive bookkeeping. [`crate::repo_nodes`] owns the
//! `projects` table as a [`vowp_protocol::Project`] view; this module exposes the richer
//! [`ProjectEntry`] view the project commands and the fleet snapshot read.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.2, §3.5, §5.3 and §13.3.

use std::path::Path;

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{ProjectEntry, ProjectName, ProjectState};

use crate::convert::{enum_from_text, enum_to_text, opt_ts_from_text, ts_from_text, ts_to_text};
use crate::db::Store;
use crate::error::StoreError;

/// Column list shared by every `projects` entry query.
const PROJECT_ENTRY_COLUMNS: &str = "name, path, remote_url, default_branch, state, weight, paused_reason, \
     archived_at, created_at, updated_at";

/// Raw TEXT/INTEGER values of one project registry row before protocol conversion.
struct ProjectEntryRow {
    name: String,
    path: String,
    remote_url: Option<String>,
    default_branch: String,
    state: String,
    weight: i64,
    paused_reason: String,
    archived_at: Option<String>,
    created_at: String,
    updated_at: String,
}

/// Reads the textual columns of one project registry row.
fn read_project_entry_row(row: &Row<'_>) -> rusqlite::Result<ProjectEntryRow> {
    Ok(ProjectEntryRow {
        name: row.get(0)?,
        path: row.get(1)?,
        remote_url: row.get(2)?,
        default_branch: row.get(3)?,
        state: row.get(4)?,
        weight: row.get(5)?,
        paused_reason: row.get(6)?,
        archived_at: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// Converts a stored integer weight into the protocol's `1..=100` range.
fn weight_from_i64(value: i64) -> Result<u8, StoreError> {
    let weight = u8::try_from(value).map_err(|error| crate::convert::corrupt("projects.weight", error))?;
    if !(1..=100).contains(&weight) {
        return Err(crate::convert::corrupt(
            "projects.weight",
            format!("weight {weight} is outside 1..=100"),
        ));
    }
    Ok(weight)
}

/// Converts a raw registry row into a protocol [`ProjectEntry`].
fn project_entry_from_row(raw: ProjectEntryRow) -> Result<ProjectEntry, StoreError> {
    Ok(ProjectEntry {
        name: ProjectName::parse(&raw.name)?,
        path: raw.path,
        remote_url: raw.remote_url,
        default_branch: raw.default_branch,
        state: enum_from_text::<ProjectState>(&raw.state, "projects.state")?,
        weight: weight_from_i64(raw.weight)?,
        paused_reason: raw.paused_reason,
        archived_at: opt_ts_from_text(raw.archived_at.as_deref(), "projects.archived_at")?,
        created_at: ts_from_text(&raw.created_at, "projects.created_at")?,
        updated_at: ts_from_text(&raw.updated_at, "projects.updated_at")?,
    })
}

impl Store {
    /// Returns the registry entry called `name`, when it exists.
    pub fn project_entry(&self, name: &ProjectName) -> Result<Option<ProjectEntry>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_ENTRY_COLUMNS} FROM main.projects WHERE name = ?1"),
                [name.as_str()],
                read_project_entry_row,
            )
            .optional()?;
        raw.map(project_entry_from_row).transpose()
    }

    /// Lists every registry entry, ordered by name.
    pub fn list_project_entries(&self) -> Result<Vec<ProjectEntry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {PROJECT_ENTRY_COLUMNS} FROM main.projects ORDER BY name"
        ))?;
        let rows = statement.query_map([], read_project_entry_row)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(project_entry_from_row(row?)?);
        }
        Ok(entries)
    }

    /// Returns the registry entry whose recorded checkout path is exactly `path`.
    pub fn project_by_path(&self, path: &Path) -> Result<Option<ProjectEntry>, StoreError> {
        let text = path
            .to_str()
            .ok_or_else(|| StoreError::InvalidInput(format!("project path is not valid UTF-8: {}", path.display())))?;
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_ENTRY_COLUMNS} FROM main.projects WHERE path = ?1"),
                [text],
                read_project_entry_row,
            )
            .optional()?;
        raw.map(project_entry_from_row).transpose()
    }

    /// Sets one project's registry state and returns the updated entry.
    ///
    /// `reason` is stored only for [`ProjectState::Paused`] (`owner` or `broken: <error>`);
    /// active and archived projects have an empty pause reason. Archiving stamps
    /// `archived_at` once and keeps it on a repeated archive; any other state clears it.
    pub fn set_project_state(
        &self,
        name: &ProjectName,
        state: ProjectState,
        reason: &str,
    ) -> Result<ProjectEntry, StoreError> {
        let now = self.clock().now();
        let state_text = enum_to_text(&state)?;
        let paused_reason = if state == ProjectState::Paused { reason } else { "" };
        let changed = self.conn.execute(
            "UPDATE main.projects SET
                state = ?1,
                paused_reason = ?2,
                archived_at = CASE WHEN ?1 = 'archived' THEN COALESCE(archived_at, ?3) ELSE NULL END,
                updated_at = ?3
             WHERE name = ?4",
            params![state_text, paused_reason, ts_to_text(now), name.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("project", name.as_str()));
        }
        self.project_entry(name)?
            .ok_or_else(|| StoreError::not_found("project", name.as_str()))
    }

    /// Sets one project's fairness weight (`1..=100`).
    pub fn set_project_weight(&self, name: &ProjectName, weight: u8) -> Result<(), StoreError> {
        if !(1..=100).contains(&weight) {
            return Err(StoreError::InvalidInput(format!(
                "project weight {weight} is outside 1..=100"
            )));
        }
        let changed = self.conn.execute(
            "UPDATE main.projects SET weight = ?1, updated_at = ?2 WHERE name = ?3",
            params![i64::from(weight), ts_to_text(self.clock().now()), name.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("project", name.as_str()));
        }
        Ok(())
    }

    /// Removes one project's registry row, leaving its project database and history alone.
    pub fn remove_project_record(&self, name: &ProjectName) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM main.projects WHERE name = ?1", [name.as_str()])?;
        if changed == 0 {
            return Err(StoreError::not_found("project", name.as_str()));
        }
        Ok(())
    }
}
