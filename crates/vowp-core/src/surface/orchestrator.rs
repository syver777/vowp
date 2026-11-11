//! Orchestrator sessions and their app hosts.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.6. An orchestrator is one
//! project-less or project-bound session with role [`SessionRole::Orchestrator`], a
//! project-local directory under `~/.vowp/orchestrators/<name>/`, the app's own config
//! files (never the user's global app config) and, for the app hosts, one live surface
//! lease. The `vowp chat` app is a fallback with no background host: its join command
//! starts the chat surface directly.
//!
//! State lives in `orchestrator.json` (mode 0600). It is a superset of
//! [`OrchestratorCard`] so the fleet snapshot reads the same file: the card fields plus
//! `executor` and `created_at`. The app host keeps its own `host.json`; a running card is
//! only reported as running while that host's pid is alive.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_adapters::proc::{Signal, signal_group};
use vowp_protocol::{
    ExecutorRef, JoinInstructions, Lease, Level, NewSession, OrchestratorApp, OrchestratorCard, Project, ProjectName,
    SessionId, SessionRole, Surface, SurfaceId, SurfaceKind, Timestamp,
};
use vowp_store::{Store, StoreError};

use super::claude_remote::ClaudeRemoteHost;
use super::lease::{AcquireLease, LeaseGuard};
use super::mirror::{self, MirrorCursor};
use super::native::{AppHandle, AppHost, HostRequest, process_alive, write_private_json};
use super::opencode_host::OpencodeHost;
use crate::CoreError;
use crate::deps::Deps;
use crate::session::service::SessionService;

/// The instructions file written into every orchestrator directory.
///
/// The text lives in this crate so `cargo package -p vowp-core` carries it. The
/// repository copy in `framework/templates/ORCHESTRATOR.md` stays byte-identical and
/// a `vowp-cli` test checks that the two do not drift.
pub const ORCHESTRATOR_INSTRUCTIONS: &str = r#"# VOWP orchestrator

You are the owner's VOWP orchestrator. This directory belongs to one orchestrator session; the
session itself lives on the VOWP home and is shared by every surface the owner uses (the opencode
desktop app, a Claude app, or `vowp chat`). This file is your standing instruction, not a job brief.

## Start every turn with the real picture

1. Read `vowp fleet status --json` (or the home's `snapshot.json`), then `vowp fleet queue --json`
   and `vowp fleet inbox --json`. Read `vowp capacity report --json` when capacity, waiting work or
   a brake could matter.
2. Say what is running, waiting, blocked or failed, with the times from the snapshot. Do not invent
   status, counts or completion.
3. Say what you propose and why, then ask. The owner decides.
4. Apply only after an explicit yes, through the VOWP tools or the matching `vowp` command.

## How to talk to the owner

- Plain sentences, short paragraphs, times in the home's timezone. No wide tables and no colour
  requirement; the owner may be on a phone over SSH, or in the Claude mobile app for Remote Control.
- A phone join is app-specific: run the SSH `join.phone` command for opencode and `vowp chat`, and
  open the Claude mobile app for Claude Remote Control.
- Report a failure with its exact command or tool, the exit status and the useful tail of the output.
- When the owner asks to see something, use the `vowp tunnel open` command printed in `join.desktop`
  **on the control machine**. This home cannot open tunnels itself; `tunnel_open` and
  `vowp tunnel open` refuse here. Ask the owner to run the control-machine command and give you the
  local address, then close it on the control machine when the owner is done.
- When the owner changes app, device or model, continue the same session. The new surface gets the
  capsule and the last turns from `session_resume`; the old surface becomes read-only and says so.

## Hard limits

- Never open, query or edit a database, `orchestrator.json`, a capsule or a session log by hand.
  Use the commands and tools.
- Never read, print, log or commit a secret value, token, key or `.env` file. Refer to secrets by
  name only; use `vowp secret request` for a value the owner must enter.
- Never push, publish, deploy or change repository visibility. The supervisor merges; the owner
  publishes.
- A write tool that refuses because the session moved is final. Do not retry it with another token
  or another surface; tell the owner and continue read-only here.
- Never claim a tool, app join, tunnel or job is working without checking it.

## VOWP tools

When the app exposes the VOWP MCP server, prefer these tools over shell parsing: `fleet_status`,
`fleet_queue`, `inbox_list`, `inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_report`,
`capacity_set`, `session_tail`, `session_handoff`, `session_resume`, `secret_request`.
`tunnel_open` and `tunnel_close` exist only on a control-machine surface; do not promise them from
the home.

Some opencode 2.0.28 builds report the server `connected` without exposing its tools to the model.
The config and the server are still real; if you cannot see the tools, use the `vowp` CLI or the
`vowp chat` fallback and tell the owner which path you used.

## This directory

- `orchestrator.json` records the name, app, session id, bound project, executor and state.
- `opencode.json` or `.mcp.json` registers the `vowp mcp` server for this app, using the lease token
  in `lease.token`.
- `host.json` belongs to the running app host; `mirror.json` is the transcript cursor.

Do not edit these files. `vowp orchestrator stop` and `vowp orchestrator resume` manage them.
"#;

/// The persisted orchestrator state file.
const STATE_FILE: &str = "orchestrator.json";

/// The lease token file referenced by the app's MCP server config.
const LEASE_FILE: &str = "lease.token";

/// The app host's state file (written by the host implementation).
const HOST_FILE: &str = "host.json";

/// The transcript cursor written for a running orchestrator.
const MIRROR_FILE: &str = "mirror.json";

/// The pid file of the long-lived lease keeper.
pub const KEEPER_PID_FILE: &str = "keeper.pid";

/// How long the keeper waits between checks of its pid file and lease.
const KEEPER_TICK: Duration = Duration::from_secs(1);

/// A request to create an orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub struct NewOrchestrator {
    /// Orchestrator name.
    pub name: String,
    /// Project to bind, when any.
    pub project: Option<ProjectName>,
    /// App to run.
    pub app: OrchestratorApp,
    /// Executor to use, when chosen.
    pub executor: Option<ExecutorRef>,
    /// Replace an existing orchestrator with the same name.
    pub force: bool,
}

