//! The `vowp chat` fallback surface.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3–§7.4.
//!
//! `vowp chat` is the guaranteed terminal path: it takes the session lease, runs turns
//! through the same `turn-begin` → `__exec` → `ingest` transport as every other surface,
//! and prints a plain, 80-column display without colour or wide tables. The slash
//! commands map to the same core paths as the CLI commands.
//!
//! The executor is this surface's machine by default. A different `--node` is refused
//! rather than silently executing somewhere the owner did not ask for; on a control
//! machine the command is routed to the home by the CLI before it reaches this module.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;
use time::{OffsetDateTime, UtcOffset, macros::format_description};
use vowp_core::session::remote::{BeginTurn, begin_turn};
use vowp_core::session::service::SessionService;
use vowp_core::surface::follow::follow_to;
use vowp_core::surface::lease::STALE_AFTER;
use vowp_core::surface::takeover::notice as takeover_notice;
use vowp_core::surface::{AcquireLease, LeaseGuard, check_lease_with};
use vowp_protocol::{
    CancelToken, CapsuleTrigger, EventBody, Executor, ExecutorRef, NodeName, RunOutcome, RunPurpose, Session,
    SessionId, SessionState, Surface, SurfaceId, SurfaceKind, TurnGrant,
};
use vowp_store::Store;
use vowp_store::session_log::SessionLog;

use crate::node_ctx::NodeRole;
use crate::{CliError, Ctx, IoStreams};

/// Display width for wrapped assistant text and event summaries.
pub const CHAT_WIDTH: usize = 80;

/// Options for one `vowp chat` run.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatOptions {
    /// Session to talk to.
    pub session: SessionId,
    /// Executor to switch to before the first turn.
    pub executor: Option<ExecutorRef>,
    /// Executor node; defaults to this surface's machine.
    pub node: Option<NodeName>,
    /// Follow the log read-only.
    pub follow: bool,
    /// Take the lease from a live surface.
    pub takeover: bool,
}

/// Runs one chat surface.
pub fn run(ctx: &mut Ctx, io: &mut dyn IoStreams, opts: &ChatOptions) -> Result<i32, CliError> {
    run_shared(ctx, io, opts)
}

/// Runs one chat surface through a shared context reference.
///
/// The command dispatcher only holds `&Ctx`; the chat handler itself needs no mutable
/// context state. This entry point keeps [`run`]'s frozen signature for callers that do.
pub fn run_shared(ctx: &Ctx, io: &mut dyn IoStreams, opts: &ChatOptions) -> Result<i32, CliError> {
    if ctx.role() != NodeRole::Home {
        return Err(CliError::Guard(
            "vowp chat needs the home that owns the session".to_owned(),
        ));
    }
    let store = ctx.store()?;
    let mut session = store.require_session(&opts.session)?;
    if !opts.follow {
        ensure_executor_node(ctx, opts.node.as_ref())?;
    }

    if opts.follow {
        return follow_mode(ctx, io, &store, &session, session.last_seq);
    }

    let surface = chat_surface(ctx);
    let (guard, previous) = acquire(ctx, io, &store, &session, &surface, opts.takeover)?;
    let mut guard = Some(guard);
    // The attach note moved the log on, so every display started from here must use the
    // refreshed row rather than the pre-lease snapshot.
    session = store.require_session(&session.id)?;
    match &previous {
        Some(lease) if lease.holder != surface.holder => {
            writeln!(io.out(), "took session {} over from {}", session.id, lease.holder)?;
        }
        Some(_) => {}
        None => writeln!(io.out(), "attached session {} as {}", session.id, surface.holder)?,
    }

    if let Some(target) = &opts.executor {
        ensure_executor_node(ctx, target.node.as_ref().or(opts.node.as_ref()))?;
        let Some(token) = guard.as_ref().map(|held| held.token().to_owned()) else {
            return Ok(0);
        };
        if check_lease_with(&store, &session.id, Some(&token)).is_err() {
            return revoked_follow(ctx, io, &store, &session.id, || release_guard(&mut guard));
        }
        let switched = crate::cmd::session::switch_to(
            ctx,
            &session.id,
            target.executor.clone(),
            target.node.clone().or_else(|| opts.node.clone()),
            vowp_protocol::SwitchReason::User,
            false,
        )?;
        writeln!(io.out(), "switched to {}", switched.to_executor)?;
        session = store.require_session(&session.id)?;
    }

    loop {
        let mut line = String::new();
        let read = io.input().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('/') {
            if !slash_command(ctx, io, &store, &mut session, &mut guard, rest, opts)? {
                break;
            }
            continue;
        }

        let Some(token) = guard.as_ref().map(|held| held.token().to_owned()) else {
            break;
        };
        if check_lease_with(&store, &session.id, Some(&token)).is_err() {
            return revoked_follow(ctx, io, &store, &session.id, || release_guard(&mut guard));
        }
        let status = match guard.as_ref() {
            Some(held) => run_turn(ctx, io, &store, &session, held, line, opts.node.as_ref())?,
            None => break,
        };
        match status {
            TurnStatus::Ok => {}
            TurnStatus::Revoked => {
                return revoked_follow(ctx, io, &store, &session.id, || release_guard(&mut guard));
            }
        }
        session = store.require_session(&session.id)?;
        if session.state == SessionState::Blocked {
            return Err(CliError::Limited(
                "session is blocked by a provider limit; use /switch <executor>".to_owned(),
            ));
        }
        io.out().flush()?;
    }
    Ok(0)
}

