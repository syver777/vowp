//! Agent configuration and private file helpers.
//!
//! The state directory is `~/.vowp-agent` by default. `VOWP_AGENT_HOME` overrides it for tests
//! and portable installs. `agent.json` and `status.json` are created 0600; the directory is
//! created 0700. Paired surface tokens live below `surfaces/<id>/token`, also 0600, in a
//! 0700 directory.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use vowp_protocol::{AgentPathMap, MAX_PATH_REF_LEN, NodeName, ProjectName, SessionId, SurfaceId};

use crate::error::AgentError;

/// One paired surface token, kept in memory for one chat process.
///
/// `Debug` redacts the token so a status line or an error cannot print it.
#[derive(Clone, PartialEq, Eq)]
pub struct PairedToken {
    /// Surface the token belongs to.
    pub surface_id: SurfaceId,
    /// Plaintext `vowp_surf_` token; never logged.
    pub token: String,
}

impl std::fmt::Debug for PairedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairedToken")
            .field("surface_id", &self.surface_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Returns the paired surface token for `session`, when one is stored.
///
/// The binding lives in `surfaces/<surface>/session`; `vowp agent pair` writes it next to
/// the token. A directory without a binding is ignored, so tokens from older agents remain
/// readable without being attributed to the wrong session.
pub fn paired_token_for_session(state_dir: &Path, session: &SessionId) -> Result<Option<PairedToken>, AgentError> {
    let dir = AgentConfig::surfaces_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AgentError::Io(error)),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // A refused or revoked surface keeps its dead token for inspection but must never be
        // presented again; only a re-pair clears the state.
        if !crate::surface_state::state_of_dir(&path).is_active() {
            continue;
        }
        let bound = match fs::read_to_string(path.join(SURFACE_SESSION_FILE)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(AgentError::Io(error)),
        };
        if bound.trim() != session.as_str() {
            continue;
        }
        let token = fs::read_to_string(path.join(SURFACE_TOKEN_FILE))?.trim().to_owned();
        if token.is_empty() {
            return Err(AgentError::Config(format!(
                "surface token at {} is empty",
                path.join(SURFACE_TOKEN_FILE).display()
            )));
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        found.push(PairedToken {
            surface_id: SurfaceId(name.to_owned()),
            token,
        });
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(AgentError::Config(format!(
            "more than one paired surface is bound to session {session}"
        ))),
    }
}

/// Writes the session binding for a paired surface.
pub(crate) fn write_surface_binding(
    state_dir: &Path,
    surface: &SurfaceId,
    session: &SessionId,
) -> Result<(), AgentError> {
    ensure_private_dir(state_dir)?;
    ensure_private_dir(&AgentConfig::surfaces_dir(state_dir))?;
    ensure_private_dir(&AgentConfig::surface_dir(state_dir, surface))?;
    write_private(
        &AgentConfig::surface_session_path(state_dir, surface),
        session.as_str().as_bytes(),
    )
}

/// Default directory name below `$HOME`.
pub const AGENT_DIR_NAME: &str = ".vowp-agent";
/// Enrollment config file name.
pub const CONFIG_FILE: &str = "agent.json";
/// Last reported status file name.
pub const STATUS_FILE: &str = "status.json";
/// Directory holding one subdirectory per paired surface.
pub const SURFACES_DIR: &str = "surfaces";
/// Secret surface token file name inside a surface directory.
pub const SURFACE_TOKEN_FILE: &str = "token";
/// Session binding written when the surface was paired.
pub const SURFACE_SESSION_FILE: &str = "session";
/// RFC 3339 surface-token expiry file name inside a surface directory.
pub const SURFACE_EXPIRY_FILE: &str = "expires_at";
/// Default worktree directory below a project checkout.
pub const DEFAULT_WORKTREE_DIR: &str = ".vowp/worktrees";

/// The persisted enrollment and transport settings of this agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Home identity returned by `Enrolled.home_id`.
    pub home: String,
    /// Node name assigned by the home.
    pub node: NodeName,
    /// Local state directory (`~/.vowp-agent`).
    pub state_dir: PathBuf,
    /// How to reach the home.
    pub transport: TransportConfig,
    /// Projects this node can run in, with their local roots and allow lists.
    ///
    /// Absolute roots live only in this 0600 file and in the `PathMapUpdate`/`Enroll` frames
    /// the home stores in its owner-only `node_paths` rows.
    #[serde(default)]
    pub path_maps: Vec<AgentPathMap>,
}

