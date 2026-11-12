//! Integration test for the `vowp --version` output.
//!
//! It checks that `vowp --version` succeeds, prints `vowp <package version>`, and reports
//! the expected package version `0.3.0`.

use std::process::Command;

#[test]
fn version_flag_prints_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_vowp"))
        .arg("--version")
        .output()
        .expect("failed to run vowp --version");

    assert!(
        output.status.success(),
        "vowp --version exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("vowp {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.0");
}
