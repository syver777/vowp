//! The `vowp-daemon` binary.
//!
//! Usage: `vowp-daemon [--home <dir>] [--project <name>] [--port <n>]`. The listener is
//! always `127.0.0.1`; port zero asks the operating system for an ephemeral port. The
//! bound address is printed once, and nothing else at startup. `VOWP_API_TOKEN` enables the
//! inbox answer and executor switch writes; missing or empty leaves reads available and
//! writes disabled. The binary never prints a home path, token or secret.

use std::net::Ipv4Addr;
use std::process::ExitCode;

use vowp_daemon::{
    ApiState, ApiToken, DaemonError, Invocation, Options, USAGE, build_deps, parse_invocation, select_project, serve,
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vowp-daemon: {error}");
            if matches!(error, DaemonError::Config(_)) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// Parses arguments, selects the project and serves until the process is stopped.
async fn run() -> Result<(), DaemonError> {
    let options = match parse_invocation(std::env::args_os().skip(1))? {
        Invocation::Serve(options) => options,
        Invocation::Help => {
            print!("{USAGE}");
            return Ok(());
        }
    };
    let paths = options.paths()?;
    let selection = select_project(&paths, options.project.as_deref())?;
    let deps = build_deps(&paths, &selection)?;
    let state = ApiState::new(&paths, &selection)
        .with_api_token(ApiToken::from_env())
        .with_session_deps(deps);
    let listener = bind(options).await?;
    let address = listener.local_addr().map_err(DaemonError::Serve)?;
    println!("vowp-daemon: listening on http://{address}");
    serve(listener, state).await.map_err(DaemonError::Serve)
}

/// Binds the loopback listener for `options.port`.
async fn bind(options: Options) -> Result<tokio::net::TcpListener, DaemonError> {
    tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, options.port))
        .await
        .map_err(DaemonError::Serve)
}