/// The transport section of [`AgentConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportConfig {
    /// The Phase 0 SSH stdio gateway.
    Ssh {
        /// ssh destination of the home.
        target: String,
    },
    /// The optional WSS ingress through the owner's TLS reverse proxy.
    ///
    /// Spec: `docs/design/v0.4/hardening.md` §2.2. TLS is the proxy's job; the agent uses
    /// the system trust store by default, or a hard `sha256:` pin for a self-signed
    /// deployment. The first system-mode connection records the leaf fingerprint in this
    /// file; a later change is refused until the owner confirms it.
    Wss {
        /// `wss://host[:port]/agent/v1` URL.
        url: String,
        /// `system` (default) or `pin`.
        #[serde(default)]
        tls: WssTlsMode,
        /// Required for `tls = "pin"`; also stores the first system-mode observation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
}

/// How the WSS transport verifies the proxy's certificate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WssTlsMode {
    /// System trust store plus hostname verification, with a recorded leaf fingerprint.
    #[default]
    System,
    /// A hard pin for a self-signed deployment.
    Pin,
}

impl AgentConfig {
    /// Builds an SSH-backed config.
    pub fn new(home: impl Into<String>, node: NodeName, state_dir: PathBuf, target: impl Into<String>) -> Self {
        Self {
            home: home.into(),
            node,
            state_dir,
            transport: TransportConfig::Ssh { target: target.into() },
            path_maps: Vec::new(),
        }
    }

    /// Replaces the mapped projects.
    pub fn with_path_maps(mut self, path_maps: Vec<AgentPathMap>) -> Self {
        self.path_maps = path_maps;
        self
    }

    /// Returns the default worktree root for a checkout: `<local_root>/.vowp/worktrees`.
    pub fn default_worktree_root(local_root: &Path) -> PathBuf {
        local_root.join(DEFAULT_WORKTREE_DIR)
    }

    /// Returns every mapped project.
    pub fn path_maps(&self) -> &[AgentPathMap] {
        &self.path_maps
    }

    /// Returns the map for one project, when it is mapped.
    pub fn path_map(&self, project: &ProjectName) -> Option<&AgentPathMap> {
        self.path_maps.iter().find(|map| &map.project == project)
    }

    /// Adds or replaces one project map and validates the resulting map set.
    ///
    /// The change stays in memory until [`AgentConfig::write`] persists it.
    pub fn set_path_map(&mut self, map: AgentPathMap) -> Result<(), AgentError> {
        let mut path_maps = self.path_maps.clone();
        match path_maps.iter_mut().find(|entry| entry.project == map.project) {
            Some(entry) => *entry = map,
            None => path_maps.push(map),
        }
        Self::validate_path_maps(&self.state_dir, &path_maps)?;
        self.path_maps = path_maps;
        Ok(())
    }

    /// Removes one project map. Returns false when the project was not mapped.
    pub fn remove_path_map(&mut self, project: &ProjectName) -> bool {
        let before = self.path_maps.len();
        self.path_maps.retain(|map| &map.project != project);
        self.path_maps.len() != before
    }

