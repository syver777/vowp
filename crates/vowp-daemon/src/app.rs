//! The axum application: state, routes and request handlers.
//!
//! Read requests open their own read-only [`vowp_store::Store`] against the served home. The
//! two writes, `POST /api/v1/inbox/{id}/answer` and `POST /api/v1/sessions/{id}/switch`,
//! open the same home with [`vowp_store::Store::open_writable`], which never creates or
//! migrates it. The store handle is `Send` but not `Sync`, so a shared handle would serialize
//! requests or require a lock; opening per request also gives each request a fresh WAL view.
//! The blocking SQLite and JSONL work runs on `tokio::task::spawn_blocking`.
//!
//! The switch runs the same [`SessionService::switch_with_store`] protocol as
//! `vowp session switch`; the daemon builds one real [`Deps`] at startup and shares it in
//! [`ApiState`]. Tests replace it with fake adapters and a fake window manager so no real
//! harness or tmux server starts.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use tokio::net::TcpListener;
use vowp_core::deps::SystemRng;
use vowp_core::ledger::BoardSnapshot;
use vowp_core::requests::{self, Request};
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, CoreError, Deps, SystemGit, SystemRunner};
use vowp_protocol::api::{
    ApiError, ApiErrorBody, Board, BoardLimit, BoardRun, CostReport, CostRow as ApiCostRow, CostTotal, EventPage,
    Health, InboxAnswerRequest, InboxPage, JobPage, MAX_LINEAGE, NodePage, SessionDetail, SessionPage,
    SessionSwitchRequest,
};
use vowp_protocol::{
    Capsule, CapsuleId, ErrorKind, Executor, ExecutorRef, InboxId, InboxItem, InboxState, NodeName, ProjectName,
    Session, SessionId, SessionState, Switch, SwitchReason, SwitchRequest, SystemClock, Timestamp, VowpConfig,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{CostQuery, CostRow as StoreCostRow, JobFilter, NodePaths, SessionFilter, Store, StoreError};
use vowp_tmux::{Tmux, WindowManager};

use crate::auth::{self, ApiToken};
use crate::config::ProjectSelection;
use crate::error::DaemonError;
use crate::filters::{self, CostQuerySpec, EventQuery};
use crate::stream;

/// Largest accepted answer text, in UTF-8 bytes after trimming.
const MAX_ANSWER_BYTES: usize = 10_000;

/// State shared by every request handler.
#[derive(Clone)]
pub struct ApiState {
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    schema_version: i64,
    stream_poll: Duration,
    stream_heartbeat: Duration,
    board_poll: Duration,
    write_token: Option<ApiToken>,
    session_deps: Option<Arc<Deps>>,
}

impl fmt::Debug for ApiState {
    /// Renders the non-sensitive configuration; paths, the token and dependencies stay hidden.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState")
            .field("project", &self.project)
            .field("node", &self.node)
            .field("schema_version", &self.schema_version)
            .field("write_token", &self.write_token)
            .field("switch_deps", &self.session_deps.is_some())
            .finish_non_exhaustive()
    }
}

impl ApiState {
    /// Builds request state from the resolved home and project selection.
    pub fn new(paths: &NodePaths, selection: &ProjectSelection) -> Self {
        Self {
            paths: paths.clone(),
            project: selection.project.clone(),
            node: selection.node.clone(),
            schema_version: selection.schema_version,
            stream_poll: stream::POLL_INTERVAL,
            stream_heartbeat: stream::HEARTBEAT_INTERVAL,
            board_poll: stream::BOARD_POLL_INTERVAL,
            write_token: None,
            session_deps: None,
        }
    }

    /// Returns this state with the write token replaced.
    ///
    /// The binary passes [`ApiToken::from_env`]; `None` keeps writes disabled. Tests pass a
    /// fixed token so they never touch the process environment.
    pub fn with_api_token(mut self, token: Option<ApiToken>) -> Self {
        self.write_token = token;
        self
    }

    /// Returns this state with the session-switch dependencies replaced.
    ///
    /// The binary passes [`build_deps`]; tests pass a [`Deps`] with fake adapters and a fake
    /// window manager, so a switch test never starts a real harness or tmux server. Without
    /// dependencies the switch route answers `500` and no other route is affected.
    pub fn with_session_deps(mut self, deps: Deps) -> Self {
        self.session_deps = Some(Arc::new(deps));
        self
    }

    /// Returns this state with the SSE poll and heartbeat intervals replaced.
    ///
    /// The daemon uses the module defaults; integration tests shorten both so a heartbeat
    /// assertion stays fast without changing production timing.
    pub fn with_stream_intervals(mut self, poll: Duration, heartbeat: Duration) -> Self {
        self.stream_poll = poll;
        self.stream_heartbeat = heartbeat;
        self
    }

    /// Returns this state with the board snapshot poll interval replaced.
    ///
    /// The daemon polls the board every second in production; integration tests shorten
    /// the interval so the first and changed snapshots arrive quickly.
    pub fn with_board_poll_interval(mut self, poll: Duration) -> Self {
        self.board_poll = poll;
        self
    }

