//! Append-only ledger of dispatches, gates, costs and state changes, plus the board and
//! night-report views built from it. Spec: `docs/design/crates/vowp-core.md` §5.

pub mod board;
pub mod price;
pub mod record;
pub mod report;

pub use board::{BoardRun, BoardSnapshot};
pub use record::Ledger;
