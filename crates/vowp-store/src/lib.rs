//! SQLite-backed storage, JSONL logs and blob handling for VOWP.
//!
//! One node keeps its database at `<root>/vowp.db` and its session content under
//! `<root>/sessions/<id>/`. This crate owns persistence only: SQLite schema and migrations,
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
pub mod paths;
pub mod reindex;
pub mod repo_jobs;
pub mod repo_ledger;
pub mod repo_loops;
pub mod repo_misc;
pub mod repo_nodes;
pub mod repo_runs;
pub mod repo_sessions;
pub mod session_files;
pub mod session_log;

pub use db::{
    CostGroup, CostQuery, CostRow, JobFilter, LedgerFilter, Request, RequestId, RunFinish, SCHEMA_VERSION,
    SessionFilter, StateChange, Store,
};
pub use error::StoreError;
pub use paths::NodePaths;