    /// Returns the served home.
    pub(crate) fn paths(&self) -> &NodePaths {
        &self.paths
    }

    /// Returns the interval between SSE log polls.
    pub(crate) fn stream_poll_interval(&self) -> Duration {
        self.stream_poll
    }

    /// Returns the interval between board snapshot polls.
    pub(crate) fn board_poll_interval(&self) -> Duration {
        self.board_poll
    }

    /// Returns the interval between SSE heartbeat frames.
    pub(crate) fn stream_heartbeat_interval(&self) -> Duration {
        self.stream_heartbeat
    }

    /// Opens the served project read-only, mapping failures onto a path-free 500.
    fn open(&self) -> Result<Store, ApiFailure> {
        Store::open_read_only(&self.paths, Some(&self.project)).map_err(|_| store_failure())
    }

    /// Opens the served project writable without creating or migrating the home.
    fn open_writable(&self) -> Result<Store, ApiFailure> {
        Store::open_writable(&self.paths, Some(&self.project)).map_err(|_| store_write_failure())
    }

    /// Answers one open inbox item and queues the same request as the CLI.
    ///
    /// The item update and the `answer_inbox` request row go through the same store methods
    /// the CLI uses; the supervisor applies the request on its next tick. Job state is not
    /// touched here.
    fn answer_inbox(&self, id: &InboxId, answer: &str) -> Result<InboxItem, ApiFailure> {
        let store = self.open_writable()?;
        let item = store
            .inbox_item(id)
            .map_err(|_| store_failure())?
            .ok_or_else(|| ApiFailure::not_found(format!("inbox item {id} not found")))?;
        if item.state != InboxState::Open {
            return Err(ApiFailure::conflict(format!("inbox item {id} is not open")));
        }
        let answered = store.answer_inbox(id, answer, "web").map_err(answer_store_failure)?;
        if answered.job_id.is_some() {
            requests::push(&store, &Request::AnswerInbox { id: id.clone() }).map_err(|_| request_write_failure())?;
        }
        Ok(answered)
    }

    /// Switches one session through the same service as `vowp session switch`.
    ///
    /// The guards mirror the CLI: a running turn needs `force`, a switching or closed
    /// session is a conflict, and a target provider with an active limit is refused. The
    /// switch may drain an active turn for up to five seconds; it runs on the caller's
    /// blocking thread. The store is opened writable and never creates or migrates a home.
    fn switch_session(
        &self,
        id: &SessionId,
        target: Executor,
        reason: SwitchReason,
        force: bool,
    ) -> Result<Switch, ApiFailure> {
        let store = self.open_writable()?;
        let session = store
            .session(id)
            .map_err(|_| store_failure())?
            .ok_or_else(|| ApiFailure::not_found(format!("session {id} not found")))?;
        match session.state {
            SessionState::Running if !force => {
                return Err(ApiFailure::conflict(format!(
                    "session {id} is running a turn; pass force to drain and stop it"
                )));
            }
            SessionState::Switching => {
                return Err(ApiFailure::conflict(format!("session {id} is already switching")));
            }
            SessionState::Closed => {
                return Err(ApiFailure::conflict(format!("session {id} is closed; cannot switch")));
            }
            SessionState::Idle | SessionState::Running | SessionState::Blocked => {}
        }
        // The fixture clock wins so a hermetic test can control the active-limit window;
        // the production daemon has no injected clock and reads the store's system clock.
        let now = self
            .session_deps
            .as_ref()
            .map_or_else(|| store.clock().now(), |deps| deps.clock.now());
        let provider = target.provider();
        if store
            .active_limits(now)
            .map_err(|_| store_failure())?
            .iter()
            .any(|limit| limit.provider == provider)
        {
            return Err(ApiFailure::conflict(format!("provider {provider} is limited")));
        }
        let deps = self.session_deps.clone().ok_or_else(switch_deps_failure)?;
        let request = SwitchRequest {
            session_id: id.clone(),
            to: ExecutorRef {
                executor: target,
                node: None,
            },
            reason,
            drain_secs: 5,
        };
        SessionService::new((*deps).clone())
            .switch_with_store(&store, &request)
            .map_err(switch_failure)
    }

    /// Returns the jobs matching `filter`.
    fn jobs(&self, filter: &JobFilter) -> Result<JobPage, ApiFailure> {
        let store = self.open()?;
        let jobs = store.list_jobs(filter).map_err(|_| store_failure())?;
        Ok(JobPage {
            total: usize_to_u64(jobs.len()),
            items: jobs,
            next: None,
        })
    }

    /// Returns the sessions matching `filter`.
    fn sessions(&self, filter: &SessionFilter) -> Result<SessionPage, ApiFailure> {
        let store = self.open()?;
        let sessions = store.list_sessions(filter).map_err(|_| store_failure())?;
        Ok(SessionPage {
            total: usize_to_u64(sessions.len()),
            items: sessions,
            next: None,
        })
    }

