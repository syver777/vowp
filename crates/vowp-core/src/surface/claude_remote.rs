//! The Claude Code Remote Control app host.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5. Remote Control needs a claude.ai
//! subscription login on the home; an API key is not enough. The login probe asks Claude
//! Code's own `auth status` command and reads its `loggedIn` flag; it never reads a login or
//! credential file. A fresh home where `claude auth status` reports
//! `{"loggedIn": false, "authMethod": "none"}` is refused, even though Claude has created a
//! `.claude.json` there. Without a login every operation that needs the app is refused with
//! the exact setup message from the spec, and the transcript reader uses the Claude Code
//! transcript importer.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_protocol::{JoinInstructions, OrchestratorApp, Timestamp};

use vowp_adapters::proc::{Signal, signal_group};

use crate::CoreError;
use crate::session::importer::{find_latest_transcript, scan_file};

use super::mirror::MirrorCursor;
use super::native::{
    AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, process_alive, remove_private, shell_quote_text,
    stop_pid, write_private_json,
};

/// The refusal text the setup report and the CLI show when no claude.ai login exists.
pub const NO_LOGIN: &str = "Claude Remote Control needs a claude.ai login on the home";

/// How long `start` waits before reporting a process that exited immediately.
const SPAWN_CHECK: Duration = Duration::from_millis(150);

/// The Claude Code Remote Control host.
#[derive(Debug, Clone)]
pub struct ClaudeRemoteHost {
    binary: PathBuf,
    config_dir: Option<PathBuf>,
    spawn_check: Duration,
}

impl Default for ClaudeRemoteHost {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            config_dir: None,
            spawn_check: SPAWN_CHECK,
        }
    }
}

impl ClaudeRemoteHost {
    /// Creates a host that runs `claude`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            config_dir: None,
            spawn_check: SPAWN_CHECK,
        }
    }

    /// Points the login and transcript probes at an explicit Claude config directory.
    ///
    /// Production leaves this unset and reads `CLAUDE_CONFIG_DIR` or `$HOME/.claude`.
    pub fn with_config_dir(mut self, config_dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(config_dir.into());
        self
    }

    /// Overrides the short wait that detects a process which exited immediately.
    pub fn with_spawn_check(mut self, check: Duration) -> Self {
        self.spawn_check = check;
        self
    }

    /// Returns true when Claude Code reports a claude.ai login.
    ///
    /// The probe runs `claude auth status` and reads only its `loggedIn`/`authMethod`
    /// metadata. It never opens the login or credential files, and a missing binary, a
    /// failed command or an unrecognised response counts as no login.
    pub fn login_present(&self) -> bool {
        let mut command = Command::new(&self.binary);
        command
            .args(["auth", "status"])
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(root) = &self.config_dir {
            command.env("CLAUDE_CONFIG_DIR", root);
        }
        let Ok(output) = command.output() else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return false;
        };
        let logged_in = value.get("loggedIn").and_then(serde_json::Value::as_bool) == Some(true);
        let method = value
            .get("authMethod")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        logged_in && !method.eq_ignore_ascii_case("none") && !method.eq_ignore_ascii_case("api_key")
    }

    /// Returns the Claude config root (`CLAUDE_CONFIG_DIR` or `$HOME/.claude`).
    fn config_root(&self) -> Option<PathBuf> {
        if let Some(root) = &self.config_dir {
            return Some(root.clone());
        }
        if let Some(root) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(root));
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
    }

    /// Returns the transcript entries after `cursor`.
    fn read_transcript(&self, handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
        let Some(root) = self.config_root() else {
            return Ok(TranscriptBatch {
                entries: Vec::new(),
                cursor: cursor.clone(),
            });
        };
        let Some(path) = find_latest_transcript(&root, &handle.workdir) else {
            return Ok(TranscriptBatch {
                entries: Vec::new(),
                cursor: cursor.clone(),
            });
        };
        let mut entries = scan_file(&path)?;
        let native_session = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_owned);
        for entry in &mut entries {
            if entry.native_session.is_none() {
                entry.native_session.clone_from(&native_session);
            }
        }
        let entries = match cursor.last_native_id.as_deref() {
            None => entries,
            Some(last) => match entries.iter().position(|entry| entry.native_id == last) {
                Some(position) => entries.into_iter().skip(position + 1).collect(),
                None => entries,
            },
        };
        let last = entries
            .last()
            .map(|entry| entry.native_id.clone())
            .or_else(|| cursor.last_native_id.clone());
        Ok(TranscriptBatch {
            entries,
            cursor: MirrorCursor {
                last_native_id: last,
                updated_at: Timestamp::now(),
            },
        })
    }

    /// Returns the host state path and the log file path for one working directory.
    fn paths(workdir: &Path) -> (PathBuf, PathBuf) {
        (workdir.join("host.json"), workdir.join("remote-control.log"))
    }
}

