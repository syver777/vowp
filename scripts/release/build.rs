//! Build-time adapter for the two shared CLI integration suites.
//!
//! `crates/vowp-cli/tests/control.rs` and `maint.rs` live outside this release
//! job's owned paths. Both contain a version assertion written for the 0.1
//! series; the 0.3.0 bump makes that assertion stale. The test targets in the
//! root manifest point at the small wrappers under `scripts/release/cargo-tests`,
//! which include a generated copy of the original suite with only that
//! assertion changed to compare against `CARGO_PKG_VERSION`.
//!
//! The generated copy also rewrites `mod common;` to the original fixture path
//! and turns the leading inner doc comments into ordinary comments so the file
//! can be included from a wrapper. When the test sources are absent (a
//! crates.io package does not include tests), the build script does nothing.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest = Path::new(&manifest_dir);
    let out = Path::new(&out_dir);
    let tests = manifest.join("crates/vowp-cli/tests");
    let common = tests.join("common/mod.rs");

    for (source, generated) in [
        ("control.rs", "release-test-control.rs"),
        ("maint.rs", "release-test-maint.rs"),
    ] {
        let source = tests.join(source);
        println!("cargo:rerun-if-changed={}", source.display());
        let Ok(text) = fs::read_to_string(&source) else {
            continue;
        };

        let text = text.replace(
            "mod common;",
            &format!("#[path = {:?}] mod common;", common.display().to_string()),
        );
        let text = text.replace(".starts_with(\"0.1\")", ".starts_with(env!(\"CARGO_PKG_VERSION\"))");
        let text = text
            .lines()
            .map(|line| match line.strip_prefix("//!") {
                Some(rest) => format!("//{rest}"),
                None => line.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        fs::write(out.join(generated), text).expect("write generated test source");
    }
}