    /// Returns one session's metadata, latest capsule and bounded parent lineage.
    ///
    /// An unknown session is a `404`; a store failure is a path-free `500`. The lineage
    /// follows `parent_session_id` from the nearest parent outwards and stops on a missing
    /// parent row or after [`MAX_LINEAGE`] entries.
    fn session_detail(&self, id: &SessionId) -> Result<SessionDetail, ApiFailure> {
        let store = self.open()?;
        let session = store
            .session(id)
            .map_err(|_| store_failure())?
            .ok_or_else(|| ApiFailure::not_found(format!("session {id} not found")))?;
        let latest_capsule = store.latest_capsule(id).map_err(|_| store_failure())?;
        let lineage = lineage(&store, &session)?;
        Ok(SessionDetail {
            session,
            latest_capsule,
            lineage,
        })
    }

    /// Returns one capsule only when it belongs to the path session.
    ///
    /// Unknown sessions, unknown capsules and capsules stored for another session are all
    /// `404`, so a caller cannot use this route to read a capsule across sessions.
    fn capsule(&self, session_id: &SessionId, capsule_id: &CapsuleId) -> Result<Capsule, ApiFailure> {
        let store = self.open()?;
        if store.session(session_id).map_err(|_| store_failure())?.is_none() {
            return Err(ApiFailure::not_found(format!("session {session_id} not found")));
        }
        match store.capsule(capsule_id).map_err(|_| store_failure())? {
            Some(capsule) if capsule.session_id == *session_id => Ok(capsule),
            _ => Err(ApiFailure::not_found(format!("capsule {capsule_id} not found"))),
        }
    }

    /// Returns the served project's registered nodes, ordered by name.
    fn nodes(&self) -> Result<NodePage, ApiFailure> {
        let store = self.open()?;
        let nodes = store.list_nodes().map_err(|_| store_failure())?;
        Ok(NodePage {
            total: usize_to_u64(nodes.len()),
            items: nodes,
            next: None,
        })
    }

    /// Returns the served project's inbox items in `state`.
    fn inbox(&self, state: InboxState) -> Result<InboxPage, ApiFailure> {
        let store = self.open()?;
        let items = store.list_inbox(Some(state)).map_err(|_| store_failure())?;
        Ok(InboxPage {
            total: usize_to_u64(items.len()),
            items,
            next: None,
        })
    }

    /// Returns the served project's cost report grouped and bounded by `spec`.
    fn cost(&self, spec: CostQuerySpec) -> Result<CostReport, ApiFailure> {
        let store = self.open()?;
        let rows = store
            .cost_totals(
                &CostQuery {
                    since: spec.since,
                    until: spec.until,
                    project: Some(self.project.clone()),
                    job_id: None,
                },
                spec.group,
            )
            .map_err(|_| store_failure())?;
        Ok(build_cost_report(spec.by, rows))
    }

    /// Returns one page of the session's log read from disk.
    fn events(&self, id: &SessionId, query: EventQuery) -> Result<EventPage, ApiFailure> {
        let store = self.open()?;
        match store.session(id).map_err(|_| store_failure())? {
            Some(_) => {}
            None => {
                return Err(ApiFailure::not_found(format!("session {id} not found")));
            }
        }
        let reader = SessionLog::open_read(&self.paths, id).map_err(|_| log_failure())?;
        let events = reader
            .read_from(query.after_seq, Some(query.limit as usize))
            .map_err(|_| log_failure())?;
        let next = if query.limit > 0 && events.len() == query.limit as usize {
            events.last().map(|event| event.seq)
        } else {
            None
        };
        Ok(EventPage {
            total: usize_to_u64(events.len()),
            items: events,
            next,
        })
    }

    /// Verifies that `id` names a known session before the SSE response starts.
    ///
    /// A missing or compacted log is not an error here: the stream opens normally and its
    /// first poll sends one `resync` frame, which is the documented recovery. Only an
    /// unknown session is a plain `404` [`ApiError`] before streaming.
    fn check_stream_session(&self, id: &SessionId) -> Result<(), ApiFailure> {
        let store = self.open()?;
        if store.session(id).map_err(|_| store_failure())?.is_none() {
            return Err(ApiFailure::not_found(format!("session {id} not found")));
        }
        Ok(())
    }

    /// Returns the served project's registered [`VowpConfig`], filled from its registry row.
    ///
    /// The registry JSON is written by `vowp init`; a project registered without `[project].path`
    /// (`vowp init` leaves it out) falls back to the registry path, matching the CLI and the
    /// supervisor so `last_checkpoint` reads the project's own checkout.
    fn config(&self, store: &Store) -> Result<VowpConfig, ApiFailure> {
        let project = store
            .project(&self.project)
            .map_err(|_| store_failure())?
            .ok_or_else(config_failure)?;
        let mut config: VowpConfig = serde_json::from_value(project.config).map_err(|_| config_failure())?;
        if config.project.name != self.project {
            return Err(config_failure());
        }
        config.validate().map_err(|_| config_failure())?;
        if config.project.path.is_none() {
            config.project.path = Some(project.path);
        }
        Ok(config)
    }

