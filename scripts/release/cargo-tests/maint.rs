//! Wrapper for the CLI maintenance integration suite.
//!
//! See `scripts/release/build.rs`: the shared suite is generated from
//! `crates/vowp-cli/tests/maint.rs` with its stale 0.1-series version
//! assertion updated to the package version. Every other test is unchanged.

include!(concat!(env!("OUT_DIR"), "/release-test-maint.rs"));
