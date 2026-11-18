//! Local surface state: `paired`, `needs_pair` and `revoked`.
//!
//! Spec: `docs/design/v0.4/hardening.md` §4.3 and §4.5. The state file lives at
//! `surfaces/<id>/state` (0600 in a 0700 directory). A refused or expired token marks the
//! surface `needs_pair` and drops it from the active expiry set, but the dead token file is
//! kept for inspection. The status document carries the surface id, a 12-character prefix
//! and the expiry, never the token.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vowp_protocol::{NodeName, SessionId, SurfaceId, Timestamp};

use crate::config::{AgentConfig, SURFACE_EXPIRY_FILE, SURFACE_TOKEN_FILE, ensure_private_dir, write_private};
use crate::error::AgentError;

/// File name inside `surfaces/<id>/` that records the local state.
pub const SURFACE_STATE_FILE: &str = "state";
/// Number of scheme-stripped token characters shown as a display prefix.
pub const SURFACE_PREFIX_CHARS: usize = 12;
/// Scheme every surface token carries.
const TOKEN_SCHEME: &str = "vowp_surf_";

/// One local surface state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceState {
    /// A token is stored and may be used.
    #[default]
    Paired,
    /// The home refused the token; the owner must re-pair.
    NeedsPair,
    /// The home revoked the surface; its token is gone.
    Revoked,
}

impl SurfaceState {
    /// Returns whether the stored token may still be presented to the home.
    pub fn is_active(self) -> bool {
        self == Self::Paired
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Paired => "paired",
            Self::NeedsPair => "needs_pair",
            Self::Revoked => "revoked",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "paired" => Some(Self::Paired),
            "needs_pair" => Some(Self::NeedsPair),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One row of the `vowp agent status` surface array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceStatus {
    /// Surface id.
    pub surface: SurfaceId,
    /// First 12 scheme-stripped token characters, when a token is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Active token expiry; `None` when the surface is `needs_pair` or `revoked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// Local state.
    pub state: SurfaceState,
}

impl SurfaceStatus {
    /// Returns whether the surface may present a token to the home.
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }
}

/// A refusal the agent can show: the surface id, state and a recovery message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRefusal {
    /// Surface whose token was refused.
    pub surface: SurfaceId,
    /// State written locally (`needs_pair`).
    pub state: SurfaceState,
    /// Message naming the surface, the reason and the recovery commands; never the token.
    pub message: String,
}

/// Returns the display prefix for a token: 12 scheme-stripped characters.
pub fn token_prefix(token: &str) -> Option<String> {
    let stripped = token.trim().strip_prefix(TOKEN_SCHEME).unwrap_or(token.trim());
    let prefix: String = stripped.chars().take(SURFACE_PREFIX_CHARS).collect();
    if prefix.is_empty() { None } else { Some(prefix) }
}

/// Returns the local state recorded for one surface.
///
/// A missing or unreadable state file is `paired` for backward compatibility with agents
/// written before this file existed; a directory without a token and without a state file is
/// `needs_pair` because a pairing never completed.
pub fn state_of(state_dir: &Path, surface: &SurfaceId) -> SurfaceState {
    let dir = AgentConfig::surface_dir(state_dir, surface);
    state_of_dir(&dir)
}

/// Returns the local state recorded in one surface directory.
pub fn state_of_dir(dir: &Path) -> SurfaceState {
    if let Ok(text) = fs::read_to_string(dir.join(SURFACE_STATE_FILE))
        && let Some(state) = SurfaceState::parse(&text)
    {
        return state;
    }
    if dir.join(SURFACE_TOKEN_FILE).is_file() {
        SurfaceState::Paired
    } else {
        SurfaceState::NeedsPair
    }
}