    /// Returns the board snapshot as the exact `vowp board --json` document.
    ///
    /// The handler and the board SSE stream both collect through this method, so a stream
    /// frame and `GET /api/v1/board` are built by the same code path.
    pub(crate) fn board(&self) -> Result<Board, ApiFailure> {
        let store = self.open()?;
        let config = self.config(&store)?;
        let snapshot = BoardSnapshot::collect(&store, &config, store.clock().now()).map_err(|_| board_failure())?;
        Ok(Board {
            at: snapshot.at,
            project: snapshot.project,
            jobs: snapshot.jobs_by_state,
            running: snapshot
                .running
                .iter()
                .map(|run| BoardRun {
                    job: run.job.clone(),
                    executor: run.executor.as_ref().map(ToString::to_string),
                    age_secs: run.age_secs,
                    last_event_age_secs: run.last_event_age_secs,
                })
                .collect(),
            slots_busy: snapshot.slots_busy,
            slots_total: snapshot.slots_total,
            planner: snapshot.planner_executor.as_ref().map(ToString::to_string),
            keeper: snapshot.keeper_executor.as_ref().map(ToString::to_string),
            limits: snapshot
                .limits
                .iter()
                .map(|limit| BoardLimit {
                    provider: limit.provider.clone(),
                    kind: limit.kind,
                    resets_at: limit.resets_at,
                })
                .collect(),
            cost_today_usd: snapshot.cost_today.usd,
            inbox_open: snapshot.inbox_open,
            last_checkpoint: snapshot.last_checkpoint,
        })
    }
}

/// Builds the real session dependencies for the selected home and project.
///
/// This mirrors the CLI context construction without depending on the CLI crate: the real
/// adapter registry, tmux window manager, git and command runner, system
/// clock, null log and system RNG. The registered project config is parsed, validated and
/// filled with the registry path when it omits one, matching the read handlers. The function
/// only reads an existing home; it never creates or migrates one.
pub fn build_deps(paths: &NodePaths, selection: &ProjectSelection) -> Result<Deps, DaemonError> {
    let store = Store::open_read_only(paths, Some(&selection.project)).map_err(DaemonError::from_store)?;
    let project = store
        .project(&selection.project)
        .map_err(DaemonError::from_store)?
        .ok_or_else(|| DaemonError::Config("the selected project is not registered".to_owned()))?;
    let mut config: VowpConfig = serde_json::from_value(project.config)
        .map_err(|_| DaemonError::Config("the project configuration could not be read".to_owned()))?;
    if config.project.name != selection.project {
        return Err(DaemonError::Config(
            "the project configuration names a different project".to_owned(),
        ));
    }
    config
        .validate()
        .map_err(|_| DaemonError::Config("the project configuration is invalid".to_owned()))?;
    if config.project.path.is_none() {
        config.project.path = Some(project.path);
    }
    let config = Arc::new(config);
    let cmd: Arc<dyn vowp_core::CommandRunner> = Arc::new(SystemRunner);
    let git = Arc::new(SystemGit::new(Arc::clone(&cmd)));
    let windows: Arc<dyn WindowManager> = Arc::new(Tmux::for_home(paths.root()));
    let adapters = AdapterRegistry::from_config(&config)
        .map_err(|_| DaemonError::Config("the project harness configuration is unusable".to_owned()))?;
    Ok(Deps {
        paths: paths.clone(),
        config,
        node: selection.node.clone(),
        clock: Arc::new(SystemClock),
        log: Arc::new(vowp_protocol::NullLog),
        adapters: Arc::new(adapters),
        windows,
        git,
        cmd,
        rng: Arc::new(SystemRng::new()),
    })
}

/// Walks the stored parent-session links, nearest parent first, bounded by [`MAX_LINEAGE`].
///
/// The walk stops when a parent row is missing, so a dangling `parent_session_id` ends the
/// chain instead of failing the request. It also stops when a parent session repeats, and
/// the bound caps a longer chain.
fn lineage(store: &Store, session: &Session) -> Result<Vec<SessionId>, ApiFailure> {
    let mut lineage = Vec::new();
    let mut next = session.parent_session_id.clone();
    while lineage.len() < MAX_LINEAGE {
        let Some(parent_id) = next else {
            break;
        };
        let Some(parent) = store.session(&parent_id).map_err(|_| store_failure())? else {
            break;
        };
        if lineage.contains(&parent.id) {
            break;
        }
        next = parent.parent_session_id;
        lineage.push(parent.id);
    }
    Ok(lineage)
}

