//! Cron-based job schedules (L9), with missed-run policies and the three schedule actions.
//! Spec: `docs/design/crates/vowp-core.md` §6 and `docs/design/loops.md` §11.

pub mod engine;
pub mod spec;

pub use engine::{DueRun, FireOutcome, ScheduleEngine};
pub use spec::ScheduleSpec;
