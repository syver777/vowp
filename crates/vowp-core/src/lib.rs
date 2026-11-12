//! VOWP supervisor core: loops, gates, ledger, schedules, sessions and promotion.
//!
//! The crate composes the protocol types, the store and the adapter registry into the
//! supervisor's deterministic loops and the shared services the CLI calls.
//! Spec: `docs/design/crates/vowp-core.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod adapters;

pub mod capacity;
pub mod deps;
pub mod error;
pub mod fleet;
pub mod gates;
pub mod ledger;
pub mod lifecycle;
pub mod loops;
pub mod promote;
pub mod requests;
pub mod schedules;
pub mod session;
pub mod surface;
pub mod vault;

pub use capacity::CapacityManager;
pub use deps::{
    AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, Deps, FixedRng, GitOps, RebaseResult, Rng, SystemGit,
    SystemRunner,
};
pub use error::CoreError;
pub use fleet::{build_snapshot, read_snapshot, write_snapshot};
pub use lifecycle::{BackupOptions, GcOptions, backup, gc};
pub use requests::Request;
pub use surface::{LeaseGuard, takeover};
pub use vault::Redactor;
pub use vowp_protocol::VaultMode;