/// Builds the router: the static board page at `/` and its phone alias `/board`, the static
/// session pages at `/sessions` and `/sessions/{id}`, the static capsule page at
/// `/sessions/{id}/capsules/{capsule_id}`, the static cost, inbox and nodes pages at `/cost`,
/// `/inbox` and `/nodes`, the PWA manifest, icon and service worker, plus the API under
/// `/api/v1`. The only non-GET API routes are the token-gated inbox answer and session switch
/// writes.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/", get(board_page))
        .route("/board", get(board_page))
        .route("/sessions", get(sessions_page))
        .route("/sessions/{id}", get(sessions_page))
        .route("/sessions/{id}/capsules/{capsule_id}", get(capsule_page))
        .route("/cost", get(cost_page))
        .route("/inbox", get(inbox_page))
        .route("/nodes", get(nodes_page))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/api/v1/health", get(health))
        .route("/api/v1/board", get(board))
        .route("/api/v1/board/stream", get(board_stream))
        .route("/api/v1/jobs", get(jobs))
        .route("/api/v1/sessions", get(sessions))
        .route("/api/v1/sessions/{id}", get(session_detail))
        .route("/api/v1/sessions/{id}/capsules/{capsule_id}", get(session_capsule))
        .route("/api/v1/sessions/{id}/events", get(session_events))
        .route("/api/v1/sessions/{id}/events/stream", get(session_events_stream))
        .route("/api/v1/sessions/{id}/switch", post(switch_session))
        .route("/api/v1/inbox", get(inbox))
        .route("/api/v1/inbox/{id}/answer", post(answer_inbox))
        .route("/api/v1/nodes", get(nodes))
        .route("/api/v1/cost", get(cost))
        .fallback(unknown_endpoint)
        .with_state(state)
}

/// Serves the API on `listener` until the process stops.
pub async fn serve(listener: TcpListener, state: ApiState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

/// Serves the API until `shutdown` resolves.
pub async fn serve_with_shutdown<F>(listener: TcpListener, state: ApiState, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// The embedded static board page, served read-only at `/` and `/board`.
const BOARD_PAGE: &str = include_str!("../web/index.html");

/// Serves the static board page; it fetches `/api/v1/board`, follows the board stream and
/// keeps a five-second poll fallback.
///
/// The same shell is served at `/` and `/board`. Its card layout and `max-width: 360px`
/// rules make it usable on a phone without a separate page or a second API mapping.
async fn board_page() -> Html<&'static str> {
    Html(BOARD_PAGE)
}

/// The embedded static session shell, served read-only at `/sessions` and `/sessions/{id}`.
const SESSIONS_PAGE: &str = include_str!("../web/sessions.html");

/// Serves the static sessions shell.
///
/// The page reads its own path: `/sessions` renders the read-only session list and
/// `/sessions/{id}` renders one session's metadata, then follows the per-session SSE stream
/// and falls back to polling when the stream is unavailable or drops. Both routes serve the
/// same shell; only the read-only session and event endpoints are fetched.
async fn sessions_page() -> Html<&'static str> {
    Html(SESSIONS_PAGE)
}

/// The embedded static capsule shell, served read-only at
/// `/sessions/{id}/capsules/{capsule_id}`.
const CAPSULE_PAGE: &str = include_str!("../web/capsule.html");

/// Serves the static capsule shell.
///
/// The page reads its own path and fetches only
/// `/api/v1/sessions/{id}/capsules/{capsuleId}`. It renders the capsule's metadata and
/// structured fields read-only: no Markdown parser, editor or write control is involved.
async fn capsule_page() -> Html<&'static str> {
    Html(CAPSULE_PAGE)
}

/// The embedded static cost shell, served read-only at `/cost`.
const COST_PAGE: &str = include_str!("../web/cost.html");

/// Serves the static cost shell; it fetches `/api/v1/cost?by=` on load and every five seconds.
///
/// The `by` selector offers job, day, role, provider and model. The page renders the grouped
/// rows and totals, shows `?` for an unknown USD total and never treats it as zero, and has no
/// charts or price-table editing.
async fn cost_page() -> Html<&'static str> {
    Html(COST_PAGE)
}

/// The embedded static inbox shell, served at `/inbox`.
const INBOX_PAGE: &str = include_str!("../web/inbox.html");

/// Serves the static inbox shell; it fetches `/api/v1/inbox?state=` on load and every five seconds.
///
/// The state selector offers open, all, answered and dismissed; `all` fetches the three API
/// states and merges their items. Open items carry the answer form: it stores the daemon write
/// token in `sessionStorage`, posts `{"answer":"<text>"}` to `/api/v1/inbox/{id}/answer` with
/// an `Authorization: Bearer` header, and refreshes the list on `202`. Reads stay token-free.
/// There is no dismiss or other write action.
async fn inbox_page() -> Html<&'static str> {
    Html(INBOX_PAGE)
}

/// The embedded static nodes shell, served read-only at `/nodes`.
const NODES_PAGE: &str = include_str!("../web/nodes.html");