/// What one chat turn did.
enum TurnStatus {
    /// The turn finished and the surface still holds the lease.
    Ok,
    /// The home revoked the lease while the turn ran.
    Revoked,
}

/// Returns the executor node and refuses a node that is not this surface.
///
/// The chat process runs the harness on its own machine. Recording a switch to another
/// node and then executing here would silently ignore the owner's `--node`, so the
/// mismatch is rejected before any switch or turn is written.
fn ensure_executor_node(ctx: &Ctx, node: Option<&NodeName>) -> Result<NodeName, CliError> {
    let surface = ctx.config().node.name.clone();
    match node {
        Some(node) if node != &surface => Err(CliError::Invalid(format!(
            "executor node {node} is not this surface ({surface}); run chat on {node} or omit --node"
        ))),
        Some(node) => Ok(node.clone()),
        None => Ok(surface),
    }
}

/// Builds the transient surface row for this terminal.
fn chat_surface(ctx: &Ctx) -> Surface {
    let node = ctx.config().node.name.clone();
    let now = ctx.clock().now();
    Surface {
        id: SurfaceId::new(),
        kind: SurfaceKind::Chat,
        node: node.clone(),
        holder: node.to_string(),
        app: None,
        cwd: Some(ctx.project_root().display().to_string()),
        metadata: json!({"terminal": true}),
        created_at: now,
        last_seen_at: now,
    }
}

/// Acquires the lease, asking the owner before replacing a live holder.
fn acquire(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &Session,
    surface: &Surface,
    takeover: bool,
) -> Result<(LeaseGuard, Option<vowp_protocol::Lease>), CliError> {
    let now = ctx.clock().now();
    let existing = store.lease(&session.id)?;
    let mut take = takeover;
    if let Some(lease) = &existing
        && now.since(lease.heartbeat_at) < STALE_AFTER
        && !take
    {
        let question = format!(
            "Active on {} since {}. Take over? [y/N] ",
            lease.holder,
            clock_time(lease.acquired_at, ctx.tz_offset_minutes())
        );
        if io.is_tty() {
            write!(io.out(), "{question}")?;
            io.out().flush()?;
            let mut answer = String::new();
            io.input().read_line(&mut answer)?;
            take = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
            if !take {
                return Err(CliError::Conflict(format!(
                    "session {} is active on {}",
                    session.id, lease.holder
                )));
            }
        } else {
            return Err(CliError::Conflict(format!(
                "session {} is active on {}; pass --takeover or run with a terminal",
                session.id, lease.holder
            )));
        }
    }
    let guard = LeaseGuard::acquire(
        ctx.deps(),
        store,
        &AcquireLease {
            session: session.id.clone(),
            surface: surface.clone(),
            takeover: take,
        },
    )?;
    Ok((guard, existing))
}

/// Releases the held lease, if any, and stops this surface writing.
///
/// `/follow` and a revoked lease both hand the session to another surface. Releasing the
/// guard deletes the lease row before the follower starts, so the heartbeat thread stops
/// and another surface does not have to take over from a process that is only reading.
fn release_guard(guard: &mut Option<LeaseGuard>) -> Result<(), CliError> {
    if let Some(held) = guard.take() {
        held.release()?;
    }
    Ok(())
}

