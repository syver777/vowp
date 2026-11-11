//! Wrapper for the CLI control-machine integration suite.
//!
//! See `scripts/release/build.rs`: the shared suite is generated from
//! `crates/vowp-cli/tests/control.rs` with its stale 0.1-series version
//! assertion updated to the package version. Every other test is unchanged.

include!(concat!(env!("OUT_DIR"), "/release-test-control.rs"));