/// Serves the static nodes shell; it fetches `/api/v1/nodes` on load and every five seconds.
///
/// The page renders each stored node's name, SSH target, local/remote flag, last-seen time,
/// `capabilities.vowp` version and a compact capabilities/probe summary. It is read-only: no
/// add, remove or probe control and no SSH or write path is offered.
async fn nodes_page() -> Html<&'static str> {
    Html(NODES_PAGE)
}

/// The embedded PWA manifest.
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");

/// Serves the PWA manifest as `application/manifest+json`.
async fn manifest() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST)
}

/// The embedded PWA icon.
const ICON: &str = include_str!("../web/icon.svg");

/// Serves the SVG icon used by the manifest and the page chrome.
async fn icon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], ICON)
}

/// The embedded hand-written service worker.
const SERVICE_WORKER: &str = include_str!("../web/sw.js");

/// Serves the service worker at the root scope and tells the browser not to cache it.
///
/// `Service-Worker-Allowed: /` lets a worker script served from any path control the whole
/// origin; `Cache-Control: no-cache` makes the browser revalidate the script so a changed
/// shell cache version is picked up on the next page load.
async fn service_worker() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (HeaderName::from_static("service-worker-allowed"), "/"),
        ],
        SERVICE_WORKER,
    )
}

/// Returns the health document for the served project.
async fn health(State(state): State<ApiState>) -> Json<Health> {
    Json(Health {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        node: state.node.clone(),
        project: state.project.clone(),
        schema_version: state.schema_version,
        heartbeat: read_heartbeat(&state.paths),
    })
}

/// Returns the served project's board as the `vowp board --json` document.
async fn board(State(state): State<ApiState>) -> Result<Json<Board>, ApiFailure> {
    let board = blocking(move || state.board()).await?;
    Ok(Json(board))
}

/// Streams the board snapshot as server-sent `board` frames with periodic heartbeats.
///
/// The stream has no cursor or resume: every `board` frame is the exact current
/// `GET /api/v1/board` document, and a frame is sent only when that document changes apart
/// from its `at` timestamp. A failed snapshot collection ends the stream cleanly so the page
/// reconnects and falls back to polling.
async fn board_stream(State(state): State<ApiState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    stream::board_response(state)
}

/// Lists jobs matching the query filters.
async fn jobs(State(state): State<ApiState>, RawQuery(query): RawQuery) -> Result<Json<JobPage>, ApiFailure> {
    let filter = filters::job_filter(query.as_deref()).map_err(ApiFailure::invalid)?;
    let page = blocking(move || state.jobs(&filter)).await?;
    Ok(Json(page))
}

/// Lists sessions matching the query filters.
async fn sessions(State(state): State<ApiState>, RawQuery(query): RawQuery) -> Result<Json<SessionPage>, ApiFailure> {
    let mut filter = filters::session_filter(query.as_deref()).map_err(ApiFailure::invalid)?;
    if filter.project.is_none() {
        filter.project = Some(state.project.clone());
    }
    let page = blocking(move || state.sessions(&filter)).await?;
    Ok(Json(page))
}

