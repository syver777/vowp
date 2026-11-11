//! SQLite-backed storage, JSONL logs and blob handling for VOWP.
//!
//! One node keeps its global database at `<root>/vowp.db`, one database per project at
//! `<root>/projects/<name>/project.db`, and its session content under `<root>/sessions/<id>/`.
//! This crate owns persistence only: the split schema and migration, cross-file reconcile,
//! typed row access for every table, the append-only session log, content-addressed blobs and
//! `reindex`. It knows nothing about job-state legality, loops or harnesses.
//!
//! Spec: `docs/design/crates/vowp-store.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod blobs;
mod convert;
pub mod db;
pub mod error;
pub mod migrate;
pub mod paths;
pub mod reconcile;
pub mod reindex;
pub mod repo_capacity;
pub mod repo_catalog;
pub mod repo_jobs;
pub mod repo_ledger;
pub mod repo_loops;
pub mod repo_misc;
pub mod repo_nodes;
pub mod repo_projects;
pub mod repo_runs;
pub mod repo_secrets;
pub mod repo_sessions;
pub mod repo_surfaces;
pub mod session_files;
pub mod session_log;

pub use db::{
    CostGroup, CostQuery, CostRow, GLOBAL_SCHEMA_VERSION, JobFilter, LedgerFilter, PROJECT_SCHEMA_VERSION, Request,
    RequestId, RunFinish, SCHEMA_VERSION, SessionFilter, StateChange, Store,
};
pub use error::StoreError;
pub use migrate::{
    MigrationCount, MigrationReport, migrate_rollback, migrate_v01, migrate_v01_dry_run, resume_migration,
};
pub use paths::NodePaths;
pub use reconcile::{CrossDbReconcile, ReconcileCounts};

/// The reserved project that owns project-less session runs and node-wide history.
pub const RESERVED_NODE_PROJECT: &str = "_node";
