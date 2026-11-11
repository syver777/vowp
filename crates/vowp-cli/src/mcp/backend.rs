//! Home and control-machine behaviour behind the MCP tools.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7 and §8.3. On the home every tool
//! reads or writes the database directly. A control machine routes fleet reads through the
//! home's JSON CLI (with the snapshot cache as the offline fallback), sends writes to a
//! one-shot `vowp mcp` on the home over SSH, and keeps tunnel processes local.

use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use vowp_core::fleet;
use vowp_core::ledger::Ledger;
use vowp_core::requests::{self, Request};
use vowp_core::session::service::SessionService;
use vowp_core::surface::{AcquireLease, LeaseGuard, check_lease_with, takeover};
use vowp_core::vault;
use vowp_core::{AdapterRegistry, CapacityManager, CoreError, Deps};
use vowp_protocol::{
    CapsuleTrigger, FleetStatus, InboxId, InboxItem, JobId, JobState, LeaseStatus, LedgerEntry, LedgerKind, NewJob,
    Project, ProjectName, SecretName, SecretRequest, Session, SessionId, Surface, SurfaceId, SurfaceKind, TargetMode,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{SessionFilter, Store, StoreError};
use vowp_tmux::quote::shell_quote;

use crate::Ctx;
use crate::node_ctx::{ControlHome, NodeRole};

use super::McpOptions;
use super::tools::ToolError;

/// Milliseconds a job request waits for a live supervisor before leaving it queued.
const JOB_WAIT: Duration = Duration::from_secs(5);

/// The tool implementation for one MCP server process.
pub struct Backend<'a> {
    ctx: &'a Ctx,
    opts: &'a McpOptions,
}

impl<'a> Backend<'a> {
    /// Creates a backend for `ctx` and the server options.
    pub fn new(ctx: &'a Ctx, opts: &'a McpOptions) -> Self {
        Self { ctx, opts }
    }