/// Returns every local surface with its prefix, active expiry and state.
///
/// Sorted by surface id so the status document is stable. A token value is never copied.
pub fn surface_statuses(state_dir: &Path) -> Vec<SurfaceStatus> {
    let entries = match fs::read_dir(AgentConfig::surfaces_dir(state_dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return Vec::new(),
    };
    let mut statuses = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let surface = SurfaceId(name.to_owned());
        let state = state_of_dir(&path);
        let token = fs::read_to_string(path.join(SURFACE_TOKEN_FILE)).ok();
        let prefix = token.as_deref().and_then(token_prefix);
        let expires_at = if state.is_active() {
            fs::read_to_string(path.join(SURFACE_EXPIRY_FILE))
                .ok()
                .and_then(|text| Timestamp::parse(text.trim()).ok())
        } else {
            None
        };
        statuses.push(SurfaceStatus {
            surface,
            prefix,
            expires_at,
            state,
        });
    }
    statuses.sort_by(|left, right| left.surface.0.cmp(&right.surface.0));
    statuses
}

/// Marks one surface `needs_pair` and returns the refusal message for the status line.
///
/// The token and expiry files are deliberately kept: the dead token makes the failure
/// inspectable and is never presented again because [`state_of_dir`] now reports `needs_pair`
/// and [`crate::config::paired_token_for_session`] skips it.
pub fn mark_needs_pair(state_dir: &Path, surface: &SurfaceId, reason: &str) -> Result<SurfaceRefusal, AgentError> {
    let dir = AgentConfig::surface_dir(state_dir, surface);
    ensure_private_dir(&dir)?;
    write_state(&dir, SurfaceState::NeedsPair)?;
    Ok(SurfaceRefusal {
        surface: surface.clone(),
        state: SurfaceState::NeedsPair,
        message: refusal_message(state_dir, surface, reason),
    })
}

/// Marks one surface `paired` after a successful pairing or rotation.
pub fn mark_paired(state_dir: &Path, surface: &SurfaceId) -> Result<(), AgentError> {
    let dir = AgentConfig::surface_dir(state_dir, surface);
    ensure_private_dir(&dir)?;
    write_state(&dir, SurfaceState::Paired)
}

/// Marks one surface `revoked`; the caller has already deleted the token.
pub fn mark_revoked(state_dir: &Path, surface: &SurfaceId) -> Result<(), AgentError> {
    let dir = AgentConfig::surface_dir(state_dir, surface);
    ensure_private_dir(&dir)?;
    write_state(&dir, SurfaceState::Revoked)
}

fn write_state(dir: &Path, state: SurfaceState) -> Result<(), AgentError> {
    write_private(&dir.join(SURFACE_STATE_FILE), state.as_str().as_bytes())
}

/// Returns the owner recovery commands for one refused surface.
///
/// The node and session are filled when the agent knows them. The command names and the
/// ordering are the exact recovery path in §4.5; no token appears.
pub fn recovery_commands(surface: &SurfaceId, node: Option<&NodeName>, session: Option<&SessionId>) -> String {
    let node = node.map(ToString::to_string).unwrap_or_else(|| "<name>".to_owned());
    let session = session
        .map(ToString::to_string)
        .unwrap_or_else(|| "<session>".to_owned());
    format!(
        "`vowp surface revoke {surface}`, then `vowp surface pair --session {session} --node {node} --kind <kind>`, \
         then `vowp agent pair --code-file - --surface <new-id> --session {session}`"
    )
}

fn refusal_message(state_dir: &Path, surface: &SurfaceId, reason: &str) -> String {
    let session = bound_session(state_dir, surface);
    format!(
        "surface {surface} needs pairing: {reason}; recover with {}",
        recovery_commands(surface, None, session.as_ref())
    )
}

/// Reads the session binding stored beside one surface, when present.
pub fn bound_session(state_dir: &Path, surface: &SurfaceId) -> Option<SessionId> {
    let text = fs::read_to_string(AgentConfig::surface_session_path(state_dir, surface)).ok()?;
    SessionId::parse(text.trim()).ok()
}

