//! Repository methods for the `nodes` and `projects` tables.
//! Spec: `docs/design/crates/vowp-store.md` §3.4 and `docs/design/types.md` §10.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{Node, NodeName, Project, ProjectName, Timestamp};

use crate::convert::{
    bool_from_int, bool_to_int, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text, ts_from_text,
    ts_to_text,
};
use crate::db::Store;
use crate::error::StoreError;

/// Column list shared by every `nodes` query.
const NODE_COLUMNS: &str = "name, ssh_target, is_local, capabilities, last_seen_at, created_at";

/// Column list shared by every `projects` query.
const PROJECT_COLUMNS: &str = "name, path, remote_url, default_branch, config_json, created_at, updated_at";

/// Raw TEXT/INTEGER values of one `nodes` row before protocol conversion.
struct NodeRow {
    name: String,
    ssh_target: Option<String>,
    is_local: i64,
    capabilities: String,
    last_seen_at: Option<String>,
    created_at: String,
}

/// Raw TEXT values of one `projects` row before protocol conversion.
struct ProjectRow {
    name: String,
    path: String,
    remote_url: Option<String>,
    default_branch: String,
    config: String,
    created_at: String,
    updated_at: String,
}

/// Reads the textual columns of a `nodes` row.
fn read_node_row(row: &Row<'_>) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        name: row.get(0)?,
        ssh_target: row.get(1)?,
        is_local: row.get(2)?,
        capabilities: row.get(3)?,
        last_seen_at: row.get(4)?,
        created_at: row.get(5)?,
    })
}

/// Converts a raw `nodes` row into a protocol [`Node`].
fn node_from_row(raw: NodeRow) -> Result<Node, StoreError> {
    Ok(Node {
        name: NodeName::parse(&raw.name)?,
        ssh_target: raw.ssh_target,
        is_local: bool_from_int(raw.is_local),
        capabilities: json_from_text(&raw.capabilities, "nodes.capabilities")?,
        last_seen_at: opt_ts_from_text(raw.last_seen_at.as_deref(), "nodes.last_seen_at")?,
        created_at: ts_from_text(&raw.created_at, "nodes.created_at")?,
    })
}

