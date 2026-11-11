//! Surfaces and leases: one live lease per session, heartbeat, takeover and stale sweep.

pub mod claude_remote;
pub mod follow;
pub mod lease;
pub mod mirror;
pub mod native;
pub mod opencode_host;
pub mod orchestrator;
pub mod takeover;

use vowp_protocol::Timestamp;
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

pub use lease::{AcquireLease, LeaseGuard, check_lease, check_lease_with, new_lease_token};
pub use mirror::ResumeContext;
pub use takeover::takeover;
pub use vowp_protocol::TakeoverReport;

/// Removes leases whose heartbeat is older than the stale window.
///
/// Delegates to [`lease::sweep_leases`], which deletes stale rows and reports the count.
pub fn sweep_leases(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError> {
    lease::sweep_leases(deps, store, now)
}