    /// Returns the default state directory (`$VOWP_AGENT_HOME`, else `$HOME/.vowp-agent`).
    pub fn default_state_dir() -> Result<PathBuf, AgentError> {
        if let Some(dir) = std::env::var_os("VOWP_AGENT_HOME") {
            return Ok(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| AgentError::Config("HOME is not set; pass VOWP_AGENT_HOME".to_owned()))?;
        Ok(PathBuf::from(home).join(AGENT_DIR_NAME))
    }

    /// Returns `state_dir/agent.json`.
    pub fn file_path(state_dir: &Path) -> PathBuf {
        state_dir.join(CONFIG_FILE)
    }

    /// Returns `state_dir/surfaces`.
    pub fn surfaces_dir(state_dir: &Path) -> PathBuf {
        state_dir.join(SURFACES_DIR)
    }

    /// Returns `state_dir/surfaces/<id>`.
    pub fn surface_dir(state_dir: &Path, surface: &SurfaceId) -> PathBuf {
        Self::surfaces_dir(state_dir).join(&surface.0)
    }

    /// Returns `state_dir/surfaces/<id>/token`.
    pub fn surface_token_path(state_dir: &Path, surface: &SurfaceId) -> PathBuf {
        Self::surface_dir(state_dir, surface).join(SURFACE_TOKEN_FILE)
    }

    /// Returns `state_dir/surfaces/<id>/session`.
    pub fn surface_session_path(state_dir: &Path, surface: &SurfaceId) -> PathBuf {
        Self::surface_dir(state_dir, surface).join(SURFACE_SESSION_FILE)
    }

    /// Returns `state_dir/surfaces/<id>/expires_at`.
    pub fn surface_expiry_path(state_dir: &Path, surface: &SurfaceId) -> PathBuf {
        Self::surface_dir(state_dir, surface).join(SURFACE_EXPIRY_FILE)
    }

    /// Loads `agent.json` from `state_dir`.
    pub fn load(state_dir: &Path) -> Result<Self, AgentError> {
        let path = Self::file_path(state_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(AgentError::NotEnrolled(format!("{} is missing", path.display())));
            }
            Err(error) => return Err(AgentError::Io(error)),
        };
        let config: Self = serde_json::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Writes `agent.json` atomically enough for one local process, mode 0600.
    pub fn write(&self) -> Result<(), AgentError> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_private(&Self::file_path(&self.state_dir), &bytes)
    }

    /// Returns the configured SSH target.
    pub fn ssh_target(&self) -> Result<&str, AgentError> {
        match &self.transport {
            TransportConfig::Ssh { target } => {
                if target.trim().is_empty() {
                    Err(AgentError::Config("ssh target must not be empty".to_owned()))
                } else {
                    Ok(target)
                }
            }
            TransportConfig::Wss { .. } => Err(AgentError::Config(
                "agent.transport is wss; no ssh target is configured".to_owned(),
            )),
        }
    }

    /// Returns the configured transport.
    pub fn transport(&self) -> &TransportConfig {
        &self.transport
    }