/// Reads the textual columns of a `projects` row.
fn read_project_row(row: &Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        name: row.get(0)?,
        path: row.get(1)?,
        remote_url: row.get(2)?,
        default_branch: row.get(3)?,
        config: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Converts a raw `projects` row into a protocol [`Project`].
fn project_from_row(raw: ProjectRow) -> Result<Project, StoreError> {
    Ok(Project {
        name: ProjectName::parse(&raw.name)?,
        path: raw.path,
        remote_url: raw.remote_url,
        default_branch: raw.default_branch,
        config: json_from_text(&raw.config, "projects.config_json")?,
        created_at: ts_from_text(&raw.created_at, "projects.created_at")?,
        updated_at: ts_from_text(&raw.updated_at, "projects.updated_at")?,
    })
}

impl Store {
    /// Inserts or updates one node row, keyed by name.
    pub fn upsert_node(&self, node: &Node) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
               ssh_target = excluded.ssh_target,
               is_local = excluded.is_local,
               capabilities = excluded.capabilities,
               last_seen_at = excluded.last_seen_at",
            params![
                node.name.as_str(),
                node.ssh_target,
                bool_to_int(node.is_local),
                json_to_text(&node.capabilities)?,
                opt_ts_to_text(node.last_seen_at),
                ts_to_text(node.created_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the node called `name`, when it exists.
    pub fn node(&self, name: &NodeName) -> Result<Option<Node>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM main.nodes WHERE name = ?1"),
                [name.as_str()],
                read_node_row,
            )
            .optional()?;
        raw.map(node_from_row).transpose()
    }

    /// Returns the node called `name` or [`StoreError::NotFound`].
    pub fn require_node(&self, name: &NodeName) -> Result<Node, StoreError> {
        self.node(name)?
            .ok_or_else(|| StoreError::not_found("node", name.as_str()))
    }

    /// Returns this node, when it has been registered.
    pub fn local_node(&self) -> Result<Option<Node>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM main.nodes WHERE is_local = 1"),
                [],
                read_node_row,
            )
            .optional()?;
        raw.map(node_from_row).transpose()
    }

    /// Lists every known node, ordered by name.
    pub fn list_nodes(&self) -> Result<Vec<Node>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {NODE_COLUMNS} FROM main.nodes ORDER BY name"))?;
        let rows = statement.query_map([], read_node_row)?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(node_from_row(row?)?);
        }
        Ok(nodes)
    }

    /// Records that `name` was seen at `at`.
    pub fn touch_node_seen(&self, name: &NodeName, at: Timestamp) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.nodes SET last_seen_at = ?1 WHERE name = ?2",
            params![ts_to_text(at), name.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("node", name.as_str()));
        }
        Ok(())
    }

    /// Removes a node; the local node cannot be removed.
    pub fn remove_node(&self, name: &NodeName) -> Result<(), StoreError> {
        let node = self.require_node(name)?;
        if node.is_local {
            return Err(StoreError::Conflict("cannot remove the local node".to_owned()));
        }
        self.conn
            .execute("DELETE FROM main.nodes WHERE name = ?1", [name.as_str()])?;
        Ok(())
    }

    /// Inserts or updates one project row, keyed by name.
    ///
    /// A newly inserted row also gets its project database, created idempotently. Binding
    /// follows the spec's rules: an unbound handle binds the first non-reserved project (or
    /// `_node` when no real project exists), and a `_node`-bound handle re-binds once to the
    /// first real project. A handle bound to a real project never re-binds.
    pub fn upsert_project(&self, project: &Project) -> Result<(), StoreError> {
        let existed = self.project_row_exists(&project.name)?;
        let real_before = self.count_real_projects_excluding(Some(&project.name))?;
        self.conn.execute(
            "INSERT INTO main.projects (
                name, path, remote_url, default_branch, config_json, state, weight, paused_reason,
                archived_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, '', NULL, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
               path = excluded.path,
               remote_url = excluded.remote_url,
               default_branch = excluded.default_branch,
               config_json = excluded.config_json,
               updated_at = excluded.updated_at",
            params![
                project.name.as_str(),
                project.path,
                project.remote_url,
                project.default_branch,
                json_to_text(&project.config)?,
                ts_to_text(project.created_at),
                ts_to_text(project.updated_at),
            ],
        )?;

        if !existed {
            if let Some(paths) = &self.paths {
                Store::create_project_db(paths, &project.name)?;
            }
        }

        let bound_name = self.bound_project().cloned();
        let is_node = project.name.as_str() == crate::RESERVED_NODE_PROJECT;
        let should_bind = self.auto_bind
            && !existed
            && if is_node {
                bound_name.is_none() && real_before == 0
            } else {
                real_before == 0
                    && bound_name
                        .as_ref()
                        .is_none_or(|bound| bound.as_str() == crate::RESERVED_NODE_PROJECT)
            };
        if should_bind {
            match &self.paths {
                Some(paths) => self.attach_file(paths, &project.name)?,
                None => self.attach_memory(&project.name)?,
            }
        }
        Ok(())
    }

    /// Returns whether a project registry row with this name exists.
    pub(crate) fn project_row_exists(&self, name: &ProjectName) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM main.projects WHERE name = ?1", [name.as_str()], |_| {
                Ok(())
            })
            .optional()?
            .is_some())
    }

    /// Counts non-reserved project rows, optionally ignoring one name.
    fn count_real_projects_excluding(&self, except: Option<&ProjectName>) -> Result<u64, StoreError> {
        let count: i64 = match except {
            Some(name) => self.conn.query_row(
                "SELECT COUNT(*) FROM main.projects WHERE name <> ?1 AND name <> ?2",
                params![name.as_str(), crate::RESERVED_NODE_PROJECT],
                |row| row.get(0),
            )?,
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM main.projects WHERE name <> ?1",
                [crate::RESERVED_NODE_PROJECT],
                |row| row.get(0),
            )?,
        };
        Ok(count as u64)
    }

    /// Returns the project called `name`, when it exists.
    pub fn project(&self, name: &ProjectName) -> Result<Option<Project>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_COLUMNS} FROM main.projects WHERE name = ?1"),
                [name.as_str()],
                read_project_row,
            )
            .optional()?;
        raw.map(project_from_row).transpose()
    }

    /// Returns the project called `name` or [`StoreError::NotFound`].
    pub fn require_project(&self, name: &ProjectName) -> Result<Project, StoreError> {
        self.project(name)?
            .ok_or_else(|| StoreError::not_found("project", name.as_str()))
    }

    /// Lists every registered project, ordered by name.
    pub fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {PROJECT_COLUMNS} FROM main.projects ORDER BY name"))?;
        let rows = statement.query_map([], read_project_row)?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(project_from_row(row?)?);
        }
        Ok(projects)
    }
}