    /// Dispatches one tool and returns its JSON value.
    pub fn call(&self, name: &str, arguments: &Value) -> Result<Value, ToolError> {
        let empty = Value::Object(Map::new());
        let arguments = if arguments.is_null() { &empty } else { arguments };
        let args = arguments
            .as_object()
            .ok_or_else(|| ToolError::invalid("arguments must be a JSON object"))?;

        match name {
            "fleet_status" => self.fleet_status(opt_str(args, "project")?),
            "fleet_queue" => self.fleet_queue(opt_str(args, "project")?, opt_str(args, "state")?),
            "inbox_list" => self.inbox_list(opt_str(args, "project")?, opt_bool(args, "all")?.unwrap_or(false)),
            "capacity_report" => self.capacity_report(),
            "session_tail" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, false)
                } else {
                    self.session_tail(args)
                }
            }
            "inbox_answer" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.inbox_answer(args)
                }
            }
            "job_add" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.job_add(args)
                }
            }
            "job_pause" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.job_pause(args)
                }
            }
            "job_retry" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.job_retry(args)
                }
            }
            "capacity_set" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.capacity_set(args)
                }
            }
            "tunnel_open" => self.tunnel_open(args),
            "tunnel_close" => self.tunnel_close(args),
            "secret_request" => {
                if self.is_control() {
                    self.secret_request_control(args)
                } else {
                    self.secret_request(args)
                }
            }
            "session_handoff" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.session_handoff(args)
                }
            }
            "session_resume" => {
                if self.is_control() {
                    let takeover = opt_bool(args, "takeover")?.unwrap_or(false);
                    if takeover {
                        return Err(ToolError::refused(
                            "session takeover must run on the home; resume from an app host on the home or run `vowp session takeover` there",
                        ));
                    }
                    self.remote_tool_call(name, arguments, false)
                } else {
                    self.session_resume(args)
                }
            }
            "_mcp_audit" => {
                if self.is_control() {
                    self.remote_tool_call(name, arguments, true)
                } else {
                    self.lease_audit(args)
                }
            }
            other => Err(ToolError::new("invalid_input", format!("unknown tool: {other}"))),
        }
    }

    /// Returns whether this server runs on a control machine.
    fn is_control(&self) -> bool {
        self.ctx.role() == NodeRole::Control
    }

    /// Returns the actor string recorded on every write.
    fn actor(&self) -> String {
        format!("session:{}/surface:{}", self.opts.session, self.opts.surface)
    }

    /// Opens the unbound global store.
    fn global(&self) -> Result<Store, ToolError> {
        Ok(self.ctx.fleet_store()?)
    }

    /// Opens one registered project's database, creating it when absent.
    fn project_store(&self, project: &ProjectName) -> Result<Store, ToolError> {
        Store::create_project_db(self.ctx.paths(), project)?;
        Ok(Store::open_with_project(self.ctx.paths(), project)?)
    }

    /// Builds dependencies bound to one project's config.
    fn deps_for_project(&self, project: &ProjectName) -> Result<Deps, ToolError> {
        let global = self.global()?;
        let entry = global
            .project(project)?
            .ok_or_else(|| ToolError::new("not_found", format!("project {project}")))?;
        let root = std::path::Path::new(&entry.path);
        let config = match crate::ctx::load_project_config(&root.join("vowp.toml"), self.ctx.paths()) {
            Ok(config) => config,
            Err(_) if project.as_str() == vowp_store::RESERVED_NODE_PROJECT => {
                // A project-less session uses the node's own home config, which may have
                // no `vowp.toml` yet on a freshly migrated home.
                let mut config = (*self.ctx.config()).clone();
                config.project.name = project.clone();
                config.project.path = Some(entry.path.clone());
                config
            }
            Err(error) => return Err(error.into()),
        };
        let adapters = AdapterRegistry::from_config(&config)?;
        let mut deps = self.ctx.deps().clone();
        deps.config = Arc::new(config);
        deps.adapters = Arc::new(adapters);
        Ok(deps)
    }

    /// Registers the reserved `_node` project when a project-less session needs it.
    fn ensure_node_project(&self) -> Result<ProjectName, ToolError> {
        let project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
        let global = self.global()?;
        if global.project(&project)?.is_none() {
            let now = self.ctx.clock().now();
            global.upsert_project(&Project {
                name: project.clone(),
                path: self.ctx.paths().root().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: json!({}),
                created_at: now,
                updated_at: now,
            })?;
        }
        Ok(project)
    }

    /// Returns the project that owns a session, using `_node` for project-less sessions.
    fn session_project(&self, row: &Session) -> Result<ProjectName, ToolError> {
        match &row.project {
            Some(project) => Ok(project.clone()),
            None => self.ensure_node_project(),
        }
    }

    /// Returns the project a job operation should use.
    fn project_for(&self, explicit: Option<&str>, session: &SessionId) -> Result<ProjectName, ToolError> {
        if let Some(name) = explicit {
            return Ok(ProjectName::parse(name)?);
        }
        let global = self.global()?;
        if let Some(row) = global.session(session)?
            && let Some(project) = row.project
        {
            return Ok(project);
        }
        let projects = fleet::project_names(&global)?;
        match projects.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolError::invalid(
                "no project is registered; register one with `vowp project add`",
            )),
            many => Err(ToolError::invalid(format!(
                "several projects are registered ({}); pass project",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Finds the project that owns `job`, preferring the session's project.
    fn project_for_job(&self, explicit: Option<&str>, job: &JobId) -> Result<ProjectName, ToolError> {
        if let Some(name) = explicit {
            return Ok(ProjectName::parse(name)?);
        }
        let global = self.global()?;
        if let Some(row) = global.session(&self.opts.session)?
            && let Some(project) = row.project
        {
            let store = self.project_store(&project)?;
            if store.job(job)?.is_some() {
                return Ok(project);
            }
        }
        let mut found = Vec::new();
        for project in fleet::project_names(&global)? {
            let store = self.project_store(&project)?;
            if store.job(job)?.is_some() {
                found.push(project);
            }
        }
        match found.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolError::new("not_found", format!("job {job}"))),
            many => Err(ToolError::invalid(format!(
                "job {job} exists in several projects ({}); pass project",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Resolves a session id or unique prefix.
    fn resolve_session(&self, input: &str) -> Result<SessionId, ToolError> {
        let store = self.global()?;
        if let Ok(id) = SessionId::parse(input)
            && store.session(&id)?.is_some()
        {
            return Ok(id);
        }
        if input.len() < 6 {
            return Err(ToolError::invalid("session id prefix must be at least 6 characters"));
        }
        let prefix = input.to_ascii_uppercase();
        let matches: Vec<SessionId> = store
            .list_sessions(&SessionFilter::default())?
            .into_iter()
            .filter(|session| session.id.as_str().to_ascii_uppercase().starts_with(&prefix))
            .map(|session| session.id)
            .collect();
        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(ToolError::new("not_found", format!("session {input}"))),
            many => Err(ToolError::invalid(format!(
                "ambiguous session prefix {input}: {}",
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Returns the target session of a tool call, defaulting to the server's session.
    fn target_session(&self, input: Option<&str>) -> Result<SessionId, ToolError> {
        match input {
            Some(text) => self.resolve_session(text),
            None => Ok(self.opts.session.clone()),
        }
    }

    /// Reads the live lease and refuses a write from a taken-over surface.
    ///
    /// The token file is re-read on every write; a takeover replaces the row and this
    /// check then fails with the exact §7.4 read-only notice.
    fn lease_gate(&self, session: &SessionId) -> Result<(), ToolError> {
        let text = std::fs::read_to_string(&self.opts.token_file).map_err(|error| {
            ToolError::new(
                "not_found",
                format!("lease token file {}: {error}", self.opts.token_file.display()),
            )
        })?;
        let token = text.trim_end_matches(['\r', '\n']).to_owned();
        if token.is_empty() {
            return Err(ToolError::invalid("lease token file is empty"));
        }
        self.check_lease_token(session, &token)
    }

    /// Checks the lease when the token file exists, for read-only resume.
    fn lease_gate_optional(&self, session: &SessionId) -> Result<(), ToolError> {
        let text = match std::fs::read_to_string(&self.opts.token_file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(ToolError::new(
                    "not_found",
                    format!("lease token file {}: {error}", self.opts.token_file.display()),
                ));
            }
        };
        let token = text.trim_end_matches(['\r', '\n']).to_owned();
        if token.is_empty() {
            return Ok(());
        }
        self.check_lease_token(session, &token)
    }

    /// Compares one token with the live lease row.
    fn check_lease_token(&self, session: &SessionId, token: &str) -> Result<(), ToolError> {
        let store = self.global()?;
        match check_lease_with(&store, session, Some(token)) {
            Ok(()) => Ok(()),
            Err(CoreError::Refused(_)) => {
                let notice = store
                    .lease(session)?
                    .map(|lease| takeover::notice(&lease.holder, lease.acquired_at, self.ctx.tz_offset_minutes()))
                    .unwrap_or_else(|| "this surface is read-only".to_owned());
                Err(ToolError::refused(notice))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Checks the lease on the home and records one control-machine write actor.
    fn audit_control_write(&self, tool: &str, detail: Value) -> Result<(), ToolError> {
        let arguments = json!({
            "tool": tool,
            "detail": detail,
            "session": self.opts.session.as_str(),
        });
        self.remote_tool_call("_mcp_audit", &arguments, true)?;
        Ok(())
    }

    /// Home half of [`Self::audit_control_write`].
    fn lease_audit(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let tool = req_str(args, "tool")?;
        let detail = args.get("detail").cloned().unwrap_or_else(|| json!({}));
        let session = self.target_session(opt_str(args, "session")?)?;
        self.lease_gate(&session)?;
        let global = self.global()?;
        let row = global.require_session(&session)?;
        let store = match &row.project {
            Some(project) => self.project_store(project)?,
            None => global,
        };
        self.record(
            &store,
            row.project.clone(),
            LedgerKind::Note,
            tool,
            detail,
            format!("mcp {tool}"),
        )?;
        Ok(json!({"ok": true}))
    }

    /// Appends the MCP actor row for a successful write.
    fn record(
        &self,
        store: &Store,
        project: Option<ProjectName>,
        kind: LedgerKind,
        tool: &str,
        detail: Value,
        summary: String,
    ) -> Result<(), ToolError> {
        let deps = match &project {
            Some(project) => self.deps_for_project(project)?,
            None => self.ctx.deps().clone(),
        };
        let entry = LedgerEntry {
            id: None,
            ts: self.ctx.clock().now(),
            kind,
            project,
            job_id: None,
            run_id: None,
            session_id: Some(self.opts.session.clone()),
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary,
            detail: json!({
                "actor": self.actor(),
                "tool": tool,
                "surface": self.opts.surface.0,
                "session": self.opts.session.as_str(),
                "detail": detail,
            }),
        };
        Ledger::new(deps).record(store, entry)?;
        Ok(())
    }

    /// Builds the snapshot from the file or from the databases.
    fn snapshot_now(&self) -> Result<vowp_protocol::Snapshot, ToolError> {
        let store = self.global()?;
        let now = self.ctx.clock().now();
        let tick = self.ctx.config().node.tick_secs;
        match fleet::read_snapshot(self.ctx.paths()) {
            Ok(snapshot) if fleet::snapshot_is_fresh(&snapshot, now, tick) => Ok(snapshot),
            Ok(_) | Err(CoreError::NotFound(_)) => Ok(fleet::build_snapshot(self.ctx.deps(), &store, now)?),
            Err(error) => Err(error.into()),
        }
    }

    /// Runs `fleet_status`.
    fn fleet_status(&self, project: Option<&str>) -> Result<Value, ToolError> {
        if self.is_control() {
            let value = match self.remote_json(&["fleet", "status"]) {
                Ok(value) => value,
                Err(error) => match self.cached_status()? {
                    Some(cached) => cached,
                    None => return Err(error),
                },
            };
            let mut status: FleetStatus = serde_json::from_value(value)
                .map_err(|error| ToolError::new("remote", format!("fleet status: {error}")))?;
            status.snapshot = filter_snapshot(status.snapshot, project)?;
            return Ok(serde_json::to_value(status)?);
        }
        let snapshot = filter_snapshot(self.snapshot_now()?, project)?;
        let status = FleetStatus {
            snapshot,
            source: "local".to_owned(),
            fetched_at: self.ctx.clock().now(),
            stale: false,
        };
        Ok(serde_json::to_value(status)?)
    }

    /// Reads the control snapshot cache as a `FleetStatus`.
    fn cached_status(&self) -> Result<Option<Value>, ToolError> {
        let Some(control) = self.ctx.control() else {
            return Ok(None);
        };
        let Some(cached) = crate::remote::cached_snapshot(&control.root)? else {
            return Ok(None);
        };
        let stale = crate::remote::cached_is_stale(&cached);
        let status = FleetStatus {
            snapshot: cached.snapshot,
            source: "cache".to_owned(),
            fetched_at: cached.fetched_at,
            stale,
        };
        Ok(Some(serde_json::to_value(status)?))
    }

    /// Runs `fleet_queue`.
    fn fleet_queue(&self, project: Option<&str>, state: Option<&str>) -> Result<Value, ToolError> {
        if self.is_control() {
            let mut args = vec!["fleet".to_owned(), "queue".to_owned()];
            if let Some(project) = project {
                args.extend(["--project".to_owned(), project.to_owned()]);
            }
            if let Some(state) = state {
                args.extend(["--state".to_owned(), state.to_owned()]);
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            return self.remote_json(&refs);
        }
        let snapshot = self.snapshot_now()?;
        let mut rows = fleet::queue_rows(&snapshot);
        if let Some(project) = project {
            let project = ProjectName::parse(project)?;
            rows.retain(|row| row.project == project);
        }
        if let Some(state) = state {
            let state: JobState = serde_json::from_value(Value::String(state.to_owned()))
                .map_err(|_| ToolError::invalid(format!("unknown job state {state:?}")))?;
            rows.retain(|row| row.state == state);
        }
        Ok(serde_json::to_value(rows)?)
    }

    /// Runs `inbox_list`.
    fn inbox_list(&self, project: Option<&str>, all: bool) -> Result<Value, ToolError> {
        if self.is_control() {
            let mut args = vec!["fleet".to_owned(), "inbox".to_owned()];
            if let Some(project) = project {
                args.extend(["--project".to_owned(), project.to_owned()]);
            }
            if all {
                args.push("--all".to_owned());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            return self.remote_json(&refs);
        }
        let store = self.global()?;
        let mut items = fleet::collect_inbox(&store, all)?;
        if let Some(project) = project {
            let project = ProjectName::parse(project)?;
            items.retain(|item| item.project.as_ref() == Some(&project));
        }
        Ok(serde_json::to_value(items)?)
    }

    /// Runs `capacity_report`.
    fn capacity_report(&self) -> Result<Value, ToolError> {
        if self.is_control() {
            return self.remote_json(&["capacity", "report"]);
        }
        let store = self.global()?;
        let report = CapacityManager::new(self.ctx.deps().clone()).report(&store, self.ctx.clock().now())?;
        Ok(serde_json::to_value(report)?)
    }

    /// Runs `session_tail` on the home.
    fn session_tail(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.target_session(opt_str(args, "session")?)?;
        let limit = opt_u32(args, "limit")?.unwrap_or(50).clamp(1, 1000) as usize;
        let reader = match SessionLog::open_read(self.ctx.paths(), &session) {
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

    /// Runs `inbox_answer` on the home.
    fn inbox_answer(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let input = req_str(args, "id")?;
        let answer = req_str(args, "answer")?;
        let global = self.global()?;
        let items = fleet::collect_inbox(&global, true)?;
        let item = resolve_inbox(&items, input)?;
        let store = match &item.project {
            Some(project) => self.project_store(project)?,
            None => global,
        };
        let answered = store.answer_inbox(&item.id, answer, &self.actor())?;
        if answered.job_id.is_some() {
            requests::push(
                &store,
                &Request::AnswerInbox {
                    id: answered.id.clone(),
                },
            )?;
        }
        self.record(
            &store,
            answered.project.clone(),
            LedgerKind::Inbox,
            "inbox_answer",
            json!({"inbox": answered.id.as_str(), "job": answered.job_id.as_ref().map(JobId::as_str)}),
            format!("mcp answered inbox {}", answered.id),
        )?;
        Ok(serde_json::to_value(answered)?)
    }

    /// Runs `job_add` on the home.
    fn job_add(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let new: NewJob = serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::invalid(format!("NewJob: {error}")))?;
        if new.title.trim().is_empty() {
            return Err(ToolError::invalid("job title must not be empty"));
        }
        let project = self.project_for(opt_str(args, "project")?, &self.opts.session)?;
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

    /// Writes the brief for a job created through MCP.
    fn write_brief(
        &self,
        project: &ProjectName,
        job: &vowp_protocol::Job,
        body: Option<&str>,
        new: &NewJob,
    ) -> Result<(), ToolError> {
        let global = self.global()?;
        let entry = global
            .project(project)?
            .ok_or_else(|| ToolError::new("not_found", format!("project {project}")))?;
        let path = std::path::Path::new(&entry.path).join(&job.brief);
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

    /// Runs `job_pause` on the home.
    fn job_pause(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let job = JobId::parse(req_str(args, "job")?)?;
        let project = self.project_for_job(opt_str(args, "project")?, &job)?;
        let store = self.project_store(&project)?;
        let current = store.require_job(&job)?;
        if current.state.is_terminal() {
            return Err(ToolError::new(
                "conflict",
                format!("job {job} is {} and cannot be parked", current.state.as_str()),
            ));
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

    /// Runs `job_retry` on the home.
    fn job_retry(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let job = JobId::parse(req_str(args, "job")?)?;
        let project = self.project_for_job(opt_str(args, "project")?, &job)?;
        let store = self.project_store(&project)?;
        let current = store.require_job(&job)?;
        let (request, expected) = match current.state {
            JobState::Failed => (Request::RetryJob { job: job.clone() }, vec![JobState::Ready]),
            JobState::Parked => (Request::ResolveJob { job: job.clone() }, vec![JobState::Ready]),
            JobState::Stalled => (Request::RetryJob { job: job.clone() }, vec![JobState::Running]),
            other => {
                return Err(ToolError::new(
                    "conflict",
                    format!("job {job} is {} and cannot be retried", other.as_str()),
                ));
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
    ) -> Result<vowp_protocol::Job, ToolError> {
        let deps = self.deps_for_project(project)?;
        if crate::ctx::supervisor_pid(self.ctx.paths()).is_some() {
            requests::push(store, request)?;
            let deadline = std::time::Instant::now() + JOB_WAIT;
            loop {
                let row = store.require_job(job)?;
                if expected.contains(&row.state) || std::time::Instant::now() >= deadline {
                    return Ok(row);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        requests::apply_direct(&deps, store, request)?;
        Ok(store.require_job(job)?)
    }

    /// Runs `capacity_set` on the home.
    fn capacity_set(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let target = opt_u32(args, "target")?;
        let mode_text = opt_str(args, "mode")?;
        let mode = match mode_text.map(str::to_ascii_lowercase).as_deref() {
            Some("auto") => TargetMode::Auto,
            Some("fixed") => TargetMode::Fixed,
            Some(other) => {
                return Err(ToolError::invalid(format!("unknown mode {other:?}; use auto or fixed")));
            }
            None if target.is_some() => TargetMode::Fixed,
            None => TargetMode::Auto,
        };
        if mode == TargetMode::Fixed && target.is_none() {
            return Err(ToolError::invalid("fixed mode requires target"));
        }
        let is_override = opt_bool(args, "override")?.unwrap_or(false);
        let reason = opt_str(args, "reason")?
            .unwrap_or("target set by the MCP surface")
            .to_owned();
        let store = self.global()?;
        let manager = CapacityManager::new(self.ctx.deps().clone());
        let written = manager.set_target(&store, mode, target.unwrap_or(0), is_override, &self.actor(), &reason)?;
        self.record(
            &store,
            None,
            LedgerKind::Note,
            "capacity_set",
            json!({"target": written.target_workers, "mode": format!("{:?}", written.mode).to_lowercase(), "override": written.is_override}),
            format!("mcp set capacity target {}", written.target_workers),
        )?;
        Ok(serde_json::to_value(written)?)
    }

    /// Opens one tunnel through the local CLI when this is the control machine.
    fn tunnel_open(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        if !self.is_control() {
            return Err(ToolError::refused("tunnels are opened by the control machine"));
        }
        let name = req_str(args, "name")?;
        let remote_port =
            opt_u32(args, "remote_port")?.ok_or_else(|| ToolError::invalid("tunnel_open requires remote_port"))?;
        self.audit_control_write("tunnel_open", json!({"name": name, "remote_port": remote_port}))?;
        let mut command = vec![
            "tunnel".to_owned(),
            "open".to_owned(),
            name.to_owned(),
            "--remote-port".to_owned(),
            remote_port.to_string(),
        ];
        if let Some(local) = opt_u32(args, "local_port")? {
            command.extend(["--local-port".to_owned(), local.to_string()]);
        }
        if let Some(host) = opt_str(args, "host")? {
            command.extend(["--host".to_owned(), host.to_owned()]);
        }
        self.run_cli_json(&command)
    }

    /// Closes one tunnel through the local CLI when this is the control machine.
    fn tunnel_close(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        if !self.is_control() {
            return Err(ToolError::refused("tunnels are opened by the control machine"));
        }
        let name = req_str(args, "name")?;
        self.audit_control_write("tunnel_close", json!({"name": name}))?;
        self.run_cli_json(&["tunnel".to_owned(), "close".to_owned(), name.to_owned()])
    }

    /// Runs `secret_request` on the home.
    fn secret_request(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        self.lease_gate(&self.opts.session)?;
        let name = SecretName::parse(req_str(args, "name")?)?;
        let request = vault::request_secret(self.ctx.deps(), &name, self.ctx.clock().now())?;
        let store = self.global()?;
        self.record(
            &store,
            None,
            LedgerKind::Note,
            "secret_request",
            json!({"name": name.as_str(), "expires_at": request.expires_at.as_rfc3339()}),
            format!("mcp requested a one-time paste page for {name}"),
        )?;
        Ok(serde_json::to_value(request)?)
    }

    /// Starts a secret request on the home and keeps its page alive from this control process.
    ///
    /// The home's `secret_request` checks the lease and records the actor. This process
    /// keeps the one-shot MCP connection open for the page lifetime, forwards the home's
    /// loopback port through a local tunnel and returns only the local URL and expiry.
    fn secret_request_control(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let name = SecretName::parse(req_str(args, "name")?)?;
        let control = self
            .ctx
            .control()
            .ok_or_else(|| ToolError::refused("this command needs a control machine"))?;
        let token = self.remote_token(true)?;
        let arguments = json!({"name": name.as_str()});
        let input = self.remote_input(&token, "secret_request", &arguments);
        let (mut child, mut reader) = self.spawn_remote_mcp(control, &input)?;

        let mut call_result = None;
        let mut call_error = None;
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|error| ToolError::new("remote", format!("secret request: {error}")))?;
            if read == 0 {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(2) {
                continue;
            }
            if let Some(error) = value.get("error") {
                call_error = Some(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("remote error")
                        .to_owned(),
                );
            } else {
                call_result = value.get("result").cloned();
            }
            break;
        }
        if let Some(message) = call_error {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::new("remote", message));
        }
        let Some(result) = call_result else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::new("remote", "the home did not answer the secret request"));
        };
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("remote tool failed");
            let _ = child.kill();
            let _ = child.wait();
            return Err(parse_tool_text(text));
        }
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("remote", "secret request answer has no text content"))?;
        let request: SecretRequest = serde_json::from_str(text)
            .map_err(|error| ToolError::new("remote", format!("secret request answer: {error}")))?;
        let Some((remote_port, path)) = loopback_url(&request.url) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::new(
                "remote",
                format!("secret request returned a non-loopback URL: {}", request.url),
            ));
        };

        let tunnel_name = format!("secret-{name}");
        let local_port = match self.ensure_local_tunnel(&tunnel_name, remote_port) {
            Ok(port) => port,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };

        // Keep the home page alive until its expiry. Dropping stdin closes the remote MCP
        // loop, which stops the paste page.
        let stdin = child.stdin.take();
        let expires_at = request.expires_at;
        let now = self.ctx.clock().now();
        let secs = expires_at.unix_secs().saturating_sub(now.unix_secs());
        let secs = u64::try_from(secs.clamp(1, 600)).unwrap_or(1);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            drop(stdin);
            let mut rest = String::new();
            let _ = reader.read_to_string(&mut rest);
            let _ = child.wait();
            let mut io = crate::BufferIo::new("");
            let _ = crate::run(
                vec![
                    "vowp".to_owned(),
                    "--json".to_owned(),
                    "tunnel".to_owned(),
                    "close".to_owned(),
                    tunnel_name,
                ],
                &mut io,
            );
        });

        let request = SecretRequest {
            name,
            url: format!("http://127.0.0.1:{local_port}{path}"),
            expires_at,
            single_use: true,
        };
        Ok(serde_json::to_value(request)?)
    }

    /// Reuses or opens the local forward for one secret request.
    fn ensure_local_tunnel(&self, name: &str, remote_port: u16) -> Result<u16, ToolError> {
        let tunnels = self.run_cli_json(&["tunnel".to_owned(), "ls".to_owned()])?;
        if let Some(existing) = tunnels
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"].as_str() == Some(name)))
        {
            if existing["remote_port"].as_u64() == Some(u64::from(remote_port))
                && let Some(port) = existing["local_port"]
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
            {
                return Ok(port);
            }
            self.run_cli_json(&["tunnel".to_owned(), "close".to_owned(), name.to_owned()])?;
        }
        let tunnel = self.run_cli_json(&[
            "tunnel".to_owned(),
            "open".to_owned(),
            name.to_owned(),
            "--remote-port".to_owned(),
            remote_port.to_string(),
        ])?;
        tunnel["local_port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| ToolError::new("remote", "tunnel open returned no local port"))
    }

    /// Writes a capsule so another app can continue the session.
    fn session_handoff(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.target_session(opt_str(args, "session")?)?;
        self.lease_gate(&session)?;
        let global = self.global()?;
        let row = global.require_session(&session)?;
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

    /// Returns the capsule, tail and lease for continuing the session.
    fn session_resume(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let session = self.target_session(opt_str(args, "session")?)?;
        let takeover_requested = opt_bool(args, "takeover")?.unwrap_or(false);
        let limit = opt_u32(args, "limit")?.unwrap_or(20).clamp(1, 200) as usize;
        let global = self.global()?;
        let row = global.require_session(&session)?;
        let project = self.session_project(&row)?;

        if takeover_requested {
            let surface_id = opt_str(args, "surface")?
                .map(|text| SurfaceId(text.to_owned()))
                .unwrap_or_else(|| self.opts.surface.clone());
            let now = self.ctx.clock().now();
            let surface = Surface {
                id: surface_id,
                kind: SurfaceKind::Mcp,
                node: self.ctx.deps().node.clone(),
                holder: "mcp".to_owned(),
                app: Some("vowp".to_owned()),
                cwd: None,
                metadata: json!({}),
                created_at: now,
                last_seen_at: now,
            };
            let store = self.project_store(&project)?;
            let deps = self.deps_for_project(&project)?;
            let guard = LeaseGuard::acquire(
                &deps,
                &store,
                &AcquireLease {
                    session: session.clone(),
                    surface,
                    takeover: true,
                },
            )?;
            crate::remote::write_private(&self.opts.token_file, guard.token().as_bytes())?;
            // Keep the guard's heartbeat alive for the MCP process lifetime. Dropping it
            // would release the lease this resume just acquired.
            std::mem::forget(guard);
        } else {
            self.lease_gate_optional(&session)?;
        }

        let capsule = global.latest_capsule(&session)?;
        let tail = match SessionLog::open_read(self.ctx.paths(), &session) {
            Ok(reader) => match capsule.as_ref() {
                // The capsule covers events through `at_seq`; return the last `limit`
                // events after it, not the first ones.
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
        let lease = global.lease(&session)?.map(|lease| LeaseStatus {
            held: true,
            holder: Some(lease.holder),
            kind: Some(lease.surface_kind),
            node: Some(lease.node),
            acquired_at: Some(lease.acquired_at),
            heartbeat_at: Some(lease.heartbeat_at),
            token: None,
        });

        let store = self.project_store(&project)?;
        self.record(
            &store,
            Some(project),
            LedgerKind::Note,
            "session_resume",
            json!({
                "session": session.as_str(),
                "takeover": takeover_requested,
                "capsule": capsule.as_ref().map(|capsule| capsule.capsule_id.as_str()),
                "tail": tail.len(),
            }),
            format!("mcp resumed {session}"),
        )?;
        Ok(json!({
            "session": row,
            "capsule": capsule,
            "tail": tail,
            "lease": lease,
        }))
    }

    /// Runs the home CLI with `--json` and parses its one document.
    fn remote_json(&self, args: &[&str]) -> Result<Value, ToolError> {
        let control = self
            .ctx
            .control()
            .ok_or_else(|| ToolError::refused("this command needs a control machine"))?;
        let mut remote = vec![
            "--json".to_owned(),
            "--home".to_owned(),
            control.config.remote_home.clone(),
        ];
        remote.extend(args.iter().map(|arg| (*arg).to_owned()));
        let argv = crate::remote::build_ssh_argv(&control.config.ssh_target, &remote, false);
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| ToolError::new("remote", "empty ssh argv"))?;
        let output = Command::new(program)
            .args(rest)
            .output()
            .map_err(|error| ToolError::new("remote", format!("ssh {}: {error}", control.config.ssh_target)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(ToolError::new(
                "remote",
                format!(
                    "ssh {} failed: {}{}",
                    control.config.ssh_target,
                    stdout.trim(),
                    stderr.trim()
                ),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| ToolError::new("remote", format!("invalid JSON from the home: {error}")))
    }

    /// Reads the local lease token for a routed write.
    fn remote_token(&self, require: bool) -> Result<String, ToolError> {
        let token = match std::fs::read_to_string(&self.opts.token_file) {
            Ok(text) => text.trim_end_matches(['\r', '\n']).to_owned(),
            Err(_error) if !require => String::new(),
            Err(error) => {
                return Err(ToolError::new(
                    "not_found",
                    format!("lease token file {}: {error}", self.opts.token_file.display()),
                ));
            }
        };
        if require && token.is_empty() {
            return Err(ToolError::invalid("lease token file is empty"));
        }
        Ok(token)
    }

    /// Builds the remote shell command that runs a one-shot `vowp mcp` on the home.
    fn remote_mcp_script(&self, remote_home: &str) -> String {
        format!(
            "d=$(mktemp -d) || exit 1; trap 'rm -rf \"$d\"' EXIT; umask 077; \
             IFS= read -r token; printf '%s' \"$token\" > \"$d/token\"; \
             vowp {} {} mcp --session {} --surface {} --token-file \"$d/token\"",
            shell_quote("--home"),
            shell_quote(remote_home),
            shell_quote(self.opts.session.as_str()),
            shell_quote(&self.opts.surface.0),
        )
    }

    /// Builds the four JSON-RPC lines for one routed `tools/call`.
    fn remote_input(&self, token: &str, tool: &str, arguments: &Value) -> String {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "vowp-control", "version": env!("CARGO_PKG_VERSION")},
            }
        });
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        format!(
            "{token}\n{initialize}\n{initialized}\n{call}\n",
            initialize = serde_json::to_string(&initialize).unwrap_or_default(),
            initialized = serde_json::to_string(&initialized).unwrap_or_default(),
            call = serde_json::to_string(&call).unwrap_or_default(),
        )
    }

    /// Spawns the remote MCP process and leaves its stdin open for the caller.
    fn spawn_remote_mcp(
        &self,
        control: &ControlHome,
        input: &str,
    ) -> Result<(std::process::Child, BufReader<std::process::ChildStdout>), ToolError> {
        let script = self.remote_mcp_script(&control.config.remote_home);
        let mut child = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                &control.config.ssh_target,
                &script,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| ToolError::new("remote", format!("ssh {}: {error}", control.config.ssh_target)))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(input.as_bytes())
                .map_err(|error| ToolError::new("remote", format!("ssh stdin: {error}")))?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::new("remote", "ssh produced no stdout"))?;
        Ok((child, BufReader::new(stdout)))
    }

    /// Sends one `tools/call` to a one-shot `vowp mcp` on the home.
    fn remote_tool_call(&self, tool: &str, arguments: &Value, require_token: bool) -> Result<Value, ToolError> {
        let control = self
            .ctx
            .control()
            .ok_or_else(|| ToolError::refused("this command needs a control machine"))?;
        let token = self.remote_token(require_token)?;
        let input = self.remote_input(&token, tool, arguments);
        let (mut child, mut reader) = self.spawn_remote_mcp(control, &input)?;
        drop(child.stdin.take());
        let mut stdout = String::new();
        reader
            .read_to_string(&mut stdout)
            .map_err(|error| ToolError::new("remote", format!("ssh stdout: {error}")))?;
        let status = child
            .wait()
            .map_err(|error| ToolError::new("remote", format!("ssh {}: {error}", control.config.ssh_target)))?;

        let mut call_result = None;
        let mut call_error = None;
        for line in stdout.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(2) {
                continue;
            }
            if let Some(error) = value.get("error") {
                call_error = Some(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("remote error")
                        .to_owned(),
                );
            } else {
                call_result = value.get("result").cloned();
            }
        }
        if let Some(message) = call_error {
            return Err(ToolError::new("remote", message));
        }
        let Some(result) = call_result else {
            return Err(ToolError::new(
                "remote",
                format!(
                    "no tools/call answer from the home (exit {})",
                    status.code().unwrap_or(-1)
                ),
            ));
        };
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("remote tool failed");
            return Err(parse_tool_text(text));
        }
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("remote", "tools/call answer has no text content"))?;
        serde_json::from_str(text).map_err(|error| ToolError::new("remote", format!("tool answer: {error}")))
    }

    /// Runs a local CLI command on the control machine and parses its JSON.
    fn run_cli_json(&self, args: &[String]) -> Result<Value, ToolError> {
        let mut full = vec!["vowp".to_owned(), "--json".to_owned()];
        full.extend(args.iter().cloned());
        let mut io = crate::BufferIo::new("");
        let code = crate::run(full, &mut io);
        if code != 0 {
            let kind = match code {
                2 => "invalid_input",
                3 => "not_found",
                4 => "conflict",
                5 => "refused",
                6 => "limit",
                7 => "remote",
                _ => "internal",
            };
            return Err(ToolError::new(kind, io.stderr().trim().to_owned()));
        }
        serde_json::from_str(io.stdout().trim())
            .map_err(|error| ToolError::new("internal", format!("CLI JSON: {error}")))
    }
}

/// Splits a loopback URL into its port and absolute path.
fn loopback_url(url: &str) -> Option<(u16, String)> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (port, path) = rest.split_once('/')?;
    Some((port.parse().ok()?, format!("/{path}")))
}

/// Filters a snapshot to one project.
fn filter_snapshot(
    mut snapshot: vowp_protocol::Snapshot,
    project: Option<&str>,
) -> Result<vowp_protocol::Snapshot, ToolError> {
    if let Some(project) = project {
        let project = ProjectName::parse(project)?;
        snapshot.projects.retain(|card| card.name == project);
    }
    Ok(snapshot)
}

/// Resolves an inbox id or unique prefix from a merged list.
fn resolve_inbox(items: &[InboxItem], input: &str) -> Result<InboxItem, ToolError> {
    if let Ok(id) = InboxId::parse(input)
        && let Some(item) = items.iter().find(|item| item.id == id)
    {
        return Ok(item.clone());
    }
    if input.len() < 6 {
        return Err(ToolError::invalid("inbox id prefix must be at least 6 characters"));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<&InboxItem> = items
        .iter()
        .filter(|item| item.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [item] => Ok((*item).clone()),
        [] => Err(ToolError::new("not_found", format!("inbox item {input}"))),
        many => Err(ToolError::invalid(format!(
            "ambiguous inbox prefix {input}: {}",
            many.iter()
                .map(|item| item.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Parses a remote tool error text into its kind and message.
fn parse_tool_text(text: &str) -> ToolError {
    let Some((kind, message)) = text.split_once(": ") else {
        return ToolError::new("remote", text);
    };
    let known = [
        "invalid_input",
        "not_found",
        "conflict",
        "guard",
        "limited",
        "limit",
        "refused",
        "remote",
        "internal",
        "io",
        "db",
        "parse",
        "config",
        "adapter_failed",
        "adapter_unavailable",
        "timeout",
        "auth",
    ];
    if known.contains(&kind) {
        ToolError::new(kind, message)
    } else {
        ToolError::new("remote", text)
    }
}

/// Quotes one YAML scalar with JSON's double-quoted form.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Reads a required string member.
fn req_str<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid(format!("missing string argument {name}")))
}

/// Reads an optional string member.
fn opt_str<'a>(args: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(ToolError::invalid(format!("{name} must be a string"))),
    }
}

/// Reads an optional boolean member.
fn opt_bool(args: &Map<String, Value>, name: &str) -> Result<Option<bool>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::invalid(format!("{name} must be a boolean"))),
    }
}

/// Reads an optional unsigned 64-bit member.
fn opt_u64(args: &Map<String, Value>, name: &str) -> Result<Option<u64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("{name} must be a non-negative integer"))),
        Some(_) => Err(ToolError::invalid(format!("{name} must be an integer"))),
    }
}

/// Reads an optional unsigned 32-bit member.
fn opt_u32(args: &Map<String, Value>, name: &str) -> Result<Option<u32>, ToolError> {
    Ok(opt_u64(args, name)?.map(|value| value.min(u64::from(u32::MAX)) as u32))
}
