//! The `vowp` binary.
//!
//! Everything except argument wiring lives in `vowp-cli`'s library so commands are tested
//! in-process. Spec: `docs/design/crates/vowp-cli.md` §1.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut io = vowp_cli::RealIo::new();
    let code = vowp_cli::run(std::env::args().collect(), &mut io);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
