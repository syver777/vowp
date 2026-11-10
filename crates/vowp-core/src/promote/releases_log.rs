//! Append-only `releases.log`, the operator-facing history of promotions and rollbacks.
//! Spec: `docs/design/crates/vowp-core.md` §8 rule 5.
//!
//! Every release that reaches a terminal state appends one line:
//! `<ts> <env> <release id> <state> <from>-><to> rollback: <cmd>`. The file lives beside the
//! node database (`<node root>/releases.log`) and is never rewritten.

use std::io::Write;

use vowp_protocol::{Release, ReleaseState};

use crate::CoreError;
use crate::deps::Deps;

/// Appends one line for `release` at the injected clock's current instant.
pub fn append(deps: &Deps, release: &Release) -> Result<(), CoreError> {
    let path = deps.paths.root().join("releases.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
    writeln!(file, "{}", line(deps.clock.now(), release)).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Formats the release-log line without its trailing newline.
fn line(ts: vowp_protocol::Timestamp, release: &Release) -> String {
    format!(
        "{ts} {} {} {} {}->{} rollback: {}",
        release.env,
        release.id,
        state_name(release.state),
        release.from_commit.as_deref().unwrap_or("-"),
        release.to_commit.as_deref().unwrap_or("-"),
        release.rollback_cmd.as_deref().unwrap_or("-"),
    )
}

/// Returns the canonical snake_case name of a release state.
fn state_name(state: ReleaseState) -> &'static str {
    match state {
        ReleaseState::Planned => "planned",
        ReleaseState::Guarded => "guarded",
        ReleaseState::Banked => "banked",
        ReleaseState::Approved => "approved",
        ReleaseState::Deploying => "deploying",
        ReleaseState::Probing => "probing",
        ReleaseState::Live => "live",
        ReleaseState::Failed => "failed",
        ReleaseState::RolledBack => "rolled_back",
        ReleaseState::Refused => "refused",
    }
}
