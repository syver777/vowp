//! `inbox`: list, show, ask, answer and dismiss human items.
//!
//! Spec: `docs/design/cli.md` §5 and `docs/design/loops.md` §9. Answers are stored on the
//! item and, when a job is attached, an `answer_inbox` request resolves the parked job on the
//! next supervisor tick.

use vowp_core::requests::{self, Request};
use vowp_graph::JobEvent;
use vowp_protocol::{InboxId, InboxItem, InboxKind, InboxState, JobId, ParkedReason};
use vowp_store::{StateChange, Store};

use crate::cli::{InboxAnswerArgs, InboxAskArgs, InboxCommand, InboxDismissArgs, InboxLsArgs, InboxShowArgs};
use crate::output::{Table, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Runs one inbox subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &InboxCommand) -> Result<i32, CliError> {
    match command {
        InboxCommand::Ls(args) => list(ctx, io, args),
        InboxCommand::Show(args) => show(ctx, io, args),
        InboxCommand::Ask(args) => ask(ctx, io, args),
        InboxCommand::Answer(args) => answer(ctx, io, args),
        InboxCommand::Dismiss(args) => dismiss(ctx, io, args),
    }
}

/// Lists open items, or every item with `--all`.
fn list(ctx: &Ctx, io: &mut dyn IoStreams, args: &InboxLsArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let items = store.list_inbox(if args.all { None } else { Some(InboxState::Open) })?;
    if ctx.json() {
        print_json(io, &items)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut table = Table::new(&["ID", "KIND", "JOB", "TITLE", "AGE"]);
    for item in &items {
        table.row(vec![
            item.id.to_string(),
            kind_name(item.kind).to_owned(),
            item.job_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
            item.title.clone(),
            age(now.since(item.created_at).as_secs()),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Shows one item, including its body, options and answer.
fn show(ctx: &Ctx, io: &mut dyn IoStreams, args: &InboxShowArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let item = resolve_item(&store, &args.id)?;
    if ctx.json() {
        print_json(io, &item)?;
        return Ok(0);
    }
    writeln!(io.out(), "id: {}", item.id)?;
    writeln!(io.out(), "kind: {}", kind_name(item.kind))?;
    writeln!(
        io.out(),
        "job: {}",
        item.job_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(io.out(), "title: {}", item.title)?;
    writeln!(io.out(), "state: {}", state_name(item.state))?;
    if !item.body.is_empty() {
        writeln!(io.out(), "\n{}", item.body)?;
    }
    if !item.options.is_empty() {
        writeln!(io.out(), "\noptions: {}", item.options.join(", "))?;
    }
    if let Some(answer) = &item.answer {
        writeln!(
            io.out(),
            "\nanswer: {answer} (by {})",
            item.answered_by.as_deref().unwrap_or("-")
        )?;
    }
    Ok(0)
}

/// Creates an item and parks the attached job for questions and approvals.
fn ask(ctx: &Ctx, io: &mut dyn IoStreams, args: &InboxAskArgs) -> Result<i32, CliError> {
    if args.title.trim().is_empty() {
        return Err(CliError::Invalid("inbox title must not be empty".to_owned()));
    }
    let kind = match args.kind.as_deref().unwrap_or("question") {
        "question" => InboxKind::Question,
        "approval" => InboxKind::Approval,
        "decision" => InboxKind::Decision,
        "notice" => InboxKind::Notice,
        other => {
            return Err(CliError::Invalid(format!(
                "unknown inbox kind: {other} (question, approval, decision or notice)"
            )));
        }
    };
    let store = ctx.store()?;
    let job_id = args.job.as_deref().map(JobId::parse).transpose()?;
    if let Some(job_id) = &job_id {
        store.require_job(job_id)?;
    }
    let now = ctx.clock().now();
    let item = InboxItem {
        id: InboxId::new(),
        project: Some(ctx.project().clone()),
        job_id: job_id.clone(),
        kind,
        title: args.title.clone(),
        body: args.body.clone().unwrap_or_default(),
        options: args.options.clone(),
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now,
        answered_at: None,
    };
    store.insert_inbox(&item)?;
    if let Some(job_id) = &job_id
        && kind != InboxKind::Notice
    {
        park_job(&store, job_id, ParkedReason::Question)?;
    }
    if ctx.json() {
        print_json(io, &item)?;
    } else {
        writeln!(io.out(), "{}", item.id)?;
    }
    Ok(0)
}

/// Stores an answer and queues the job resolution for the next tick.
fn answer(ctx: &Ctx, io: &mut dyn IoStreams, args: &InboxAnswerArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let item = resolve_item(&store, &args.id)?;
    let answer_text = match (&args.text, args.approve, args.reject) {
        (Some(text), false, false) if !text.trim().is_empty() => text.clone(),
        (None, true, false) => {
            if item.kind != InboxKind::Approval {
                return Err(CliError::Invalid(
                    "--approve is only valid for approval items".to_owned(),
                ));
            }
            "approve".to_owned()
        }
        (None, false, true) => {
            if item.kind != InboxKind::Approval {
                return Err(CliError::Invalid(
                    "--reject is only valid for approval items".to_owned(),
                ));
            }
            "reject".to_owned()
        }
        (Some(_), _, _) => {
            return Err(CliError::Invalid(
                "pass either an answer, --approve or --reject, not several".to_owned(),
            ));
        }
        (None, false, false) => {
            return Err(CliError::Invalid(
                "an answer, --approve or --reject is required".to_owned(),
            ));
        }
        (None, true, true) => {
            return Err(CliError::Invalid(
                "--approve and --reject are mutually exclusive".to_owned(),
            ));
        }
    };
    let by = args
        .by
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "cli".to_owned());
    let answered = store.answer_inbox(&item.id, &answer_text, &by)?;
    if answered.job_id.is_some() {
        requests::push(&store, &Request::AnswerInbox { id: item.id.clone() })?;
    }
    if ctx.json() {
        print_json(io, &answered)?;
    } else {
        writeln!(io.out(), "answered inbox {}", answered.id)?;
    }
    Ok(0)
}

/// Dismisses an open item without resolving anything.
fn dismiss(ctx: &Ctx, io: &mut dyn IoStreams, args: &InboxDismissArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let item = resolve_item(&store, &args.id)?;
    if item.state != InboxState::Open {
        return Err(CliError::Conflict(format!("inbox item {} is not open", item.id)));
    }
    store.dismiss_inbox(&item.id, "cli")?;
    let updated = store
        .inbox_item(&item.id)?
        .ok_or_else(|| CliError::NotFound(format!("inbox item {}", item.id)))?;
    if ctx.json() {
        print_json(io, &updated)?;
    } else {
        writeln!(io.out(), "dismissed inbox {}", updated.id)?;
    }
    Ok(0)
}

/// Parks a job with the given reason through the graph transition table.
///
/// `inbox ask` is one of the short CLI mutations that writes directly under WAL; running
/// the same graph transition keeps the state machine authoritative.
fn park_job(store: &Store, job_id: &JobId, reason: ParkedReason) -> Result<(), CliError> {
    let job = store.require_job(job_id)?;
    if job.state == vowp_protocol::JobState::Parked {
        return Ok(());
    }
    let outcome = vowp_graph::transition(&job, &JobEvent::Blocked { reason })?;
    store.set_job_state(
        job_id,
        outcome.to,
        &StateChange {
            note: None,
            parked_reason: Some(reason),
        },
    )?;
    Ok(())
}

/// Resolves an inbox id or unique prefix.
fn resolve_item(store: &Store, input: &str) -> Result<InboxItem, CliError> {
    if let Ok(id) = InboxId::parse(input)
        && let Some(item) = store.inbox_item(&id)?
    {
        return Ok(item);
    }
    if input.len() < 6 {
        return Err(CliError::Invalid(
            "inbox id prefix must be at least 6 characters".to_owned(),
        ));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<InboxItem> = store
        .list_inbox(None)?
        .into_iter()
        .filter(|item| item.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [item] => Ok(item.clone()),
        [] => Err(CliError::NotFound(format!("inbox item {input}"))),
        many => Err(CliError::Invalid(format!(
            "ambiguous inbox prefix {input}: {}",
            many.iter()
                .map(|item| item.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Returns the wire name of an inbox kind.
fn kind_name(kind: InboxKind) -> &'static str {
    match kind {
        InboxKind::Question => "question",
        InboxKind::Approval => "approval",
        InboxKind::Decision => "decision",
        InboxKind::Notice => "notice",
    }
}

/// Returns the wire name of an inbox state.
fn state_name(state: InboxState) -> &'static str {
    match state {
        InboxState::Open => "open",
        InboxState::Answered => "answered",
        InboxState::Dismissed => "dismissed",
    }
}

/// Formats an age in seconds as a compact human duration.
fn age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}