impl AppHost for ClaudeRemoteHost {
    fn kind(&self) -> OrchestratorApp {
        OrchestratorApp::ClaudeCode
    }

    fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError> {
        if !self.login_present() {
            return Err(CoreError::Refused(NO_LOGIN.to_owned()));
        }
        let (host_json, log_path) = Self::paths(&req.workdir);
        if let Some(state) = read_host_state(&host_json)? {
            if process_alive(state.pid) {
                return Err(CoreError::Invalid(format!(
                    "Claude Remote Control for {} is already running (pid {})",
                    req.workdir.display(),
                    state.pid
                )));
            }
            remove_private(&host_json);
        }
        std::fs::create_dir_all(&req.workdir).map_err(|source| CoreError::Io {
            path: req.workdir.display().to_string(),
            source,
        })?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)
            .map_err(|source| CoreError::Io {
                path: log_path.display().to_string(),
                source,
            })?;
        let _ = std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600));
        let stderr = log.try_clone().map_err(|source| CoreError::Io {
            path: log_path.display().to_string(),
            source,
        })?;
        let mut child = Command::new(&self.binary)
            .arg("remote-control")
            .arg("--name")
            .arg(&req.name)
            .current_dir(&req.workdir)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .process_group(0)
            .spawn()
            .map_err(|source| CoreError::Io {
                path: self.binary.display().to_string(),
                source,
            })?;
        let pid = child.id();
        // The host object is stateless; one thread reaps the child so a long-lived
        // supervisor does not collect zombies.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        std::thread::sleep(self.spawn_check);
        if !process_alive(pid) {
            return Err(CoreError::Command(format!(
                "Claude Remote Control exited immediately; see {}",
                log_path.display()
            )));
        }
        let started_at = Timestamp::now();
        if let Err(error) = write_private_json(
            &host_json,
            &HostState {
                kind: "claude-code".to_owned(),
                name: req.name.clone(),
                pid,
                started_at,
                workdir: req.workdir.display().to_string(),
                vowp_session: req.session.to_string(),
            },
        ) {
            stop_remote(pid);
            return Err(error);
        }
        Ok(AppHandle {
            kind: OrchestratorApp::ClaudeCode,
            pid,
            port: None,
            started_at,
            workdir: req.workdir.clone(),
        })
    }

    fn stop(&self, handle: &AppHandle) -> Result<(), CoreError> {
        let (host_json, _) = Self::paths(&handle.workdir);
        let state = read_host_state(&host_json)?;
        if let Some(state) = &state {
            stop_remote(state.pid);
        } else if process_alive(handle.pid) {
            stop_remote(handle.pid);
        }
        remove_private(&host_json);
        Ok(())
    }

    fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError> {
        let (host_json, _) = Self::paths(&handle.workdir);
        if !self.login_present() {
            return Ok(HostHealth {
                ok: false,
                detail: NO_LOGIN.to_owned(),
            });
        }
        match read_host_state(&host_json)? {
            Some(state) if process_alive(state.pid) => Ok(HostHealth {
                ok: true,
                detail: format!("Claude Remote Control is running (pid {})", state.pid),
            }),
            _ => Ok(HostHealth {
                ok: false,
                detail: "Claude Remote Control is not running".to_owned(),
            }),
        }
    }

    fn join_instructions(&self, handle: &AppHandle) -> Result<JoinInstructions, CoreError> {
        if !self.login_present() {
            return Err(CoreError::Refused(NO_LOGIN.to_owned()));
        }
        let (host_json, _) = Self::paths(&handle.workdir);
        let state =
            read_host_state(&host_json)?.ok_or_else(|| CoreError::NotFound("Claude Remote Control".to_owned()))?;
        let name = shell_quote_text(&state.name);
        Ok(JoinInstructions {
            desktop: format!(
                "Open https://claude.ai/code and choose the Remote Control session {name}. \
                 The session runs on the VOWP home; keep Claude Code running there."
            ),
            phone: format!("Open the Claude mobile app and choose the same Remote Control session {name}."),
            chat: format!("vowp chat {}", state.vowp_session),
        })
    }

    fn transcript_since(&self, handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
        self.read_transcript(handle, cursor)
    }
}

/// Stops the Remote Control process group, falling back to a single-pid stop.
fn stop_remote(pid: u32) {
    if !process_alive(pid) {
        return;
    }
    let _ = signal_group(pid, Signal::Term);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = signal_group(pid, Signal::Kill);
    stop_pid(pid);
}

/// What `host.json` records while Remote Control runs (mode 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostState {
    kind: String,
    name: String,
    pid: u32,
    started_at: Timestamp,
    workdir: String,
    vowp_session: String,
}

/// Reads `host.json`, returning `None` when the file is absent.
fn read_host_state(path: &Path) -> Result<Option<HostState>, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CoreError::Invalid(format!("host.json: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}
