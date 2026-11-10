//! VOWP supervisor core: loops, gates, ledger, schedules, sessions and promotion.
//!
//! The crate composes the protocol types, the store and the adapter registry into the
//! supervisor's deterministic loops and the shared services the CLI calls.
//! Spec: `docs/design/crates/vowp-core.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod deps;
pub mod error;
pub mod gates;
pub mod ledger;
pub mod loops;
pub mod promote;
pub mod requests;
pub mod schedules;
pub mod session;

pub use deps::{
    AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, Deps, FixedRng, GitOps, RebaseResult, Rng, SystemGit,
    SystemRunner,
};
pub use error::CoreError;
pub use requests::Request;