/// One orchestrator on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct OrchestratorInfo {
    /// Snapshot card.
    pub card: OrchestratorCard,
    /// Orchestrator directory.
    pub dir: PathBuf,
    /// Session id.
    pub session: SessionId,
    /// Running host, when any.
    pub host: Option<AppHandle>,
}

/// The orchestrator manager.
pub struct Orchestrator {
    deps: Deps,
    hosts: Vec<Arc<dyn AppHost>>,
}

impl Orchestrator {
    /// Creates a manager over `deps` with the real opencode and Claude Code hosts.
    pub fn new(deps: Deps) -> Self {
        Self::with_hosts(
            deps,
            vec![Arc::new(OpencodeHost::default()), Arc::new(ClaudeRemoteHost::default())],
        )
    }

    /// Creates a manager over `deps` with explicit app hosts.
    ///
    /// Tests pass fakes here; production uses [`Orchestrator::new`]. A missing host for
    /// an app that needs one is refused by [`Orchestrator::create`] and
    /// [`Orchestrator::resume`].
    pub fn with_hosts(deps: Deps, hosts: Vec<Arc<dyn AppHost>>) -> Self {
        Self { deps, hosts }
    }

    /// Creates and starts an orchestrator.
    pub fn create(&self, req: &NewOrchestrator) -> Result<OrchestratorInfo, CoreError> {
        validate_name(&req.name)?;
        let dir = self.dir(&req.name);
        if dir.exists() {
            if !req.force {
                return Err(CoreError::Store(StoreError::Conflict(format!(
                    "orchestrator {} already exists in {}; pass --force to replace it",
                    req.name,
                    dir.display()
                ))));
            }
            self.remove_existing(&req.name)?;
        }
        let global = Store::open_global(&self.deps.paths)?;
        self.prepare_project(&global, req.project.as_ref())?;
        drop(global);

        create_private_dir(&dir)?;
        let scoped = self.scoped_deps(req.project.as_ref())?;
        let service = SessionService::new(scoped.clone());
        let session = service.create_session(NewSession {
            project: req.project.clone(),
            job_id: None,
            role: SessionRole::Orchestrator,
            title: format!("orchestrator {}", req.name),
            cwd: dir.clone(),
            parent: None,
            executor: req.executor.as_ref().map(|executor| executor.executor.clone()),
        })?;
        let surface = self.surface(&req.name, req.app, &dir);
        let surface_id = surface.id.clone();
        let token_path = dir.join(LEASE_FILE);
        self.write_app_files(&dir, req.app, &session, &surface.id, &token_path)?;

        let store = scoped.open_store()?;
        let mut token = None;
        if req.app != OrchestratorApp::Chat {
            let guard = LeaseGuard::acquire(
                &scoped,
                &store,
                &AcquireLease {
                    session: session.clone(),
                    surface,
                    takeover: false,
                },
            )?;
            let held = guard.token().to_owned();
            if let Err(error) = write_private_text(&token_path, &held) {
                drop(guard);
                return Err(error);
            }
            // The CLI process exits after `orchestrator new`; the lease row and token
            // file are what the app's `vowp mcp` server checks. This is the same one-shot
            // pattern as the hidden `vowp session lease --acquire`.
            token = Some(held);
            std::mem::forget(guard);
        }

        let host = self.host_for(req.app)?;
        let config = self.config_path(&dir, req.app);
        let handle = if let Some(host) = host.as_ref() {
            match host.start(&HostRequest {
                name: req.name.clone(),
                session: session.clone(),
                port: None,
                workdir: dir.clone(),
                config,
            }) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    if let Some(token) = token.as_deref() {
                        let _ = store.release_lease(&session, token);
                        let _ = fs::remove_file(&token_path);
                    }
                    let join = fallback_join(req.app, &session);
                    let state = StateFile {
                        name: req.name.clone(),
                        app: req.app,
                        session_id: session.clone(),
                        project: req.project.clone(),
                        executor: req.executor.clone(),
                        state: "degraded".to_owned(),
                        created_at: self.deps.clock.now(),
                        join: Some(join),
                        surface_id: Some(surface_id.clone()),
                    };
                    let _ = write_state(&dir, &state);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let join = match (host.as_ref(), handle.as_ref()) {
            (Some(host), Some(handle)) => host
                .join_instructions(handle)
                .unwrap_or_else(|_| fallback_join(req.app, &session)),
            _ => fallback_join(req.app, &session),
        };
        let state = StateFile {
            name: req.name.clone(),
            app: req.app,
            session_id: session.clone(),
            project: req.project.clone(),
            executor: req.executor.clone(),
            state: "running".to_owned(),
            created_at: self.deps.clock.now(),
            join: Some(join),
            surface_id: Some(surface_id),
        };
        write_state(&dir, &state)?;
        if let (Some(host), Some(handle)) = (host.as_ref(), handle.as_ref()) {
            self.mirror(&dir, &state, host.as_ref(), handle);
        }
        Ok(info_from_state(state, dir, handle))
    }

    /// Lists every orchestrator.
    pub fn list(&self) -> Result<Vec<OrchestratorInfo>, CoreError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: root.display().to_string(),
                source,
            })?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Some(state) = read_state(&dir)? {
                found.push(self.info_from_dir(state, dir));
            }
        }
        found.sort_by(|left, right| left.card.name.cmp(&right.card.name));
        Ok(found)
    }

    /// Returns one orchestrator.
    pub fn info(&self, name: &str) -> Result<OrchestratorInfo, CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        Ok(self.info_from_dir(state, dir))
    }

    /// Stops one orchestrator's host and releases its lease. The session stays.
    pub fn stop(&self, name: &str) -> Result<(), CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let mut state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        self.stop_keeper(&dir);
        if let Some(handle) = host_handle(&dir, state.app) {
            if let Ok(host) = self.host_for(state.app)
                && let Some(host) = host.as_ref()
            {
                host.stop(&handle)?;
            }
        }
        self.release_lease(&dir, &state.name, &state.session_id)?;
        let _ = fs::remove_file(dir.join(LEASE_FILE));
        state.state = "stopped".to_owned();
        write_state(&dir, &state)
    }

    /// Starts one orchestrator's host again.
    pub fn resume(&self, name: &str) -> Result<OrchestratorInfo, CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let mut state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        // A resume replaces the lease token; the old keeper must stop before the new
        // token is written, or it would keep heartbeating a dead token and exit after
        // the CLI's `ensure_keeper` has already seen it alive.
        self.stop_keeper(&dir);
        let host = self.host_for(state.app)?;
        let config = self.config_path(&dir, state.app);
        let mut token = None;
        let mut surface_id = state.surface_id.clone();
        if state.app != OrchestratorApp::Chat {
            let scoped = self.scoped_deps(state.project.as_ref())?;
            let store = scoped.open_store()?;
            let mut surface = self.surface(&state.name, state.app, &dir);
            if let Some(id) = &state.surface_id {
                surface.id = id.clone();
            }
            let current_id = surface.id.clone();
            let guard = LeaseGuard::acquire(
                &scoped,
                &store,
                &AcquireLease {
                    session: state.session_id.clone(),
                    surface,
                    takeover: true,
                },
            )?;
            let held = guard.token().to_owned();
            let token_path = dir.join(LEASE_FILE);
            if let Err(error) = write_private_text(&token_path, &held)
                .and_then(|()| self.write_app_files(&dir, state.app, &state.session_id, &current_id, &token_path))
            {
                drop(guard);
                return Err(error);
            }
            token = Some(held);
            surface_id = Some(current_id);
            std::mem::forget(guard);
        }
        state.surface_id = surface_id;
        let current = host_handle(&dir, state.app);
        let handle = if let Some(current) = current {
            current
        } else {
            let Some(host) = host.as_ref() else {
                state.state = "running".to_owned();
                write_state(&dir, &state)?;
                return Ok(info_from_state(state, dir, None));
            };
            match host.start(&HostRequest {
                name: state.name.clone(),
                session: state.session_id.clone(),
                port: None,
                workdir: dir.clone(),
                config,
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    if let Some(token) = token.as_deref() {
                        self.release_lease_with(&state.session_id, token)?;
                        let _ = fs::remove_file(dir.join(LEASE_FILE));
                    }
                    state.state = "degraded".to_owned();
                    let _ = write_state(&dir, &state);
                    return Err(error);
                }
            }
        };
        let join = match host.as_ref() {
            Some(host) => host
                .join_instructions(&handle)
                .unwrap_or_else(|_| fallback_join(state.app, &state.session_id)),
            None => fallback_join(state.app, &state.session_id),
        };
        state.state = "running".to_owned();
        state.join = Some(join);
        write_state(&dir, &state)?;
        if let Some(host) = host.as_ref() {
            self.mirror(&dir, &state, host.as_ref(), &handle);
        }
        Ok(info_from_state(state, dir, Some(handle)))
    }

    /// Resumes every orchestrator whose persisted state is `running`.
    ///
    /// Called by the node supervisor at startup. A host that cannot start is marked
    /// `degraded` and does not fail the remaining orchestrators.
    pub fn resume_all(&self) -> Result<Vec<OrchestratorInfo>, CoreError> {
        let mut resumed = Vec::new();
        for state in self.states()? {
            if state.state != "running" {
                continue;
            }
            let dir = self.dir(&state.name);
            // A live host is already running. Reuse it only while it still owns its
            // lease: a supervisor restart must not invalidate every app config, and it
            // must not steal a session another surface took over.
            if let Some(handle) = host_handle(&dir, state.app) {
                if !self.can_reuse_live_lease(&dir, &state)? {
                    self.deps.log.log(
                        Level::Warn,
                        "orchestrator",
                        &format!(
                            "{} is running without its lease (another surface may hold it); leaving it",
                            state.name
                        ),
                    );
                    continue;
                }
                if let Ok(Some(host)) = self.host_for(state.app) {
                    self.mirror(&dir, &state, host.as_ref(), &handle);
                    resumed.push(info_from_state(state, dir, Some(handle)));
                    continue;
                }
            }
            match self.resume(&state.name) {
                Ok(info) => resumed.push(info),
                Err(error) => {
                    self.deps.log.log(
                        vowp_protocol::Level::Warn,
                        "orchestrator",
                        &format!("cannot resume {}: {error}", state.name),
                    );
                }
            }
        }
        Ok(resumed)
    }

    /// Returns whether a live host still owns the lease named by its token file.
    ///
    /// `resume_all` uses this to leave an already-running orchestrator and its keeper
    /// untouched; only a missing or replaced lease needs a takeover.
    fn can_reuse_live_lease(&self, dir: &Path, state: &StateFile) -> Result<bool, CoreError> {
        if state.app == OrchestratorApp::Chat {
            return Ok(true);
        }
        let token = match fs::read_to_string(dir.join(LEASE_FILE)) {
            Ok(token) => token.trim().to_owned(),
            Err(_) => return Ok(false),
        };
        if token.is_empty() {
            return Ok(false);
        }
        let scoped = self.scoped_deps(state.project.as_ref())?;
        let store = scoped.open_store()?;
        Ok(store
            .lease(&state.session_id)?
            .is_some_and(|lease| lease.token == token))
    }

    /// Keeps one orchestrator's lease alive until its pid file changes or it is revoked.
    ///
    /// `vowp orchestrator new|resume` starts this as a small detached process, because the
    /// CLI itself exits and a [`LeaseGuard`] heartbeat thread cannot outlive it. The keeper
    /// reuses the token file rather than taking over, so it never writes a switch or a
    /// takeover note. It refuses a row whose token differs (the owner moved the session),
    /// and it exits when `keeper.pid` is removed or names another process.
    pub fn keep_alive(&self, name: &str) -> Result<(), CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        if state.app == OrchestratorApp::Chat {
            return Err(CoreError::Invalid("the chat app has no host to keep alive".to_owned()));
        }
        let scoped = self.scoped_deps(state.project.as_ref())?;
        let store = scoped.open_store()?;
        let token_path = dir.join(LEASE_FILE);
        let token = fs::read_to_string(&token_path)
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                CoreError::Refused(format!("orchestrator {name} has no lease token file; the keeper stops"))
            })?;
        match store.lease(&state.session_id)? {
            Some(lease) if lease.token != token => {
                return Err(CoreError::Refused(format!(
                    "orchestrator {name} moved to another surface; the keeper stops"
                )));
            }
            Some(_) => {
                store.heartbeat_lease(&state.session_id, &token, self.deps.clock.now())?;
            }
            None => {
                let mut surface = self.surface(&state.name, state.app, &dir);
                if let Some(id) = &state.surface_id {
                    surface.id = id.clone();
                }
                store.upsert_surface(&surface)?;
                store.take_lease(&Lease {
                    session_id: state.session_id.clone(),
                    surface_id: Some(surface.id.clone()),
                    surface_kind: surface_kind(state.app),
                    node: self.deps.node.clone(),
                    holder: state.name.clone(),
                    token: token.clone(),
                    acquired_at: self.deps.clock.now(),
                    heartbeat_at: self.deps.clock.now(),
                })?;
            }
        }

        let pid = std::process::id().to_string();
        let pid_path = dir.join(KEEPER_PID_FILE);
        if let Err(error) = write_private_text(&pid_path, &pid) {
            let _ = store.release_lease(&state.session_id, &token);
            return Err(error);
        }
        loop {
            thread::sleep(KEEPER_TICK);
            let still_ours = fs::read_to_string(&pid_path)
                .ok()
                .is_some_and(|text| text.trim() == pid);
            if !still_ours || !store.heartbeat_lease(&state.session_id, &token, self.deps.clock.now())? {
                break;
            }
        }
        let _ = fs::remove_file(&pid_path);
        store.release_lease(&state.session_id, &token)?;
        Ok(())
    }

    /// Stops a running lease keeper, if any.
    fn stop_keeper(&self, dir: &Path) {
        let pid_path = dir.join(KEEPER_PID_FILE);
        let pid = match fs::read_to_string(&pid_path) {
            Ok(text) => text.trim().parse::<u32>().ok(),
            Err(_) => None,
        };
        if let Some(pid) = pid
            && process_alive(pid)
        {
            let _ = signal_group(pid, Signal::Term);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if !process_alive(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if process_alive(pid) {
                let _ = signal_group(pid, Signal::Kill);
            }
        }
        let _ = fs::remove_file(&pid_path);
    }

    /// Returns every persisted state, sorted by name.
    fn states(&self) -> Result<Vec<StateFile>, CoreError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: root.display().to_string(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(state) = read_state(&entry.path())? {
                states.push(state);
            }
        }
        states.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(states)
    }

    /// Returns the orchestrator root (`~/.vowp/orchestrators`).
    fn root(&self) -> PathBuf {
        self.deps.paths.root().join("orchestrators")
    }

    /// Returns one orchestrator's directory.
    fn dir(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    /// Builds an [`OrchestratorInfo`] from persisted state and a live host, if any.
    fn info_from_dir(&self, state: StateFile, dir: PathBuf) -> OrchestratorInfo {
        let host = host_handle(&dir, state.app);
        let mut card = state.card();
        if card.state == "running" && state.app != OrchestratorApp::Chat && host.is_none() {
            card.state = "degraded".to_owned();
        }
        OrchestratorInfo {
            card,
            dir,
            session: state.session_id,
            host,
        }
    }

    /// Returns the app host for one app, when the app has a host.
    fn host_for(&self, app: OrchestratorApp) -> Result<Option<Arc<dyn AppHost>>, CoreError> {
        if app == OrchestratorApp::Chat {
            return Ok(None);
        }
        self.hosts
            .iter()
            .find(|host| host.kind() == app)
            .cloned()
            .map(Some)
            .ok_or_else(|| CoreError::Refused(format!("no app host is configured for {}", app_name(app))))
    }

    /// Builds the surface row for the orchestrator's app.
    fn surface(&self, name: &str, app: OrchestratorApp, dir: &Path) -> Surface {
        let now = self.deps.clock.now();
        Surface {
            id: SurfaceId::new(),
            kind: surface_kind(app),
            node: self.deps.node.clone(),
            holder: name.to_owned(),
            app: Some(app_name(app).to_owned()),
            cwd: Some(dir.display().to_string()),
            metadata: serde_json::json!({"orchestrator": name}),
            created_at: now,
            last_seen_at: now,
        }
    }

    /// Writes the instructions file and the app's project-local MCP config.
    fn write_app_files(
        &self,
        dir: &Path,
        app: OrchestratorApp,
        session: &SessionId,
        surface: &SurfaceId,
        token_file: &Path,
    ) -> Result<(), CoreError> {
        write_private_text(&dir.join("AGENTS.md"), ORCHESTRATOR_INSTRUCTIONS)?;
        let token_file = token_file.display().to_string();
        let session = session.to_string();
        let surface = surface.to_string();
        match app {
            OrchestratorApp::Opencode => {
                let config = serde_json::json!({
                    "mcp": {
                        "vowp": {
                            "type": "local",
                            "command": [
                                "vowp",
                                "mcp",
                                "--session",
                                session,
                                "--surface",
                                surface,
                                "--token-file",
                                token_file,
                            ],
                            "enabled": true
                        }
                    }
                });
                write_private_json(&dir.join("opencode.json"), &config)?;
            }
            OrchestratorApp::ClaudeCode => {
                let config = serde_json::json!({
                    "mcpServers": {
                        "vowp": {
                            "command": "vowp",
                            "args": [
                                "mcp",
                                "--session",
                                session,
                                "--surface",
                                surface,
                                "--token-file",
                                token_file,
                            ]
                        }
                    }
                });
                write_private_json(&dir.join(".mcp.json"), &config)?;
            }
            OrchestratorApp::Chat => {}
        }
        Ok(())
    }

    /// Mirrors one running host's new transcript entries and writes `mirror.json`.
    ///
    /// A transcript read or cursor write failure is logged, not returned: `create` and
    /// `resume` already started the host, and the app may still be coming up. The cursor
    /// file is written even after a failed read so `mirror.json` exists for a running
    /// orchestrator and the next pass starts from the last known position.
    fn mirror(&self, dir: &Path, state: &StateFile, host: &dyn AppHost, handle: &AppHandle) {
        let path = dir.join(MIRROR_FILE);
        let mut cursor = match mirror::read_cursor(&path) {
            Ok(cursor) => cursor,
            Err(error) => {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!("cannot read {}: {error}", path.display()),
                );
                MirrorCursor {
                    last_native_id: None,
                    updated_at: self.deps.clock.now(),
                }
            }
        };
        let result = (|| -> Result<(), CoreError> {
            let scoped = self.scoped_deps(state.project.as_ref())?;
            let store = scoped.open_store()?;
            mirror::mirror_from_host(&scoped, &store, &state.session_id, host, handle, &mut cursor)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!("cannot mirror {}: {error}", state.name),
            );
        }
        if let Err(error) = mirror::write_cursor(&path, &cursor) {
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!("cannot write {}: {error}", path.display()),
            );
        }
    }

    /// Returns the app's project-local config path.
    fn config_path(&self, dir: &Path, app: OrchestratorApp) -> PathBuf {
        match app {
            OrchestratorApp::Opencode => dir.join("opencode.json"),
            OrchestratorApp::ClaudeCode => dir.join(".mcp.json"),
            OrchestratorApp::Chat => dir.join("AGENTS.md"),
        }
    }

    /// Ensures the registry row and project database the session needs exist.
    fn prepare_project(&self, global: &Store, project: Option<&ProjectName>) -> Result<(), CoreError> {
        match project {
            Some(project) => {
                if global.project(project)?.is_none() {
                    return Err(CoreError::NotFound(format!(
                        "project {project} is not registered; run `vowp project add <path>` first"
                    )));
                }
                Store::create_project_db(&self.deps.paths, project)?;
            }
            None => {
                if global.local_node()?.is_none() {
                    let now = self.deps.clock.now();
                    global.upsert_node(&vowp_protocol::Node {
                        name: self.deps.node.clone(),
                        ssh_target: None,
                        is_local: true,
                        capabilities: serde_json::json!({ "vowp": env!("CARGO_PKG_VERSION") }),
                        last_seen_at: Some(now),
                        created_at: now,
                    })?;
                }
                let node = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
                if global.project(&node)?.is_none() {
                    let now = self.deps.clock.now();
                    global.upsert_project(&Project {
                        name: node.clone(),
                        path: self.deps.paths.root().display().to_string(),
                        remote_url: None,
                        default_branch: "main".to_owned(),
                        config: serde_json::json!({}),
                        created_at: now,
                        updated_at: now,
                    })?;
                } else {
                    Store::create_project_db(&self.deps.paths, &node)?;
                }
            }
        }
        Ok(())
    }

    /// Returns deps bound to the orchestrator's project (`_node` when project-less).
    fn scoped_deps(&self, project: Option<&ProjectName>) -> Result<Deps, CoreError> {
        let target = match project {
            Some(project) => project.clone(),
            None => ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?,
        };
        let mut config = (*self.deps.config).clone();
        config.project.name = target;
        let mut scoped = self.deps.clone();
        scoped.config = Arc::new(config);
        Ok(scoped)
    }

    /// Releases this orchestrator's lease, never another surface's.
    ///
    /// The token file is the authority: a takeover replaces the lease row's token, so a
    /// stop after a takeover must leave the new holder's row alone. Only when the token
    /// file is gone does the holder name decide, which covers an old crashed directory.
    fn release_lease(&self, dir: &Path, holder: &str, session: &SessionId) -> Result<(), CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        let Some(lease) = store.lease(session)? else {
            return Ok(());
        };
        let token = fs::read_to_string(dir.join(LEASE_FILE)).ok();
        let owns_lease = match token.as_deref() {
            Some(token) => token.trim() == lease.token,
            None => lease.holder == holder,
        };
        if owns_lease {
            store.release_lease(session, &lease.token)?;
        }
        Ok(())
    }

    /// Releases a lease with an explicit token.
    fn release_lease_with(&self, session: &SessionId, token: &str) -> Result<(), CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        store.release_lease(session, token)?;
        Ok(())
    }

    /// Stops and removes an existing orchestrator directory (`--force`).
    fn remove_existing(&self, name: &str) -> Result<(), CoreError> {
        let dir = self.dir(name);
        if let Some(state) = read_state(&dir)? {
            self.stop_keeper(&dir);
            if let Some(handle) = host_handle(&dir, state.app)
                && let Ok(Some(host)) = self.host_for(state.app)
            {
                let _ = host.stop(&handle);
            }
            self.release_lease(&dir, &state.name, &state.session_id)?;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CoreError::Io {
                path: dir.display().to_string(),
                source,
            }),
        }
    }
}

