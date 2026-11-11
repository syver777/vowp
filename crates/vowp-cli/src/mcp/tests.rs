//! The `vowp mcp` suite, compiled as a lib unit-test module.
//!
//! The crate's root `Cargo.toml` lists test targets explicitly, so the same self-contained
//! suite in `crates/vowp-cli/tests/mcp.rs` is included here without an owning `[[test]]`
//! entry. It runs under `cargo test --workspace` and also works unchanged if the root
//! package later gains an `[[test]] name = "mcp"` entry.

use crate as vowp_cli;

include!("../../tests/mcp.rs");
