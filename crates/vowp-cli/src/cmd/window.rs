//! Hidden `vowp __window`: what each tmux window runs.
//!
//! Spec: `docs/design/cli.md` §6 and `docs/design/crates/vowp-tmux.md` §2. Every window is
//! read-only: it renders the board, tails the supervisor log or a session log, or lists the
//! inbox. The same session tailer is the fallback for `vowp attach`.

use std::path::Path;
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset, macros::format_description};
use vowp_protocol::{EventEnvelope, InboxState, JobId, Session, SessionId, SessionRole};
use vowp_store::SessionFilter;
use vowp_store::session_log::SessionLog;

use crate::cli::WindowArgs;
use crate::output::Table;
use crate::{CliError, Ctx, IoStreams};

/// How often a follower checks for new lines.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Lines of supervisor log shown before following.
const SUPERVISOR_TAIL_LINES: usize = 200;

/// Runs `vowp __window`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &WindowArgs) -> Result<i32, CliError> {
    match args.kind.as_str() {
        "board" => {
            // On a TTY the window redraws; without one a single frame is printed so
            // scripts and tests terminate.
            if io.is_tty() {
                loop {
                    write!(io.out(), "\x1b[2J\x1b[H")?;
                    crate::cmd::board::render(ctx, io)?;
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
            crate::cmd::board::render(ctx, io)?;
        }
        "supervisor" => {
            let path = ctx.paths().logs_dir().join("supervisor.log");
            tail_file(io, &path, SUPERVISOR_TAIL_LINES)?;
        }
        "planner" => tail_role(ctx, io, SessionRole::Planner)?,
        "keeper" => tail_role(ctx, io, SessionRole::Keeper)?,
        "inbox" => list_inbox(ctx, io)?,
        "job" => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| CliError::Invalid("__window job requires a job id".to_owned()))?;
            let job = JobId::parse(id)?;
            let session = latest_job_session(ctx, &job)?
                .ok_or_else(|| CliError::NotFound(format!("job {job} has no session")))?;
            tail_session(ctx, io, &session.id, true, true, 0)?;
        }
        other => {
            return Err(CliError::Invalid(format!(
                "unknown window {other:?}; expected board, supervisor, planner, keeper, inbox or job"
            )));
        }
    }
    Ok(0)
}

/// Tails the newest session of a planner or keeper role.
fn tail_role(ctx: &Ctx, io: &mut dyn IoStreams, role: SessionRole) -> Result<(), CliError> {
    let store = ctx.store()?;
    let sessions = store.list_sessions(&SessionFilter {
        project: Some(ctx.project().clone()),
        ..SessionFilter::default()
    })?;
    match sessions.into_iter().find(|session| session.role == Some(role)) {
        Some(session) => tail_session(ctx, io, &session.id, true, true, 0),
        None => {
            let name = match role {
                SessionRole::Planner => "planner",
                SessionRole::Keeper => "keeper",
                _ => "role",
            };
            writeln!(io.out(), "no {name} session yet")?;
            Ok(())
        }
    }
}

/// Lists open inbox items as `ID KIND JOB TITLE AGE`.
fn list_inbox(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<(), CliError> {
    let store = ctx.store()?;
    let now = ctx.clock().now();
    let items = store.list_inbox(Some(InboxState::Open))?;
    if ctx.json() {
        crate::output::print_json(io, &items)?;
        return Ok(());
    }
    let mut table = Table::new(&["ID", "KIND", "JOB", "TITLE", "AGE"]);
    for item in items {
        table.row(vec![
            item.id.to_string(),
            inbox_kind_name(item.kind).to_owned(),
            item.job_id.map(|job| job.to_string()).unwrap_or_else(|| "-".to_owned()),
            item.title,
            format_duration(now.since(item.created_at).as_secs()),
        ]);
    }
    write!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(())
}

/// Returns the newest session created for `job`.
pub(crate) fn latest_job_session(ctx: &Ctx, job: &JobId) -> Result<Option<Session>, CliError> {
    let store = ctx.store()?;
    Ok(store
        .list_sessions(&SessionFilter {
            job_id: Some(job.clone()),
            limit: Some(1),
            ..SessionFilter::default()
        })?
        .into_iter()
        .next())
}

/// Tails a job's latest session, for `attach` when tmux is not usable.
pub(crate) fn tail_latest_job(ctx: &Ctx, io: &mut dyn IoStreams, job: &JobId) -> Result<i32, CliError> {
    let session =
        latest_job_session(ctx, job)?.ok_or_else(|| CliError::NotFound(format!("job {job} has no session")))?;
    tail_session(ctx, io, &session.id, true, true, 0)?;
    Ok(0)
}

/// Prints a session log, optionally following it until the terminal goes away.
pub(crate) fn tail_session(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    session: &SessionId,
    follow: bool,
    pretty: bool,
    from_seq: u64,
) -> Result<(), CliError> {
    let reader = SessionLog::open_read(ctx.paths(), session)?;
    let mut last = from_seq;
    let follow = follow && io.is_tty();
    loop {
        for event in reader.read_from(last, None)? {
            last = event.seq;
            print_event(ctx, io, &event, pretty)?;
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Prints one event as JSONL or as the pretty one-line form of `session tail --pretty`.
fn print_event(ctx: &Ctx, io: &mut dyn IoStreams, event: &EventEnvelope, pretty: bool) -> Result<(), CliError> {
    if !pretty {
        let text = serde_json::to_string(event).map_err(vowp_protocol::ProtocolError::from)?;
        writeln!(io.out(), "{text}")?;
        return Ok(());
    }
    let summary = match event.body() {
        Ok(body) => body.summary(),
        Err(_) => event.event_type.as_str().to_owned(),
    };
    let type_prefix = format!("{} ", event.event_type.as_str());
    let summary = summary.strip_prefix(&type_prefix).unwrap_or(&summary);
    writeln!(
        io.out(),
        "{} #{} {}  {}",
        clock_time(event.ts, ctx.tz_offset_minutes()),
        event.seq,
        event.event_type.as_str(),
        summary
    )?;
    Ok(())
}

/// Formats an event timestamp as `HH:MM:SS` in the node's fixed offset.
fn clock_time(ts: vowp_protocol::Timestamp, tz_offset_minutes: i32) -> String {
    let offset = UtcOffset::from_whole_seconds(tz_offset_minutes.saturating_mul(60)).unwrap_or(UtcOffset::UTC);
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(ts.unix_secs()) else {
        return ts.as_rfc3339();
    };
    datetime
        .to_offset(offset)
        .format(format_description!("[hour]:[minute]:[second]"))
        .unwrap_or_default()
}

/// Follows a growing text file, printing its last `initial_lines` lines first.
fn tail_file(io: &mut dyn IoStreams, path: &Path, initial_lines: usize) -> Result<(), CliError> {
    let follow = io.is_tty();
    let mut printed = 0_usize;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let lines: Vec<&str> = text.lines().collect();
            let start = if printed == 0 {
                lines.len().saturating_sub(initial_lines)
            } else {
                printed
            };
            for line in lines.iter().skip(start) {
                writeln!(io.out(), "{line}")?;
            }
            printed = lines.len();
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Returns the lowercase name of an inbox kind.
fn inbox_kind_name(kind: vowp_protocol::InboxKind) -> &'static str {
    match kind {
        vowp_protocol::InboxKind::Question => "question",
        vowp_protocol::InboxKind::Approval => "approval",
        vowp_protocol::InboxKind::Decision => "decision",
        vowp_protocol::InboxKind::Notice => "notice",
    }
}

/// Formats seconds as `1h 02m`, `3m 04s` or `12s`.
fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