/// The persisted `orchestrator.json` shape.
///
/// It carries every [`OrchestratorCard`] field, because the fleet snapshot reads this
/// same file, plus the state the manager needs to resume a host.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    name: String,
    app: OrchestratorApp,
    session_id: SessionId,
    #[serde(default)]
    project: Option<ProjectName>,
    #[serde(default)]
    executor: Option<ExecutorRef>,
    state: String,
    created_at: Timestamp,
    #[serde(default)]
    join: Option<JoinInstructions>,
    /// Surface id used by the app's MCP config; kept so resume can reuse it.
    #[serde(default)]
    surface_id: Option<SurfaceId>,
}

impl StateFile {
    /// Returns the public snapshot card.
    fn card(&self) -> OrchestratorCard {
        OrchestratorCard {
            name: self.name.clone(),
            session_id: self.session_id.clone(),
            project: self.project.clone(),
            app: self.app,
            state: self.state.clone(),
            join: self
                .join
                .clone()
                .unwrap_or_else(|| fallback_join(self.app, &self.session_id)),
        }
    }
}

/// The subset of `host.json` the manager needs to recognise a live host.
#[derive(Debug, Deserialize)]
struct HostFile {
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    started_at: Option<Timestamp>,
    #[serde(default)]
    workdir: Option<String>,
}