/// Captures the follow start sequence and then releases the lease.
///
/// The sequence must be read before `release`: an event indexed while the lease is being
/// released belongs to the follower's window and must be printed. Reading the session
/// afterwards would advance `last_seq` past that event and silently skip it.
///
/// `release` is injected so the ordering is testable without a live lease.
pub fn capture_follow_start(
    store: &Store,
    session: &SessionId,
    release: impl FnOnce() -> Result<(), CliError>,
) -> Result<u64, CliError> {
    let start_seq = store.require_session(session)?.last_seq;
    release()?;
    Ok(start_seq)
}

/// Follows a session read-only after any fallback found its lease revoked.
///
/// The stale guard is released before following, but the start sequence must be captured
/// first: an event indexed while the guard is being released belongs to the follower's
/// window and must be printed. Reading `last_seq` afterwards would advance it past that
/// event and silently skip it. Every revoked-lease fallback goes through this function so
/// no path can release first and follow from the post-release row.
///
/// `release` is injected so the ordering is testable without a live lease.
fn revoked_follow(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &SessionId,
    release: impl FnOnce() -> Result<(), CliError>,
) -> Result<i32, CliError> {
    let start_seq = capture_follow_start(store, session, release)?;
    let current = store.require_session(session)?;
    writeln!(
        io.err(),
        "{}",
        read_only_notice(store, &current, ctx.tz_offset_minutes())?
    )?;
    follow_mode(ctx, io, store, &current, start_seq)
}

/// Follows a session read-only after `/switch` found its lease revoked.
///
/// Unlike the other revoked-lease fallbacks this one returns to the slash-command
/// dispatcher instead of the chat loop's exit code, so it discards the follower's 0.
///
/// `release` is injected so the ordering is testable without a live lease.
pub fn revoked_switch_follow(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &SessionId,
    release: impl FnOnce() -> Result<(), CliError>,
) -> Result<(), CliError> {
    revoked_follow(ctx, io, store, session, release)?;
    Ok(())
}

/// Handles one slash command; returns whether the chat loop should continue.
fn slash_command(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &mut Session,
    guard: &mut Option<LeaseGuard>,
    rest: &str,
    opts: &ChatOptions,
) -> Result<bool, CliError> {
    let mut parts = rest.split_whitespace();
    match parts.next().unwrap_or_default() {
        "quit" | "q" => Ok(false),
        "help" | "?" => {
            writeln!(
                io.out(),
                "/switch <executor> [--node n]  /status  /takeover  /follow  /capsule  /quit"
            )?;
            Ok(true)
        }
        "status" => {
            print_status(ctx, io, store, session)?;
            Ok(true)
        }
        "switch" => {
            let Some(token) = guard.as_ref().map(|held| held.token().to_owned()) else {
                return Ok(false);
            };
            if check_lease_with(store, &session.id, Some(&token)).is_err() {
                revoked_switch_follow(ctx, io, store, &session.id, || release_guard(guard))?;
                return Ok(false);
            }
            let Some(target) = parts.next() else {
                writeln!(io.err(), "usage: /switch <executor> [--node n]")?;
                return Ok(true);
            };
            let target = Executor::parse(target)?;
            let node = match (parts.next(), parts.next()) {
                (Some("--node"), Some(node)) => Some(NodeName::parse(node)?),
                (None, None) => opts.node.clone(),
                _ => {
                    writeln!(io.err(), "usage: /switch <executor> [--node n]")?;
                    return Ok(true);
                }
            };
            if let Err(error) = ensure_executor_node(ctx, node.as_ref()) {
                writeln!(io.err(), "vowp: {error}")?;
                return Ok(true);
            }
            match crate::cmd::session::switch_to(
                ctx,
                &session.id,
                target.clone(),
                node,
                vowp_protocol::SwitchReason::User,
                false,
            ) {
                Ok(switched) => {
                    *session = store.require_session(&session.id)?;
                    writeln!(io.out(), "switched to {}", switched.to_executor)?;
                }
                Err(error) => writeln!(io.err(), "vowp: {error}")?,
            }
            Ok(true)
        }
        "takeover" => {
            let before = store.lease(&session.id)?;
            let surface = chat_surface(ctx);
            let replacement = LeaseGuard::acquire(
                ctx.deps(),
                store,
                &AcquireLease {
                    session: session.id.clone(),
                    surface: surface.clone(),
                    takeover: true,
                },
            )?;
            *guard = Some(replacement);
            match before {
                Some(lease) if lease.holder != surface.holder => {
                    writeln!(io.out(), "took session {} over from {}", session.id, lease.holder)?;
                }
                Some(_) => writeln!(io.out(), "refreshed the lease on session {}", session.id)?,
                None => writeln!(io.out(), "attached session {} as {}", session.id, surface.holder)?,
            }
            *session = store.require_session(&session.id)?;
            Ok(true)
        }
        "follow" => {
            let start_seq = capture_follow_start(store, &session.id, || release_guard(guard))?;
            writeln!(
                io.out(),
                "following session {} read-only; press Ctrl-C to stop",
                session.id
            )?;
            let current = store.require_session(&session.id)?;
            *session = current.clone();
            follow_mode(ctx, io, store, &current, start_seq).map(|_| false)
        }
        "capsule" => {
            let Some(token) = guard.as_ref().map(|held| held.token().to_owned()) else {
                return Ok(false);
            };
            check_lease_with(store, &session.id, Some(&token))?;
            let service = SessionService::new(ctx.deps().clone());
            let capsule = service.write_capsule(&session.id, CapsuleTrigger::Manual)?;
            writeln!(io.out(), "{}", capsule.capsule_id)?;
            *session = store.require_session(&session.id)?;
            Ok(true)
        }
        other => {
            writeln!(io.err(), "unknown command: /{other}")?;
            Ok(true)
        }
    }
}