/// Reads the recorded expiry of one surface without reporting it as active.
pub fn stored_expiry(state_dir: &Path, surface: &SurfaceId) -> Option<Timestamp> {
    fs::read_to_string(AgentConfig::surface_expiry_path(state_dir, surface))
        .ok()
        .and_then(|text| Timestamp::parse(text.trim()).ok())
}

/// Returns whether a state file says the surface must not present its token.
pub fn is_dead(state_dir: &Path, surface: &SurfaceId) -> bool {
    !state_of(state_dir, surface).is_active()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface() -> SurfaceId {
        SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned())
    }

    #[test]
    fn state_round_trips_and_defaults_to_paired() {
        let dir = tempfile::tempdir().expect("temp dir");
        let surface = surface();
        assert_eq!(state_of(dir.path(), &surface), SurfaceState::NeedsPair);
        mark_paired(dir.path(), &surface).expect("paired");
        assert_eq!(state_of(dir.path(), &surface), SurfaceState::Paired);
        mark_needs_pair(dir.path(), &surface, "the token expired").expect("needs_pair");
        assert_eq!(state_of(dir.path(), &surface), SurfaceState::NeedsPair);
        mark_revoked(dir.path(), &surface).expect("revoked");
        assert_eq!(state_of(dir.path(), &surface), SurfaceState::Revoked);
    }

    #[test]
    fn prefix_strips_the_scheme_and_keeps_twelve_characters() {
        let token = format!("{TOKEN_SCHEME}{}", "A".repeat(43));
        assert_eq!(token_prefix(&token).as_deref(), Some("AAAAAAAAAAAA"));
        assert_eq!(token_prefix("short").as_deref(), Some("short"));
        assert_eq!(token_prefix(""), None);
    }

    #[test]
    fn statuses_never_carry_the_token() {
        let dir = tempfile::tempdir().expect("temp dir");
        let surface = surface();
        let token = format!("{TOKEN_SCHEME}{}", "B".repeat(43));
        let path = AgentConfig::surface_dir(dir.path(), &surface);
        ensure_private_dir(&path).expect("surface dir");
        write_private(&path.join(SURFACE_TOKEN_FILE), token.as_bytes()).expect("token");
        write_private(&path.join(SURFACE_EXPIRY_FILE), b"2027-01-01T00:00:00Z").expect("expiry");
        mark_paired(dir.path(), &surface).expect("paired");

        let statuses = surface_statuses(dir.path());
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].surface, surface);
        assert_eq!(statuses[0].prefix.as_deref(), Some("BBBBBBBBBBBB"));
        assert!(statuses[0].expires_at.is_some());
        let json = serde_json::to_string(&statuses).expect("json");
        assert!(
            !json.contains(&token),
            "the status array must not carry a token: {json}"
        );

        mark_needs_pair(dir.path(), &surface, "revoked by the home").expect("needs_pair");
        let statuses = surface_statuses(dir.path());
        assert_eq!(statuses[0].state, SurfaceState::NeedsPair);
        assert!(
            statuses[0].expires_at.is_none(),
            "a needs_pair surface is not in the active expiry set"
        );
    }

    #[test]
    fn a_refusal_names_the_commands_and_never_the_token() {
        let dir = tempfile::tempdir().expect("temp dir");
        let surface = surface();
        let refusal = mark_needs_pair(dir.path(), &surface, "the token was refused").expect("refusal");
        assert!(refusal.message.contains("needs pairing"), "{}", refusal.message);
        assert!(refusal.message.contains("vowp surface revoke"), "{}", refusal.message);
        assert!(refusal.message.contains("vowp surface pair"), "{}", refusal.message);
        assert!(refusal.message.contains("vowp agent pair"), "{}", refusal.message);
        assert!(!refusal.message.contains("vowp_surf_"), "{}", refusal.message);
    }
}