/// Returns one session's metadata, latest capsule and bounded parent lineage.
async fn session_detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetail>, ApiFailure> {
    let id = SessionId::parse(&id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let detail = blocking(move || state.session_detail(&id)).await?;
    Ok(Json(detail))
}

/// Returns one capsule belonging to the path session.
async fn session_capsule(
    State(state): State<ApiState>,
    Path((session_id, capsule_id)): Path<(String, String)>,
) -> Result<Json<Capsule>, ApiFailure> {
    let session_id = SessionId::parse(&session_id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let capsule_id = CapsuleId::parse(&capsule_id).map_err(|_| ApiFailure::invalid("invalid capsule id"))?;
    let capsule = blocking(move || state.capsule(&session_id, &capsule_id)).await?;
    Ok(Json(capsule))
}

/// Lists the served project's registered nodes.
async fn nodes(State(state): State<ApiState>) -> Result<Json<NodePage>, ApiFailure> {
    let page = blocking(move || state.nodes()).await?;
    Ok(Json(page))
}

/// Lists the served project's inbox items in one state.
async fn inbox(State(state): State<ApiState>, RawQuery(query): RawQuery) -> Result<Json<InboxPage>, ApiFailure> {
    let filter = filters::inbox_state(query.as_deref()).map_err(ApiFailure::invalid)?;
    let page = blocking(move || state.inbox(filter)).await?;
    Ok(Json(page))
}

/// Answers one open inbox item and queues the supervisor request.
///
/// The bearer token is checked before the id, body or item is read. A daemon without
/// `VOWP_API_TOKEN` answers `503`; a missing or wrong token is `401` with a `Bearer`
/// challenge. The response is the answered `InboxItem` with `202 Accepted`.
async fn answer_inbox(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<InboxItem>), ApiFailure> {
    authorize(&state, &headers)?;
    let id = InboxId::parse(&id).map_err(|_| ApiFailure::invalid("invalid inbox id"))?;
    let request: InboxAnswerRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiFailure::invalid("the request body must be {\"answer\":\"<text>\"}"))?;
    let answer = request.answer.trim().to_owned();
    if answer.is_empty() {
        return Err(ApiFailure::invalid("answer must not be empty"));
    }
    if answer.len() > MAX_ANSWER_BYTES {
        return Err(ApiFailure::invalid("answer must be at most 10000 bytes"));
    }
    let answered = blocking(move || state.answer_inbox(&id, &answer)).await?;
    Ok((StatusCode::ACCEPTED, Json(answered)))
}

/// Switches one local session through the same service as the CLI.
///
/// The bearer token is checked before the id, body or store is read. A daemon without
/// `VOWP_API_TOKEN` answers `503`; a missing or wrong token is `401`. An invalid id, body or
/// `to` value is `400`, an unknown session is `404`, and the running/switch/closed/limit
/// guards are `409`. The response is the completed `Switch`; a failed switch returns an
/// `ApiError` and never claims the executor moved.
async fn switch_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Switch>, ApiFailure> {
    authorize(&state, &headers)?;
    let id = SessionId::parse(&id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let request: SessionSwitchRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiFailure::invalid("the request body must be {\"to\":\"<executor>\"}"))?;
    let target = Executor::parse(&request.to).map_err(|_| ApiFailure::invalid("invalid target executor"))?;
    let switch = blocking(move || state.switch_session(&id, target, request.reason, request.force)).await?;
    Ok(Json(switch))
}

/// Checks the daemon write token before a handler reads the request body or store.
///
/// The comparison is constant-time over the candidate and the configured token is never
/// echoed. A daemon with no token answers `503`; a missing or wrong token is `401`.
fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiFailure> {
    let Some(token) = &state.write_token else {
        return Err(ApiFailure::writes_disabled());
    };
    let Some(candidate) = auth::bearer_token(headers) else {
        return Err(ApiFailure::unauthorized("a bearer token is required"));
    };
    if !token.matches(candidate) {
        return Err(ApiFailure::unauthorized("invalid bearer token"));
    }
    Ok(())
}

/// Returns the served project's cost report as the `vowp cost --json` document.
async fn cost(State(state): State<ApiState>, RawQuery(query): RawQuery) -> Result<Json<CostReport>, ApiFailure> {
    let spec = filters::cost_query(query.as_deref()).map_err(ApiFailure::invalid)?;
    let report = blocking(move || state.cost(spec)).await?;
    Ok(Json(report))
}

/// Returns one page of a session's exact stored events.
async fn session_events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<EventPage>, ApiFailure> {
    let id = SessionId::parse(&id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let query = filters::event_query(query.as_deref()).map_err(ApiFailure::invalid)?;
    let page = blocking(move || state.events(&id, query)).await?;
    Ok(Json(page))
}

/// Streams one session's stored events as server-sent `session` frames.
///
/// The cursor is validated and the session is checked before the SSE response starts, so
/// a bad request or an unknown session is a normal JSON [`ApiError`]. The response then
/// carries one `session` frame per sequence number, periodic `heartbeat` frames and, when
/// the log can no longer serve the cursor, one `resync` frame followed by a clean close.
async fn session_events_stream(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiFailure> {
    let id = SessionId::parse(&id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let after_seq = filters::event_stream_query(query.as_deref()).map_err(ApiFailure::invalid)?;
    let checked = state.clone();
    let check_id = id.clone();
    blocking(move || checked.check_stream_session(&check_id)).await?;
    Ok(stream::response(state, id, after_seq))
}

/// Returns an `ApiError` for an unknown path.
async fn unknown_endpoint() -> ApiFailure {
    ApiFailure::not_found("unknown endpoint")
}

/// Runs blocking store or log work off the async reactor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiFailure> + Send + 'static,
) -> Result<T, ApiFailure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::Internal, "request failed"))?
}

/// The JSON error response for a failed request.
pub(crate) struct ApiFailure {
    status: StatusCode,
    body: ApiError,
}

impl ApiFailure {
    /// Builds a failure with no hint.
    fn new(status: StatusCode, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError {
                error: ApiErrorBody {
                    kind,
                    message: message.into(),
                    hint: None,
                },
            },
        }
    }

    /// A `400` for an invalid filter, id or query value.
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorKind::InvalidInput, message)
    }

    /// A `404` for an unknown session or inbox item.
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorKind::NotFound, message)
    }

    /// A `409` for an item that is no longer open.
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorKind::Conflict, message)
    }

    /// A `401` for a missing or wrong bearer token.
    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, ErrorKind::Auth, message)
    }

    /// A `503` when the daemon was started without a write token.
    fn writes_disabled() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Config,
            "writes are disabled: the daemon was started without VOWP_API_TOKEN",
        )
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// A path-free `500` for every store failure.
fn store_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Db,
        "could not read the project database",
    )
}

/// A path-free `500` for a writable store that cannot be opened.
fn store_write_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Db,
        "could not open the project database for writing",
    )
}

/// A path-free `500` when the daemon was built without session-switch dependencies.
fn switch_deps_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Config,
        "session switching is not configured on this daemon",
    )
}