/// Prints one short status block, no table and no colour.
fn print_status(ctx: &Ctx, io: &mut dyn IoStreams, store: &Store, session: &Session) -> Result<(), CliError> {
    write_wrapped(io.out(), &format!("session: {} ({})", session.id, session.title))?;
    write_wrapped(
        io.out(),
        &format!(
            "state: {}  executor: {}  node: {}",
            state_name(session.state),
            session
                .executor
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
            session.node
        ),
    )?;
    write_wrapped(
        io.out(),
        &format!(
            "events: {}  capsule: {}",
            session.last_seq,
            session
                .last_capsule_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned())
        ),
    )?;
    match store.lease(&session.id)? {
        Some(lease) => write_wrapped(
            io.out(),
            &format!(
                "lease: {} ({}) on {} since {}",
                lease.holder,
                surface_kind_name(lease.surface_kind),
                lease.node,
                clock_time(lease.acquired_at, ctx.tz_offset_minutes())
            ),
        )?,
        None => write_wrapped(io.out(), "lease: free")?,
    }
    Ok(())
}

/// Runs one turn through the same transport every other surface uses.
fn run_turn(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &Session,
    guard: &LeaseGuard,
    prompt: &str,
    node: Option<&NodeName>,
) -> Result<TurnStatus, CliError> {
    let surface_node = ctx.config().node.name.clone();
    let executor_node = ensure_executor_node(ctx, node)?;
    let executor = session
        .executor
        .clone()
        .or_else(|| {
            ctx.config()
                .profiles
                .get("worker")
                .and_then(|profile| profile.ladder.first().cloned())
        })
        .ok_or_else(|| CliError::Invalid("session has no executor; use /switch".to_owned()))?;
    let purpose = if first_turn(ctx, session)? {
        RunPurpose::Start
    } else {
        RunPurpose::Continue
    };
    let cwd = if Path::new(&session.cwd).is_dir() {
        session.cwd.clone()
    } else {
        ctx.project_root().display().to_string()
    };
    let grant = begin_turn(
        ctx.deps(),
        store,
        &BeginTurn {
            session: session.id.clone(),
            lease: guard.token().to_owned(),
            executor: ExecutorRef {
                executor: executor.clone(),
                node: Some(executor_node.clone()),
            },
            purpose,
            node: executor_node,
            cwd: Some(cwd),
            prompt: prompt.to_owned(),
        },
    )?;
    let grant_path = write_grant(ctx, &grant)?;
    let spool = ctx
        .paths()
        .root()
        .join("spool")
        .join(grant.session_id.as_str())
        .join(grant.run_id.as_str());
    let output = spawn_exec(ctx, &grant_path, &spool);
    let _ = std::fs::remove_file(&grant_path);
    let output = output?;
    let code = output.status.code().unwrap_or(-1);
    if code == 75 {
        return Ok(TurnStatus::Revoked);
    }
    if !output.status.success() {
        if !output.stderr.is_empty() {
            io.err().write_all(&output.stderr)?;
        }
        return Err(CliError::Remote(format!(
            "executor on {surface_node} failed with exit {code}"
        )));
    }
    print_new_events(ctx, io, store, session, grant.tail_seq)?;
    let run = store.require_run(&grant.run_id)?;
    if matches!(
        run.outcome,
        Some(RunOutcome::Limit | RunOutcome::Credits | RunOutcome::Auth | RunOutcome::Rate)
    ) {
        return Err(CliError::Limited(
            "provider limit reached; use /switch <executor>".to_owned(),
        ));
    }
    Ok(TurnStatus::Ok)
}

