//! Home MCP tool execution for a paired local surface.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.4 and §8.3. A local `vowp mcp`
//! forwards `tools/call` as a typed [`vowp_protocol::agent::surface::ToolCall`] over the
//! Ed25519-authenticated node connection. The gateway verifies the surface token's active
//! scopes and the live lease, then executes the same home-side tool body the CLI backend
//! runs and returns the MCP content envelope. Neither an argument nor a result carries a
//! home secret value or a local absolute path.
//!
//! This module deliberately mirrors `crates/vowp-cli/src/mcp/backend.rs` without depending
//! on the CLI crate: `vowp-core` owns the store, fleet, capacity and session services the
//! tools need.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use vowp_protocol::{
    CapsuleTrigger, FleetStatus, InboxId, InboxItem, JobId, JobState, LeaseStatus, LedgerEntry, LedgerKind, NewJob,
    Project, ProjectName, Session, SessionId, SessionRole, SurfaceId, SurfaceScope, TargetMode, Timestamp, VowpConfig,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{Store, StoreError};

use crate::fleet;
use crate::ledger::Ledger;
use crate::requests::{self, Request};
use crate::session::service::SessionService;
use crate::surface::native::process_alive;
use crate::{AdapterRegistry, CapacityManager, CoreError, Deps};

/// Milliseconds a job request waits for a live supervisor before leaving it queued.
const JOB_WAIT: Duration = Duration::from_secs(5);

/// Poll interval while a job request waits for the supervisor.
const JOB_POLL: Duration = Duration::from_millis(50);

/// Tools a local surface always refuses.
///
/// They are control-machine or CLI-only operations: a stolen surface token must not enroll
/// a node, change offload policy, pair a surface, open a tunnel or read a home secret.
const FORBIDDEN_TOOLS: &[&str] = &[
    "tunnel_open",
    "tunnel_close",
    "secret_request",
    "node_enroll",
    "node_revoke",
    "surface_pair",
    "offload_policy_set",
];

/// Tools that write session or fleet state.
const WRITE_TOOLS: &[&str] = &[
    "inbox_answer",
    "job_add",
    "job_pause",
    "job_retry",
    "capacity_set",
    "session_handoff",
];

/// One typed tool failure returned to the MCP proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    /// Stable error code (`refused`, `invalid_input`, `not_found`, `conflict`, ...).
    pub code: String,
    /// Human-readable message; never a secret value or a local absolute path.
    pub message: String,
}

impl ToolFailure {
    /// Builds one typed failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Builds a `refused` failure.
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new("refused", message)
    }

    /// Builds an `invalid_input` failure.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }

    /// Builds a `not_found` failure.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    /// Builds a `conflict` failure.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message)
    }
}

