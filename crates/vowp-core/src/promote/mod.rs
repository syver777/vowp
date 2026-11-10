//! Guarded promotion and rollback of finished work.
//! Spec: `docs/design/crates/vowp-core.md` §8 and `docs/design/concepts/work-to-production.md`.
//!
//! [`Promoter`] runs guards, banks the serving state, refuses a diff outside the intended
//! change, asks for human approval by default, deploys, probes and records every terminal
//! release. [`bank`] snapshots the serving state and [`guards`] evaluates the built-in and
//! custom checks.

pub mod bank;
pub mod guards;
pub mod promoter;
pub mod releases_log;

pub use promoter::{PromoteOpts, Promoter};
