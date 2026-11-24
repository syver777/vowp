//! Capacity measurement, admission, brakes, targets, fairness and advice.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6 and §13.2. The module measures host
//! capacity, admits jobs, applies brakes, sets targets, orders projects fairly and advises.

pub mod admit;
pub mod advisor;
pub mod brakes;
pub mod cluster;
pub mod estimates;
pub mod fair;
pub mod history;
pub mod reload;
pub mod sampler;
pub mod scope;
pub mod targets;
pub mod usage;

pub use admit::{CapacityManager, admit, request_for};
pub use advisor::{AdviceTrigger, maybe_advise};
pub use brakes::evaluate;
pub use cluster::build as cluster_capacity;
pub use estimates::{estimate_for, recompute};
pub use fair::order;
pub use history::{
    HistoryBucket, RAW_RETENTION_SECS, ROLLUP_BUCKET_SECS, ROLLUP_RETENTION_SECS, read as read_history, record_rollup,
};
pub use reload::{
    ConfigCell, ConfigFingerprint, ConfigPreview, ReloadOutcome, ReloadState, effective_config, pending, preview,
    provider_free, reload_project,
};
pub use sampler::{SamplerHandle, sample, sample_host};
pub use scope::{FakeProcFs, ProcFs, RunScope, ScopeSample, SystemProcFs};
pub use targets::{current, set};
pub use usage::record;