impl From<CoreError> for ToolFailure {
    fn from(error: CoreError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<StoreError> for ToolFailure {
    fn from(error: StoreError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<vowp_protocol::ProtocolError> for ToolFailure {
    fn from(error: vowp_protocol::ProtocolError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<serde_json::Error> for ToolFailure {
    fn from(error: serde_json::Error) -> Self {
        Self::new("invalid_input", format!("json: {error}"))
    }
}

impl From<std::io::Error> for ToolFailure {
    fn from(error: std::io::Error) -> Self {
        Self::new("io", format!("io: {error}"))
    }
}

/// One verified local-surface tool call.
pub struct ToolContext<'a> {
    /// Home dependencies.
    pub deps: &'a Deps,
    /// Global store handle; global tables are readable from any bound handle.
    pub store: &'a Store,
    /// Paired surface that made the call.
    pub surface: &'a SurfaceId,
    /// Session the surface is paired for.
    pub session: &'a SessionId,
    /// Union of the surface's active token scopes.
    pub scopes: &'a [SurfaceScope],
    /// Home clock value for this call.
    pub now: Timestamp,
}

/// Executes one MCP tool for a verified local surface.
pub fn call(ctx: &ToolContext<'_>, tool: &str, arguments: &Value) -> Result<Value, ToolFailure> {
    if FORBIDDEN_TOOLS.contains(&tool) {
        return Err(ToolFailure::refused(format!(
            "{tool} is not available to a local surface"
        )));
    }
    let empty = Value::Object(Map::new());
    let arguments = if arguments.is_null() { &empty } else { arguments };
    let args = arguments
        .as_object()
        .ok_or_else(|| ToolFailure::invalid("arguments must be a JSON object"))?;
    let executor = Executor { ctx, args };

    match tool {
        "fleet_status" => {
            executor.require_fleet_read()?;
            executor.fleet_status(opt_str(args, "project")?)
        }
        "fleet_queue" => {
            executor.require_fleet_read()?;
            executor.fleet_queue(opt_str(args, "project")?, opt_str(args, "state")?)
        }
        "inbox_list" => {
            executor.require_fleet_read()?;
            executor.inbox_list(opt_str(args, "project")?, opt_bool(args, "all")?.unwrap_or(false))
        }
        "capacity_report" => {
            executor.require_fleet_read()?;
            executor.capacity_report()
        }
        "session_tail" => {
            let target = executor.target_session(opt_str(args, "session")?)?;
            executor.require_read_scope(&target)?;
            executor.session_tail(&target, args)
        }
        "session_resume" => {
            let target = executor.target_session(opt_str(args, "session")?)?;
            let takeover = opt_bool(args, "takeover")?.unwrap_or(false);
            if takeover {
                executor.require_write_scope()?;
                if target != *ctx.session {
                    return Err(ToolFailure::refused(
                        "session takeover must target the surface's own session",
                    ));
                }
            } else {
                executor.require_read_scope(&target)?;
            }
            executor.session_resume(&target, takeover, args)
        }
        tool if WRITE_TOOLS.contains(&tool) => {
            executor.require_write_scope()?;
            match tool {
                "inbox_answer" => executor.inbox_answer(args),
                "job_add" => executor.job_add(args),
                "job_pause" => executor.job_pause(args),
                "job_retry" => executor.job_retry(args),
                "capacity_set" => executor.capacity_set(args),
                "session_handoff" => executor.session_handoff(args),
                _ => Err(ToolFailure::invalid(format!("unknown tool: {tool}"))),
            }
        }
        other => Err(ToolFailure::invalid(format!("unknown tool: {other}"))),
    }
}

/// Borrowed arguments and the verified call facts.
struct Executor<'a, 'b> {
    ctx: &'a ToolContext<'b>,
    args: &'a Map<String, Value>,
}

impl Executor<'_, '_> {
    /// Returns whether the active token scopes contain `scope`.
    fn has(&self, scope: &str) -> bool {
        self.ctx.scopes.iter().any(|granted| granted.as_str() == scope)
    }

    /// Requires `fleet.read`.
    fn require_fleet_read(&self) -> Result<(), ToolFailure> {
        if self.has("fleet.read") {
            Ok(())
        } else {
            Err(ToolFailure::refused("surface token does not grant fleet.read"))
        }
    }

    /// Requires `session.write:<caller session>`.
    fn require_write_scope(&self) -> Result<(), ToolFailure> {
        let scope = format!("session.write:{}", self.ctx.session.as_str());
        if self.has(&scope) {
            Ok(())
        } else {
            Err(ToolFailure::refused(format!("surface token does not grant {scope}")))
        }
    }

    /// Requires read access to `target`.
    fn require_read_scope(&self, target: &SessionId) -> Result<(), ToolFailure> {
        if self.has(&format!("session.read:{}", target.as_str())) {
            return Ok(());
        }
        let target_row = self.ctx.store.require_session(target)?;
        if let Some(project) = target_row.project.as_ref()
            && self.has(&format!("project.read:{}", project.as_str()))
        {
            return Ok(());
        }
        let bound_row = self.ctx.store.require_session(self.ctx.session)?;
        if bound_row.project.is_none() && bound_row.role == Some(SessionRole::Orchestrator) && self.has("fleet.read") {
            return Ok(());
        }
        Err(ToolFailure::refused(format!(
            "surface token does not grant read access to session {target}"
        )))
    }

    /// Returns the actor recorded on every write.
    fn actor(&self) -> String {
        format!("session:{}/surface:{}", self.ctx.session.as_str(), self.ctx.surface.0)
    }

    /// Opens one registered project's database, creating it when absent.
    fn project_store(&self, project: &ProjectName) -> Result<Store, ToolFailure> {
        let paths = self
            .ctx
            .store
            .paths()
            .ok_or_else(|| ToolFailure::new("internal", "the home has no on-disk node paths"))?;
        Store::create_project_db(paths, project)?;
        Ok(Store::open_with_project(paths, project)?.with_clock(self.ctx.deps.clock.clone()))
    }

    /// Builds dependencies bound to one project's config.
    fn deps_for_project(&self, project: &ProjectName) -> Result<Deps, ToolFailure> {
        let entry = self
            .ctx
            .store
            .project(project)?
            .ok_or_else(|| ToolFailure::not_found(format!("project {project}")))?;
        let root = Path::new(&entry.path);
        let config = match std::fs::read_to_string(root.join("vowp.toml")) {
            Ok(text) => VowpConfig::from_toml(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A project-less session uses the node's own home config. A test fixture
                // may also have no `vowp.toml`; the home config remains a valid base.
                let mut config = (*self.ctx.deps.config).clone();
                config.project.name = project.clone();
                config.project.path = Some(entry.path.clone());
                config
            }
            Err(source) => {
                return Err(ToolFailure::new(
                    "io",
                    format!("{}: {source}", root.join("vowp.toml").display()),
                ));
            }
        };
        let adapters = AdapterRegistry::from_config(&config)?;
        let mut deps = self.ctx.deps.clone();
        deps.config = Arc::new(config);
        deps.adapters = Arc::new(adapters);
        Ok(deps)
    }

    /// Registers the reserved `_node` project when a project-less session needs it.
    fn ensure_node_project(&self) -> Result<ProjectName, ToolFailure> {
        let project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
        if self.ctx.store.project(&project)?.is_none() {
            let paths = self
                .ctx
                .store
                .paths()
                .ok_or_else(|| ToolFailure::new("internal", "the home has no on-disk node paths"))?;
            let now = self.ctx.now;
            self.ctx.store.upsert_project(&Project {
                name: project.clone(),
                path: paths.root().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: json!({}),
                created_at: now,
                updated_at: now,
            })?;
        }
        if let Some(paths) = self.ctx.store.paths() {
            Store::create_project_db(paths, &project)?;
        }
        Ok(project)
    }

    /// Returns the project that owns a session, using `_node` for project-less sessions.
    fn session_project(&self, row: &Session) -> Result<ProjectName, ToolFailure> {
        match &row.project {
            Some(project) => Ok(project.clone()),
            None => self.ensure_node_project(),
        }
    }

    /// Resolves the target project of a `job_add`, enforcing a bound surface's scope.
    fn project_for(&self) -> Result<ProjectName, ToolFailure> {
        let bound_row = self.ctx.store.require_session(self.ctx.session)?;
        if let Some(name) = opt_str(self.args, "project")? {
            let project = ProjectName::parse(name)?;
            if let Some(bound) = bound_row.project.as_ref()
                && *bound != project
                && !self.has(&format!("project.read:{}", project.as_str()))
            {
                return Err(ToolFailure::refused(format!(
                    "the surface is paired for project {bound}, not {project}"
                )));
            }
            return Ok(project);
        }
        if let Some(project) = bound_row.project {
            return Ok(project);
        }
        let projects = fleet::project_names(self.ctx.store)?;
        match projects.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolFailure::invalid(
                "no project is registered; register one with `vowp project add`",
            )),
            many => Err(ToolFailure::invalid(format!(
                "several projects are registered ({}); pass project",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Finds the project that owns `job`, preferring the session's project.
    fn project_for_job(&self, explicit: Option<&str>, job: &JobId) -> Result<ProjectName, ToolFailure> {
        if let Some(name) = explicit {
            return Ok(ProjectName::parse(name)?);
        }
        let bound_row = self.ctx.store.require_session(self.ctx.session)?;
        if let Some(project) = bound_row.project {
            let store = self.project_store(&project)?;
            if store.job(job)?.is_some() {
                return Ok(project);
            }
        }
        let mut found = Vec::new();
        for project in fleet::project_names(self.ctx.store)? {
            let store = self.project_store(&project)?;
            if store.job(job)?.is_some() {
                found.push(project);
            }
        }
        match found.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolFailure::not_found(format!("job {job}"))),
            many => Err(ToolFailure::invalid(format!(
                "job {job} exists in several projects ({}); pass project",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Resolves a session id or unique prefix.
    fn resolve_session(&self, input: &str) -> Result<SessionId, ToolFailure> {
        if let Ok(id) = SessionId::parse(input)
            && self.ctx.store.session(&id)?.is_some()
        {
            return Ok(id);
        }
        if input.len() < 6 {
            return Err(ToolFailure::invalid("session id prefix must be at least 6 characters"));
        }
        let prefix = input.to_ascii_uppercase();
        let matches: Vec<SessionId> = self
            .ctx
            .store
            .list_sessions(&vowp_store::SessionFilter::default())?
            .into_iter()
            .filter(|session| session.id.as_str().to_ascii_uppercase().starts_with(&prefix))
            .map(|session| session.id)
            .collect();
        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolFailure::not_found(format!("session {input}"))),
            many => Err(ToolFailure::invalid(format!(
                "ambiguous session prefix {input}: {}",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Returns the target session of a tool call, defaulting to the caller's session.
    fn target_session(&self, input: Option<&str>) -> Result<SessionId, ToolFailure> {
        match input {
            Some(text) => self.resolve_session(text),
            None => Ok(self.ctx.session.clone()),
        }
    }

    /// Returns a fresh snapshot from the file or the databases.
    fn snapshot_now(&self) -> Result<vowp_protocol::Snapshot, ToolFailure> {
        let now = self.ctx.now;
        let tick = self.ctx.deps.config.node.tick_secs;
        match fleet::read_snapshot(&self.ctx.deps.paths) {
            Ok(snapshot) if fleet::snapshot_is_fresh(&snapshot, now, tick) => Ok(snapshot),
            Ok(_) | Err(CoreError::NotFound(_)) => Ok(fleet::build_snapshot(self.ctx.deps, self.ctx.store, now)?),
            Err(error) => Err(error.into()),
        }
    }

    /// Runs `fleet_status`.
    fn fleet_status(&self, project: Option<&str>) -> Result<Value, ToolFailure> {
        let mut snapshot = self.snapshot_now()?;
        if let Some(project) = project {
            let project = ProjectName::parse(project)?;
            snapshot.projects.retain(|card| card.name == project);
        }
        let status = FleetStatus {
            snapshot,
            source: "local".to_owned(),
            fetched_at: self.ctx.now,
            stale: false,
        };
        Ok(serde_json::to_value(status)?)
    }

    /// Runs `fleet_queue`.
    fn fleet_queue(&self, project: Option<&str>, state: Option<&str>) -> Result<Value, ToolFailure> {
        let snapshot = self.snapshot_now()?;
        let mut rows = fleet::queue_rows(&snapshot);
        if let Some(project) = project {
            let project = ProjectName::parse(project)?;
            rows.retain(|row| row.project == project);
        }
        if let Some(state) = state {
            let state: JobState = serde_json::from_value(Value::String(state.to_owned()))
                .map_err(|_| ToolFailure::invalid(format!("unknown job state {state:?}")))?;
            rows.retain(|row| row.state == state);
        }
        Ok(serde_json::to_value(rows)?)
    }

    /// Runs `inbox_list`.
    fn inbox_list(&self, project: Option<&str>, all: bool) -> Result<Value, ToolFailure> {
        let mut items = fleet::collect_inbox(self.ctx.store, all)?;
        if let Some(project) = project {
            let project = ProjectName::parse(project)?;
            items.retain(|item| item.project.as_ref() == Some(&project));
        }
        Ok(serde_json::to_value(items)?)
    }

    /// Runs `capacity_report`.
    fn capacity_report(&self) -> Result<Value, ToolFailure> {
        let report = CapacityManager::new(self.ctx.deps.clone()).report(self.ctx.store, self.ctx.now)?;
        Ok(serde_json::to_value(report)?)
    }

    /// Runs `session_tail`.
    fn session_tail(&self, session: &SessionId, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let limit = opt_u32(args, "limit")?.unwrap_or(50).clamp(1, 1000) as usize;
        let reader = match SessionLog::open_read(&self.ctx.deps.paths, session) {
            Ok(reader) => reader,
            Err(StoreError::NotFound { .. }) => return Ok(json!([])),
            Err(error) => return Err(error.into()),
        };
        let events = match opt_u64(args, "after_seq")? {
            Some(after) => reader.read_from(after, Some(limit))?,
            None => reader.tail(limit)?,
        };
        Ok(serde_json::to_value(events)?)
    }

    /// Runs `inbox_answer`.
    fn inbox_answer(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let input = req_str(args, "id")?;
        let answer = req_str(args, "answer")?;
        let items = fleet::collect_inbox(self.ctx.store, true)?;
        let item = resolve_inbox(&items, input)?;
        if let Some(project) = item.project.clone() {
            let store = self.project_store(&project)?;
            let answered = store.answer_inbox(&item.id, answer, &self.actor())?;
            requests::push(
                &store,
                &Request::AnswerInbox {
                    id: answered.id.clone(),
                },
            )?;
            self.record(
                &store,
                answered.project.clone(),
                LedgerKind::Inbox,
                "inbox_answer",
                json!({"inbox": answered.id.as_str(), "job": answered.job_id.as_ref().map(JobId::as_str)}),
                format!("mcp answered inbox {}", answered.id),
            )?;
            return Ok(serde_json::to_value(answered)?);
        }
        let answered = self.ctx.store.answer_inbox(&item.id, answer, &self.actor())?;
        requests::push(
            self.ctx.store,
            &Request::AnswerInbox {
                id: answered.id.clone(),
            },
        )?;
        self.record(
            self.ctx.store,
            None,
            LedgerKind::Inbox,
            "inbox_answer",
            json!({"inbox": answered.id.as_str(), "job": answered.job_id.as_ref().map(JobId::as_str)}),
            format!("mcp answered inbox {}", answered.id),
        )?;
        Ok(serde_json::to_value(answered)?)
    }

    /// Runs `job_add`.
    fn job_add(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let new: NewJob = serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolFailure::invalid(format!("NewJob: {error}")))?;
        if new.title.trim().is_empty() {
            return Err(ToolFailure::invalid("job title must not be empty"));
        }
        let project = self.project_for()?;
        let store = self.project_store(&project)?;
        let job = store.insert_job(&project, &new, &self.actor())?;
        if let Err(error) = self.write_brief(&project, &job, new.brief_text.as_deref(), &new) {
            let _ = store.delete_job(&job.id);
            return Err(error);
        }
        self.record(
            &store,
            Some(project),
            LedgerKind::Note,
            "job_add",
            json!({"job": job.id.as_str(), "title": job.title, "kind": format!("{:?}", job.kind).to_lowercase()}),
            format!("mcp added job {}", job.id),
        )?;
        Ok(serde_json::to_value(job)?)
    }

    /// Writes the brief for a job created through a local surface.
    fn write_brief(
        &self,
        project: &ProjectName,
        job: &vowp_protocol::Job,
        body: Option<&str>,
        new: &NewJob,
    ) -> Result<(), ToolFailure> {
        let entry = self
            .ctx
            .store
            .project(project)?
            .ok_or_else(|| ToolFailure::not_found(format!("project {project}")))?;
        let path = Path::new(&entry.path).join(&job.brief);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = match body {
            Some(body) => body.to_owned(),
            None => {
                let owns = new
                    .owns
                    .iter()
                    .map(|glob| yaml_string(glob))
                    .collect::<Vec<_>>()
                    .join(", ");
                let needs = new
                    .needs
                    .iter()
                    .map(|need| yaml_string(need.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "---\njob: {}\nowns: [{}]\nneeds_done: [{}]\ndone_when: \"report has status: done\"\n---\n\n# {}\n",
                    job.id, owns, needs, job.title
                )
            }
        };
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Runs `job_pause`.
    fn job_pause(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let job = JobId::parse(req_str(args, "job")?)?;
        let project = self.project_for_job(opt_str(args, "project")?, &job)?;
        let store = self.project_store(&project)?;
        let current = store.require_job(&job)?;
        if current.state.is_terminal() {
            return Err(ToolFailure::conflict(format!(
                "job {job} is {} and cannot be parked",
                current.state.as_str()
            )));
        }
        let reason = opt_str(args, "reason")?
            .unwrap_or("parked from the MCP surface")
            .to_owned();
        let request = Request::ParkJob {
            job: job.clone(),
            reason: reason.clone(),
        };
        let updated = self.submit_job(&store, &project, &request, &job, &[JobState::Parked])?;
        self.record(
            &store,
            Some(project),
            LedgerKind::Note,
            "job_pause",
            json!({"job": job.as_str(), "reason": reason}),
            format!("mcp parked job {job}"),
        )?;
        Ok(serde_json::to_value(updated)?)
    }

    /// Runs `job_retry`.
    fn job_retry(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let job = JobId::parse(req_str(args, "job")?)?;
        let project = self.project_for_job(opt_str(args, "project")?, &job)?;
        let store = self.project_store(&project)?;
        let current = store.require_job(&job)?;
        let (request, expected) = match current.state {
            JobState::Failed => (Request::RetryJob { job: job.clone() }, vec![JobState::Ready]),
            JobState::Parked => (Request::ResolveJob { job: job.clone() }, vec![JobState::Ready]),
            JobState::Stalled => (Request::RetryJob { job: job.clone() }, vec![JobState::Running]),
            other => {
                return Err(ToolFailure::conflict(format!(
                    "job {job} is {} and cannot be retried",
                    other.as_str()
                )));
            }
        };
        let updated = self.submit_job(&store, &project, &request, &job, &expected)?;
        self.record(
            &store,
            Some(project),
            LedgerKind::Note,
            "job_retry",
            json!({"job": job.as_str(), "from": current.state.as_str()}),
            format!("mcp retried job {job}"),
        )?;
        Ok(serde_json::to_value(updated)?)
    }

    /// Applies or queues one job request and returns the current row.
    fn submit_job(
        &self,
        store: &Store,
        project: &ProjectName,
        request: &Request,
        job: &JobId,
        expected: &[JobState],
    ) -> Result<vowp_protocol::Job, ToolFailure> {
        let deps = self.deps_for_project(project)?;
        if supervisor_pid(&self.ctx.deps.paths).is_some() {
            requests::push(store, request)?;
            let deadline = Instant::now() + JOB_WAIT;
            loop {
                let row = store.require_job(job)?;
                if expected.contains(&row.state) || Instant::now() >= deadline {
                    return Ok(row);
                }
                thread::sleep(JOB_POLL);
            }
        }
        requests::apply_direct(&deps, store, request)?;
        Ok(store.require_job(job)?)
    }

    /// Runs `capacity_set`.
    fn capacity_set(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let target = opt_u32(args, "target")?;
        let mode_text = opt_str(args, "mode")?;
        let mode = match mode_text.map(str::to_ascii_lowercase).as_deref() {
            Some("auto") => TargetMode::Auto,
            Some("fixed") => TargetMode::Fixed,
            Some(other) => {
                return Err(ToolFailure::invalid(format!(
                    "unknown mode {other:?}; use auto or fixed"
                )));
            }
            None if target.is_some() => TargetMode::Fixed,
            None => TargetMode::Auto,
        };
        if mode == TargetMode::Fixed && target.is_none() {
            return Err(ToolFailure::invalid("fixed mode requires target"));
        }
        let is_override = opt_bool(args, "override")?.unwrap_or(false);
        let reason = opt_str(args, "reason")?
            .unwrap_or("target set by the MCP surface")
            .to_owned();
        let manager = CapacityManager::new(self.ctx.deps.clone());
        let written = manager.set_target(
            self.ctx.store,
            mode,
            target.unwrap_or(0),
            is_override,
            &self.actor(),
            &reason,
        )?;
        self.record(
            self.ctx.store,
            None,
            LedgerKind::Note,
            "capacity_set",
            json!({"target": written.target_workers, "mode": format!("{:?}", written.mode).to_lowercase(), "override": written.is_override}),
            format!("mcp set capacity target {}", written.target_workers),
        )?;
        Ok(serde_json::to_value(written)?)
    }

    /// Runs `session_handoff`.
    fn session_handoff(&self, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
        let session = self.target_session(opt_str(args, "session")?)?;
        if session != *self.ctx.session {
            return Err(ToolFailure::refused(
                "a local surface can only hand off its own session",
            ));
        }
        let row = self.ctx.store.require_session(&session)?;
        let project = self.session_project(&row)?;
        let deps = self.deps_for_project(&project)?;
        let capsule = SessionService::new(deps).write_capsule(&session, CapsuleTrigger::Manual)?;
        let store = self.project_store(&project)?;
        let reason = opt_str(args, "reason")?.unwrap_or("session handoff");
        self.record(
            &store,
            Some(project),
            LedgerKind::Note,
            "session_handoff",
            json!({"session": session.as_str(), "capsule": capsule.capsule_id.as_str(), "reason": reason}),
            format!("mcp wrote capsule {} for {session}", capsule.capsule_id),
        )?;
        Ok(json!({"capsule_id": capsule.capsule_id, "at_seq": capsule.at_seq}))
    }

    /// Runs `session_resume`; a takeover lease was already acquired by the gateway.
    fn session_resume(
        &self,
        session: &SessionId,
        takeover: bool,
        args: &Map<String, Value>,
    ) -> Result<Value, ToolFailure> {
        let limit = opt_u32(args, "limit")?.unwrap_or(20).clamp(1, 200) as usize;
        let row = self.ctx.store.require_session(session)?;
        let project = self.session_project(&row)?;

        let capsule = self.ctx.store.latest_capsule(session)?;
        let tail = match SessionLog::open_read(&self.ctx.deps.paths, session) {
            Ok(reader) => match capsule.as_ref() {
                Some(capsule) => {
                    let mut events = reader.read_from(capsule.at_seq, None)?;
                    let keep_from = events.len().saturating_sub(limit);
                    events.split_off(keep_from)
                }
                None => reader.tail(limit)?,
            },
            Err(StoreError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let lease = self.ctx.store.lease(session)?.map(|lease| LeaseStatus {
            held: true,
            holder: Some(lease.holder),
            kind: Some(lease.surface_kind),
            node: Some(lease.node),
            acquired_at: Some(lease.acquired_at),
            heartbeat_at: Some(lease.heartbeat_at),
            token: None,
        });

        if takeover {
            let store = self.project_store(&project)?;
            self.record(
                &store,
                Some(project),
                LedgerKind::Note,
                "session_resume",
                json!({
                    "session": session.as_str(),
                    "takeover": true,
                    "capsule": capsule.as_ref().map(|capsule| capsule.capsule_id.as_str()),
                    "tail": tail.len(),
                }),
                format!("mcp resumed {session}"),
            )?;
        }
        Ok(json!({
            "session": row,
            "capsule": capsule,
            "tail": tail,
            "lease": lease,
        }))
    }

    /// Appends the MCP actor row for one successful write.
    fn record(
        &self,
        store: &Store,
        project: Option<ProjectName>,
        kind: LedgerKind,
        tool: &str,
        detail: Value,
        summary: String,
    ) -> Result<(), ToolFailure> {
        let deps = match &project {
            Some(project) => self.deps_for_project(project)?,
            None => self.ctx.deps.clone(),
        };
        let entry = LedgerEntry {
            id: None,
            ts: self.ctx.now,
            kind,
            project,
            job_id: None,
            run_id: None,
            session_id: Some(self.ctx.session.clone()),
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary,
            detail: json!({
                "actor": self.actor(),
                "tool": tool,
                "surface": self.ctx.surface.0,
                "session": self.ctx.session.as_str(),
                "detail": detail,
            }),
        };
        Ledger::new(deps).record(store, entry)?;
        Ok(())
    }
}

/// Returns the stable wire name of an error kind.
fn kind_name(kind: vowp_protocol::ErrorKind) -> &'static str {
    match kind {
        vowp_protocol::ErrorKind::InvalidInput => "invalid_input",
        vowp_protocol::ErrorKind::NotFound => "not_found",
        vowp_protocol::ErrorKind::Conflict => "conflict",
        vowp_protocol::ErrorKind::Io => "io",
        vowp_protocol::ErrorKind::Db => "db",
        vowp_protocol::ErrorKind::Parse => "parse",
        vowp_protocol::ErrorKind::Config => "config",
        vowp_protocol::ErrorKind::AdapterUnavailable => "adapter_unavailable",
        vowp_protocol::ErrorKind::AdapterFailed => "adapter_failed",
        vowp_protocol::ErrorKind::Limit => "limit",
        vowp_protocol::ErrorKind::Auth => "auth",
        vowp_protocol::ErrorKind::Timeout => "timeout",
        vowp_protocol::ErrorKind::Guard => "guard",
        vowp_protocol::ErrorKind::Refused => "refused",
        vowp_protocol::ErrorKind::Remote => "remote",
        vowp_protocol::ErrorKind::Internal => "internal",
    }
}

/// Returns the live supervisor pid from `<home>/supervisor.lock`, if one holds the lock.
fn supervisor_pid(paths: &vowp_store::NodePaths) -> Option<u32> {
    let text = std::fs::read_to_string(paths.root().join("supervisor.lock")).ok()?;
    let pid: u32 = text.lines().next()?.trim().parse().ok()?;
    (pid > 1 && pid <= i32::MAX as u32 && process_alive(pid)).then_some(pid)
}

/// Resolves an inbox id or unique prefix from a merged list.
fn resolve_inbox(items: &[InboxItem], input: &str) -> Result<InboxItem, ToolFailure> {
    if let Ok(id) = InboxId::parse(input)
        && let Some(item) = items.iter().find(|item| item.id == id)
    {
        return Ok(item.clone());
    }
    if input.len() < 6 {
        return Err(ToolFailure::invalid("inbox id prefix must be at least 6 characters"));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<&InboxItem> = items
        .iter()
        .filter(|item| item.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [item] => Ok((*item).clone()),
        [] => Err(ToolFailure::not_found(format!("inbox item {input}"))),
        many => Err(ToolFailure::invalid(format!(
            "ambiguous inbox prefix {input}: {}",
            many.iter()
                .map(|item| item.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Quotes one YAML scalar with JSON's double-quoted form.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Reads a required string member.
fn req_str<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, ToolFailure> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolFailure::invalid(format!("missing string argument {name}")))
}

/// Reads an optional string member.
fn opt_str<'a>(args: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(ToolFailure::invalid(format!("{name} must be a string"))),
    }
}

/// Reads an optional boolean member.
fn opt_bool(args: &Map<String, Value>, name: &str) -> Result<Option<bool>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolFailure::invalid(format!("{name} must be a boolean"))),
    }
}

/// Reads an optional unsigned 64-bit member.
fn opt_u64(args: &Map<String, Value>, name: &str) -> Result<Option<u64>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolFailure::invalid(format!("{name} must be a non-negative integer"))),
        Some(_) => Err(ToolFailure::invalid(format!("{name} must be an integer"))),
    }
}

/// Reads an optional unsigned 32-bit member.
fn opt_u32(args: &Map<String, Value>, name: &str) -> Result<Option<u32>, ToolFailure> {
    match opt_u64(args, name)? {
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| ToolFailure::invalid(format!("{name} is too large"))),
        None => Ok(None),
    }
}
