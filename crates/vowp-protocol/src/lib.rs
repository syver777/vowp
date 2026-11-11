//! VOWP protocol types: events, capsules, jobs, gates, profiles and configuration.
//!
//! Every wire/data type used by the workspace is defined here, once, with serde and
//! JSON Schema support. Spec: `docs/design/types.md` and `docs/design/crates/vowp-protocol.md`.
//!
//! The crate root re-exports every public type, so downstream crates import one set of
//! definitions from `vowp_protocol`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod adapter;
pub mod capacity;
pub mod capsule;
pub mod catalog;
pub mod config;
pub mod cost;
pub mod error;
pub mod event;
pub mod executor;
pub mod gate;
pub mod ids;
pub mod inbox;
pub mod job;
pub mod limit;
pub mod log;
pub mod loops;
pub mod ops;
pub mod release;
pub mod report;
pub mod run;
pub mod schema;
pub mod services;
pub mod session;
pub mod snapshot;
pub mod surface;
pub mod time;
pub mod vault;

// Re-exports so every public type is available at the crate root.
#[allow(unused_imports)]
pub use adapter::*;
#[allow(unused_imports)]
pub use capacity::*;
#[allow(unused_imports)]
pub use capsule::*;
#[allow(unused_imports)]
pub use catalog::*;
#[allow(unused_imports)]
pub use config::*;
pub use cost::*;
pub use error::*;
#[allow(unused_imports)]
pub use event::*;
pub use executor::*;
#[allow(unused_imports)]
pub use gate::*;
pub use ids::*;
#[allow(unused_imports)]
pub use inbox::*;
#[allow(unused_imports)]
pub use job::*;
pub use limit::*;
pub use log::*;
#[allow(unused_imports)]
pub use loops::*;
#[allow(unused_imports)]
pub use ops::*;
#[allow(unused_imports)]
pub use release::*;
#[allow(unused_imports)]
pub use report::*;
#[allow(unused_imports)]
pub use run::*;
#[allow(unused_imports)]
pub use schema::*;
#[allow(unused_imports)]
pub use services::*;
#[allow(unused_imports)]
pub use session::*;
#[allow(unused_imports)]
pub use snapshot::*;
#[allow(unused_imports)]
pub use surface::*;
pub use time::*;
#[allow(unused_imports)]
pub use vault::*;

/// Tool output strictly larger than this many bytes is spilled to a blob.
pub const BLOB_SPILL_BYTES: usize = 8192;

/// Number of leading characters kept as a preview for a spilled blob.
pub const BLOB_PREVIEW_CHARS: usize = 512;

/// Protocol wire-format version; bumped when a wire shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Common imports for code that works with protocol values.
///
/// `use vowp_protocol::prelude::*;` brings in the id types, [`Timestamp`], [`Clock`],
/// [`SystemClock`], [`FakeClock`], [`Executor`], [`ExecutorRef`], [`HarnessKind`],
/// [`Role`], [`Cost`], [`CostSource`], [`Tokens`], [`LimitKind`], [`ErrorKind`],
/// [`ProtocolError`], [`Log`] and [`Level`], plus the event, capsule, job and gate types
/// from the other protocol modules.
#[allow(unused_imports)]
pub mod prelude {
    pub use crate::capsule::*;
    pub use crate::cost::*;
    pub use crate::error::*;
    pub use crate::event::*;
    pub use crate::executor::*;
    pub use crate::gate::*;
    pub use crate::ids::*;
    pub use crate::job::*;
    pub use crate::limit::*;
    pub use crate::log::*;
    pub use crate::time::*;
}