/// Builds the final info value.
fn info_from_state(state: StateFile, dir: PathBuf, host: Option<AppHandle>) -> OrchestratorInfo {
    OrchestratorInfo {
        card: state.card(),
        dir,
        session: state.session_id,
        host,
    }
}

/// Reads an orchestrator state file, if it exists.
fn read_state(dir: &Path) -> Result<Option<StateFile>, CoreError> {
    let path = dir.join(STATE_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CoreError::Invalid(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Writes an orchestrator state file with mode 0600.
fn write_state(dir: &Path, state: &StateFile) -> Result<(), CoreError> {
    write_private_json(&dir.join(STATE_FILE), state)
}

/// Returns the live app handle recorded by a host, when its pid is still alive.
fn host_handle(dir: &Path, app: OrchestratorApp) -> Option<AppHandle> {
    let path = dir.join(HOST_FILE);
    let bytes = fs::read(path).ok()?;
    let host: HostFile = serde_json::from_slice(&bytes).ok()?;
    if host.pid <= 1 || !process_alive(host.pid) {
        return None;
    }
    Some(AppHandle {
        kind: app,
        pid: host.pid,
        port: host.port,
        started_at: host.started_at.unwrap_or_else(Timestamp::now),
        workdir: host.workdir.map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf()),
    })
}

/// Writes one private text file (mode 0600), creating its parent.
fn write_private_text(path: &Path, text: &str) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    fs::write(path, text).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Creates the orchestrator directory with mode 0700.
fn create_private_dir(dir: &Path) -> Result<(), CoreError> {
    fs::create_dir_all(dir).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })
}

