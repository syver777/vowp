//! Harness adapters: claude-code, opencode, generic cli. Spec: docs/design/adapters.md
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod classify;
pub mod claude_code;
pub mod cli;
pub mod opencode;
pub mod proc;
pub mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use claude_code::{ClaudeCodeAdapter, ClaudeConfig};
pub use cli::{CliAdapter, Mapping};
pub use opencode::{OpencodeAdapter, OpencodeConfig};
