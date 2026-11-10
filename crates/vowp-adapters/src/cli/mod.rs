//! Generic CLI harness adapter driven by a TOML mapping file.
//!
//! Spec: `docs/design/adapters.md` §4 and `docs/design/crates/vowp-adapters.md` §4.3.
//! A mapping names the command, how the prompt is delivered, how JSON lines or plain text
//! become canonical events, which texts mean a provider limit, and where cost lives.
//! Everything here is process-free except [`CliAdapter::start`], which spawns one run.

mod adapter;
mod mapping;
mod parser;

pub use adapter::CliAdapter;
pub use mapping::{
    CommandSpec, CostSpec, ExitSpec, LimitSpec, MapRule, Mapping, Match, OutputFormat, OutputSpec, PromptVia,
    ResetKind, ResetSpec,
};
pub use parser::{CliParser, pointer_get};