/// Validates a name used as one directory below `orchestrators/`.
fn validate_name(name: &str) -> Result<(), CoreError> {
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if safe {
        Ok(())
    } else {
        Err(CoreError::Invalid(format!(
            "invalid orchestrator name {name:?}; use letters, digits, '.', '_' or '-'"
        )))
    }
}

/// Maps an app to its surface kind.
fn surface_kind(app: OrchestratorApp) -> SurfaceKind {
    match app {
        OrchestratorApp::Opencode => SurfaceKind::Opencode,
        OrchestratorApp::ClaudeCode => SurfaceKind::ClaudeCode,
        OrchestratorApp::Chat => SurfaceKind::Chat,
    }
}

/// Returns the CLI name of an app.
pub(crate) fn app_name(app: OrchestratorApp) -> &'static str {
    match app {
        OrchestratorApp::Opencode => "opencode",
        OrchestratorApp::ClaudeCode => "claude-code",
        OrchestratorApp::Chat => "chat",
    }
}

/// Join instructions used when an app host is not available.
///
/// They carry only generic placeholders (`<home-ssh-target>`, `<opencode-session>`); the
/// control machine substitutes the owner's real target in `vowp tunnel` and setup.
fn fallback_join(app: OrchestratorApp, session: &SessionId) -> JoinInstructions {
    let chat = format!("vowp chat {session}");
    match app {
        OrchestratorApp::Opencode => JoinInstructions {
            desktop: "On the control machine run `vowp tunnel open <name> --remote-port <port>`, then \
                      print a one-time pair link on the home with `opencode pair` and open it in the \
                      opencode desktop app."
                .to_owned(),
            phone: "ssh -t <home-ssh-target> opencode --server http://127.0.0.1:<port> --session <opencode-session>"
                .to_owned(),
            chat,
        },
        OrchestratorApp::ClaudeCode => JoinInstructions {
            desktop: "Open https://claude.ai/code and choose this Remote Control session; it runs on the VOWP home."
                .to_owned(),
            phone: "Open the Claude mobile app and choose the same Remote Control session.".to_owned(),
            chat,
        },
        OrchestratorApp::Chat => JoinInstructions {
            desktop: chat.clone(),
            phone: format!("ssh -t <home-ssh-target> {chat}"),
            chat,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn names_cannot_escape_the_orchestrator_root() {
        for name in ["..", ".", "", "a/b", "a b", "a\\b"] {
            assert!(validate_name(name).is_err(), "{name:?} was accepted");
        }
        for name in ["vowp", "release-2", "team_a", "a.b"] {
            assert!(validate_name(name).is_ok(), "{name:?} was refused");
        }
    }
}
