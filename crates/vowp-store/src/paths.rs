//! The on-disk layout of one VOWP node.
//! Spec: `docs/design/crates/vowp-store.md` §3.1 and `docs/design/sessions.md` §1.
//!
//! [`NodePaths`] is a pure path calculator: construction performs no I/O and never reads
//! `$HOME`. Only [`NodePaths::from_env`] consults the environment, and only for the CLI and
//! supervisor entry points.

use std::path::{Path, PathBuf};

use vowp_protocol::{RunId, SessionId};

use crate::error::StoreError;

/// All files and directories beneath one node root (`~/.vowp` by default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePaths {
    root: PathBuf,
}

impl NodePaths {
    /// Builds the path set for `root` without touching the filesystem.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolves the node root from the environment.
    ///
    /// `VOWP_HOME` wins when set and non-empty; otherwise `$HOME/.vowp` is used. Returns
    /// [`StoreError::NoHome`] when neither variable is usable.
    pub fn from_env() -> Result<Self, StoreError> {
        if let Some(home) = std::env::var_os("VOWP_HOME").filter(|value| !value.is_empty()) {
            return Ok(Self::new(PathBuf::from(home)));
        }
        match std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            Some(home) => Ok(Self::new(PathBuf::from(home).join(".vowp"))),
            None => Err(StoreError::NoHome),
        }
    }

    /// Returns the node root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns `<root>/vowp.db`.
    pub fn db_path(&self) -> PathBuf {
        self.root.join("vowp.db")
    }

    /// Returns `<root>/projects`.
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// Returns `<root>/projects/<name>`.
    pub fn project_dir(&self, name: &vowp_protocol::ProjectName) -> PathBuf {
        self.projects_dir().join(name.as_str())
    }

    /// Returns `<root>/projects/<name>/project.db`.
    pub fn project_db_path(&self, name: &vowp_protocol::ProjectName) -> PathBuf {
        self.project_dir(name).join("project.db")
    }

    /// Returns `<root>/migration.json`, the restart manifest of the v0.1→v0.2 split.
    pub fn migration_manifest_path(&self) -> PathBuf {
        self.root.join("migration.json")
    }

    /// Returns `<root>/supervisor.lock`.
    pub fn supervisor_lock_path(&self) -> PathBuf {
        self.root.join("supervisor.lock")
    }

    /// Returns `<root>/sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Returns `<root>/sessions/<id>`.
    pub fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.sessions_dir().join(id.as_str())
    }

    /// Returns `<root>/sessions/<id>/log.jsonl`.
    pub fn log_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("log.jsonl")
    }

    /// Returns `<root>/sessions/<id>/.lock`.
    pub fn lock_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join(".lock")
    }

    /// Returns `<root>/sessions/<id>/blobs`.
    pub fn blobs_dir(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("blobs")
    }

    /// Returns `<root>/sessions/<id>/capsules`.
    pub fn capsules_dir(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("capsules")
    }

    /// Returns `<root>/sessions/<id>/adapters.json`.
    pub fn adapters_json_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("adapters.json")
    }

    /// Returns `<root>/sessions/<id>/manifest.json`.
    ///
    /// The file is written only by a full directory pull; `reindex` ignores it. A session
    /// directory without one is an ordinary local session.
    pub fn session_manifest_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("manifest.json")
    }

    /// Returns `<root>/logs` (supervisor log and other process output).
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Returns `<root>/runs` (raw harness output mirrors).
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Returns `<root>/runs/<run-id>.jsonl`.
    pub fn run_raw_path(&self, id: &RunId) -> PathBuf {
        self.runs_dir().join(format!("{id}.jsonl"))
    }

    /// Returns `<root>/backups`.
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// Returns `<root>/tmp` (atomic-write staging on the same filesystem).
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Creates the node root and the directories every node always has.
    ///
    /// Creates `root`, `sessions`, `logs`, `runs`, `backups` and `tmp`; idempotent.
    pub fn ensure(&self) -> Result<(), StoreError> {
        for dir in [
            self.root.clone(),
            self.sessions_dir(),
            self.logs_dir(),
            self.runs_dir(),
            self.backups_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
        }
        Ok(())
    }
}