/// Returns whether this session has not started a turn yet.
fn first_turn(ctx: &Ctx, session: &Session) -> Result<bool, CliError> {
    let Ok(log) = SessionLog::open_read(ctx.paths(), &session.id) else {
        return Ok(session.last_seq <= 1);
    };
    let events = log.read_all().map_err(vowp_core::CoreError::from)?;
    Ok(!events
        .iter()
        .any(|event| event.event_type == vowp_protocol::EventType::TurnStarted))
}

/// Writes the grant document as a private file for `__exec`.
fn write_grant(ctx: &Ctx, grant: &TurnGrant) -> Result<PathBuf, CliError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let dir = ctx
        .paths()
        .root()
        .join("spool")
        .join(grant.session_id.as_str())
        .join(grant.run_id.as_str());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("grant.json");
    let text = serde_json::to_vec(grant).map_err(vowp_protocol::ProtocolError::from)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&text)?;
    file.sync_all()?;
    Ok(path)
}

/// Spawns this binary's hidden `__exec` command on the surface machine.
fn spawn_exec(ctx: &Ctx, grant: &Path, spool: &Path) -> Result<std::process::Output, CliError> {
    let binary = match std::env::var_os("VOWP_EXEC_BIN") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?,
    };
    Command::new(binary)
        .arg("--home")
        .arg(ctx.paths().root())
        .arg("--project")
        .arg(ctx.project().to_string())
        .arg("--no-color")
        .arg("__exec")
        .arg("--grant")
        .arg(grant)
        .arg("--spool-dir")
        .arg(spool)
        // The executor is this surface's machine even when this account also has a control
        // machine configured; the child must ingest into the same home, not dial itself.
        .env(
            "VOWP_CONTROL_HOME",
            ctx.paths().root().join("spool").join(".local-surface"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| CliError::Remote(format!("cannot start the executor: {error}")))
}

/// Prints the events appended by the turn, wrapped for a narrow terminal.
fn print_new_events(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &Session,
    after_seq: u64,
) -> Result<(), CliError> {
    let Some(log) = SessionLog::open_read(ctx.paths(), &session.id).ok() else {
        let cancel = CancelToken::default();
        cancel.cancel();
        follow_to(store, &session.id, after_seq, &cancel, io.out())?;
        return Ok(());
    };
    let events = log.read_from(after_seq, None).map_err(vowp_core::CoreError::from)?;
    for event in events {
        match event.body() {
            Ok(EventBody::AssistantMessage(payload)) => write_wrapped(io.out(), &payload.text)?,
            Ok(EventBody::ToolCall(payload)) => write_wrapped(io.out(), &format!("· tool: {}", payload.name))?,
            Ok(EventBody::Error(payload)) => write_wrapped(io.out(), &format!("! {}", payload.message))?,
            Ok(EventBody::Note(payload)) => write_wrapped(io.out(), &format!("— {}", payload.text))?,
            Ok(_) => {}
            Err(_) => write_wrapped(io.out(), &format!("#{} {}", event.seq, event.event_type.as_str()))?,
        }
    }
    Ok(())
}