    /// Validates a project path-map set before it is persisted or sent.
    ///
    /// The rules match the home's `validate_paths` and the worktree resolver: unique project
    /// names, absolute UTF-8 roots without `..` or NUL, an existing local checkout, a worktree
    /// root that exists (or is created when it is below the agent state directory), and allow
    /// entries that are already normalised relative paths.
    pub fn validate_path_maps(state_dir: &Path, path_maps: &[AgentPathMap]) -> Result<(), AgentError> {
        let mut projects = BTreeSet::new();
        for map in path_maps {
            if !projects.insert(map.project.as_str().to_owned()) {
                return Err(AgentError::Config(format!(
                    "project {} is mapped more than once",
                    map.project
                )));
            }
            validate_root(&map.project, "local root", Path::new(&map.local_root), state_dir, false)?;
            match &map.worktree_root {
                Some(root) => validate_root(&map.project, "worktree root", Path::new(root), state_dir, true)?,
                None => {
                    let default = Self::default_worktree_root(Path::new(&map.local_root));
                    validate_root(&map.project, "worktree root", &default, state_dir, true)?;
                }
            }
            for entry in &map.allow {
                validate_allow_entry(&map.project, entry)?;
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), AgentError> {
        if self.home.trim().is_empty() {
            return Err(AgentError::Config("home name must not be empty".to_owned()));
        }
        match &self.transport {
            TransportConfig::Ssh { .. } => {
                self.ssh_target()?;
            }
            TransportConfig::Wss { .. } => {
                crate::wss::WssTransport::from_transport_config(&self.transport)?;
            }
        }
        Self::validate_path_maps(&self.state_dir, &self.path_maps)?;
        Ok(())
    }
}

fn validate_root(
    project: &ProjectName,
    label: &str,
    root: &Path,
    state_dir: &Path,
    creatable_below_state_dir: bool,
) -> Result<(), AgentError> {
    let Some(text) = root.to_str() else {
        return Err(AgentError::Config(format!(
            "project {project} {label} must be a UTF-8 path"
        )));
    };
    if text.is_empty() || !root.is_absolute() {
        return Err(AgentError::Config(format!(
            "project {project} {label} must be an absolute path"
        )));
    }
    if text.contains('\0') {
        return Err(AgentError::Config(format!(
            "project {project} {label} must not contain a NUL byte"
        )));
    }
    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(AgentError::Config(format!(
            "project {project} {label} must not contain parent traversal"
        )));
    }
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(AgentError::Config(format!(
            "project {project} {label} is not a directory"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if creatable_below_state_dir && root.starts_with(state_dir) {
                fs::create_dir_all(root)?;
                Ok(())
            } else {
                Err(AgentError::Config(format!("project {project} {label} is missing")))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AgentError::Config(format!("project {project} {label} is not readable")))
        }
        Err(error) => Err(AgentError::Io(error)),
    }
}

fn validate_allow_entry(project: &ProjectName, entry: &str) -> Result<(), AgentError> {
    let invalid = || {
        AgentError::Config(format!(
            "project {project} allow entry must be a normalised relative path"
        ))
    };
    if entry.is_empty() || entry.len() > MAX_PATH_REF_LEN || entry.contains('\0') || entry.contains('\\') {
        return Err(invalid());
    }
    if entry.starts_with('/') {
        return Err(invalid());
    }
    for part in entry.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Creates `path` and every missing parent, then makes `path` mode 0700.
pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), AgentError> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Writes a private file (0600) below a 0700 directory.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    write_file(path, bytes, 0o600)
}

/// Writes a public file (0644).
pub(crate) fn write_public(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    write_file(path, bytes, 0o644)
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), AgentError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wss(url: &str, tls: WssTlsMode, fingerprint: Option<&str>) -> TransportConfig {
        TransportConfig::Wss {
            url: url.to_owned(),
            tls,
            fingerprint: fingerprint.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn a_wss_pin_without_a_fingerprint_is_refused() {
        let config = wss("wss://home.example/agent/v1", WssTlsMode::Pin, None);
        let error =
            crate::wss::WssTransport::from_transport_config(&config).expect_err("tls = pin requires a fingerprint");
        assert!(error.to_string().contains("requires a fingerprint"), "{error}");

        let bad = wss("wss://home.example/agent/v1", WssTlsMode::Pin, Some("sha256:ABC"));
        let error = crate::wss::WssTransport::from_transport_config(&bad).expect_err("a malformed pin is refused");
        assert!(error.to_string().contains("64 lowercase hex"), "{error}");
    }

    #[test]
    fn a_wss_system_transport_defaults_and_needs_no_fingerprint() {
        let parsed: TransportConfig = serde_json::from_str(r#"{"kind":"wss","url":"wss://home.example/agent/v1"}"#)
            .expect("the minimal transport table parses");
        assert!(matches!(
            parsed,
            TransportConfig::Wss {
                tls: WssTlsMode::System,
                fingerprint: None,
                ..
            }
        ));
        crate::wss::WssTransport::from_transport_config(&parsed).expect("system mode needs no pin");

        let pinned = wss(
            "wss://home.example/agent/v1",
            WssTlsMode::Pin,
            Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
        );
        crate::wss::WssTransport::from_transport_config(&pinned).expect("a full pin is accepted");
    }

    #[test]
    fn a_wss_url_must_be_the_one_route() {
        for bad in [
            "wss://home.example/",
            "wss://home.example/other",
            "ws://home.example/agent/v1",
        ] {
            let config = wss(bad, WssTlsMode::System, None);
            assert!(
                crate::wss::WssTransport::from_transport_config(&config).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn an_agent_config_with_a_bad_wss_transport_does_not_validate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = AgentConfig {
            home: "home-1".to_owned(),
            node: NodeName::parse("mac-studio").expect("node slug"),
            state_dir: dir.path().to_path_buf(),
            transport: wss("wss://home.example/agent/v1", WssTlsMode::Pin, None),
            path_maps: Vec::new(),
        };
        assert!(config.validate().is_err(), "a pin without a fingerprint never loads");
    }
}