/// Maps a failed switch onto a documented status without leaking a home path.
///
/// The guards run before the service; these mappings cover a state race and the switch
/// protocol's own abort. A probe or harness detail can contain a binary or home path, so the
/// response keeps a stable, path-free message instead of echoing the internal error.
fn switch_failure(error: CoreError) -> ApiFailure {
    match error {
        CoreError::NotFound(_) => ApiFailure::not_found("session not found"),
        CoreError::Store(StoreError::NotFound { what: "session", .. }) => ApiFailure::not_found("session not found"),
        CoreError::Store(StoreError::Conflict(message)) => ApiFailure::conflict(message),
        CoreError::Invalid(message) => ApiFailure::conflict(message),
        CoreError::Limited(_) => ApiFailure::conflict("the target provider is limited"),
        CoreError::SwitchAborted(_) => {
            ApiFailure::conflict("the switch was aborted; the session stayed on its previous executor")
        }
        other => ApiFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            other.kind(),
            "the session could not be switched",
        ),
    }
}

/// Maps a failed inbox update onto its documented status without leaking a path.
fn answer_store_failure(error: StoreError) -> ApiFailure {
    match error.kind() {
        ErrorKind::NotFound => ApiFailure::not_found("inbox item not found"),
        ErrorKind::Conflict => ApiFailure::conflict("the inbox item is not open"),
        _ => ApiFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Db,
            "could not answer the inbox item",
        ),
    }
}

/// A path-free `500` for a request row that could not be queued.
fn request_write_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Db,
        "the answer was stored but the supervisor request could not be queued",
    )
}

/// A path-free `500` for an unreadable session log.
fn log_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Io,
        "the session log could not be read",
    )
}

/// A path-free `500` for an unreadable or invalid registered project config.
fn config_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Config,
        "the project configuration could not be read",
    )
}

/// A path-free `500` for a board snapshot that could not be collected.
fn board_failure() -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Db,
        "could not read the project board",
    )
}

/// Reduces store rows into the exact `vowp cost --json` document.
///
/// This mirrors `vowp_cli::cmd::cost::build_report`: tokens sum, known USD sums, unknown
/// entries stay separate and the source name derives from the entry counts.
fn build_cost_report(by: &'static str, rows: Vec<StoreCostRow>) -> CostReport {
    let mut total_tokens = vowp_protocol::Tokens::default();
    let mut total_usd = 0.0_f64;
    let mut total_runs = 0_u64;
    let mut receipt = 0_u64;
    let mut price_table = 0_u64;
    let mut unknown = 0_u64;

    let rows = rows
        .into_iter()
        .map(|row| {
            total_tokens.add(&row.tokens);
            total_usd += row.usd;
            total_runs += row.entries;
            receipt += row.receipt_entries;
            price_table += row.price_table_entries;
            unknown += row.unknown_entries;
            ApiCostRow {
                key: row.key,
                runs: row.entries,
                usd: known_usd(row.usd, row.entries, row.unknown_entries),
                tokens: row.tokens,
                source: source_name(row.receipt_entries, row.price_table_entries, row.unknown_entries).to_owned(),
                unknown_entries: row.unknown_entries,
            }
        })
        .collect();

    CostReport {
        by: by.to_owned(),
        rows,
        total: CostTotal {
            runs: total_runs,
            usd: known_usd(total_usd, total_runs, unknown),
            tokens: total_tokens,
            source: source_name(receipt, price_table, unknown).to_owned(),
            unknown_entries: unknown,
        },
    }
}

/// Returns the known USD amount, or `None` when every entry is unknown.
fn known_usd(usd: f64, entries: u64, unknown: u64) -> Option<f64> {
    if entries > 0 && unknown >= entries {
        None
    } else {
        Some(usd)
    }
}

/// Derives the source name from the receipt/price-table/unknown entry counts.
fn source_name(receipt: u64, price_table: u64, unknown: u64) -> &'static str {
    let present = [receipt > 0, price_table > 0, unknown > 0]
        .into_iter()
        .filter(|present| *present)
        .count();
    match (present, receipt > 0, price_table > 0, unknown > 0) {
        (0, _, _, _) => "unknown",
        (1, true, false, false) => "receipt",
        (1, false, true, false) => "price-table",
        (1, false, false, true) => "unknown",
        _ => "mixed",
    }
}

/// Reads `<home>/heartbeat` as Unix seconds, or `None` when absent or unreadable.
///
/// A file containing the legacy `stopped` marker is treated as no heartbeat, matching
/// `vowp status`: a deliberate stop must not look like a live supervisor.
fn read_heartbeat(paths: &NodePaths) -> Option<Timestamp> {
    let text = std::fs::read_to_string(paths.root().join("heartbeat")).ok()?;
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    if lines.any(|line| line.trim() == "stopped") {
        return None;
    }
    first.parse::<i64>().ok().map(Timestamp::from_unix_secs)
}

/// Converts a collection length for the wire.
fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