/// Follows new events read-only until cancellation; non-TTY runs are scriptable.
fn follow_mode(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    store: &Store,
    session: &Session,
    after_seq: u64,
) -> Result<i32, CliError> {
    if io.is_tty() {
        let watcher = crate::signals::SignalWatcher::install(ctx.paths())?;
        let cancel = watcher.cancel_token();
        let _ = follow_to(store, &session.id, after_seq, &cancel, io.out())?;
        return Ok(0);
    }

    let cancel = CancelToken::default();
    cancel.cancel();
    let mut last = after_seq;
    loop {
        // Scripted follow drains the index at every command boundary: a pipe can write an
        // event and then `/quit`, and the follower still prints that event before exiting.
        last = poll_follow(store, &session.id, last, io)?;
        let mut line = String::new();
        let read = io.input().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        match line.trim() {
            "" => {}
            "/quit" | "/q" => {
                let _ = poll_follow(store, &session.id, last, io)?;
                return Ok(0);
            }
            "/status" => {
                let current = store.require_session(&session.id)?;
                print_status(ctx, io, store, &current)?;
            }
            "/follow" => {}
            _ => writeln!(
                io.err(),
                "{}",
                read_only_notice(store, session, ctx.tz_offset_minutes())?
            )?,
        }
    }
    let _ = poll_follow(store, &session.id, last, io)?;
    Ok(0)
}

/// Drains the events already indexed after `after_seq` and returns the last sequence read.
fn poll_follow(store: &Store, session: &SessionId, after_seq: u64, io: &mut dyn IoStreams) -> Result<u64, CliError> {
    let cancel = CancelToken::default();
    cancel.cancel();
    Ok(follow_to(store, session, after_seq, &cancel, io.out())?)
}

/// Returns the exact read-only notice for the current lease holder.
fn read_only_notice(store: &Store, session: &Session, tz_offset_minutes: i32) -> Result<String, CliError> {
    match store.lease(&session.id)? {
        Some(lease) => Ok(takeover_notice(&lease.holder, lease.acquired_at, tz_offset_minutes)),
        None => Ok("This surface is read-only (follow mode).".to_owned()),
    }
}

/// Writes text wrapped at [`CHAT_WIDTH`] columns.
fn write_wrapped(out: &mut dyn std::io::Write, text: &str) -> Result<(), CliError> {
    for line in text.lines() {
        if line.chars().count() <= CHAT_WIDTH {
            writeln!(out, "{line}")?;
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let mut word = word.to_owned();
            loop {
                let extra = usize::from(!current.is_empty());
                if current.chars().count() + extra + word.chars().count() <= CHAT_WIDTH {
                    if extra == 1 {
                        current.push(' ');
                    }
                    current.push_str(&word);
                    break;
                }
                if !current.is_empty() {
                    writeln!(out, "{current}")?;
                    current.clear();
                    continue;
                }
                let split = word
                    .char_indices()
                    .nth(CHAT_WIDTH)
                    .map(|(index, _)| index)
                    .unwrap_or(word.len());
                let rest = word.split_off(split);
                writeln!(out, "{word}")?;
                word = rest;
                if word.is_empty() {
                    break;
                }
            }
        }
        if !current.is_empty() {
            writeln!(out, "{current}")?;
        }
    }
    if text.is_empty() {
        writeln!(out)?;
    }
    Ok(())
}

/// Formats a timestamp as `HH:MM` in a fixed offset.
fn clock_time(ts: vowp_protocol::Timestamp, tz_offset_minutes: i32) -> String {
    let Some(utc) = OffsetDateTime::from_unix_timestamp(ts.unix_secs()).ok() else {
        return ts.as_rfc3339();
    };
    let offset = UtcOffset::from_whole_seconds(tz_offset_minutes.saturating_mul(60)).unwrap_or(UtcOffset::UTC);
    utc.to_offset(offset)
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| ts.as_rfc3339())
}

/// Returns the wire name of one session state.
fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Running => "running",
        SessionState::Blocked => "blocked",
        SessionState::Switching => "switching",
        SessionState::Closed => "closed",
    }
}

/// Returns the short name of one surface kind.
fn surface_kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Chat => "chat",
        SurfaceKind::Opencode => "opencode",
        SurfaceKind::ClaudeCode => "claude-code",
        SurfaceKind::Mcp => "mcp",
        SurfaceKind::Mirror => "mirror",
        SurfaceKind::Api => "api",
    }
}
