//! `vowp plan`: one planner turn that turns a goal into jobs.
//!
//! Spec: `docs/design/cli.md` §3. The planner runs on the first available rung of a
//! profile (default `planner`), streams one line per event, and finally prints the jobs it
//! created by diffing the graph. `--dry-run` asks for YAML and stores no jobs.

use std::collections::BTreeSet;
use std::thread;
use std::time::Duration;

use serde::Serialize;
use vowp_core::session::service::SessionService;
use vowp_protocol::{
    CancelToken, Executor, ExecutorRef, NewSession, RunPurpose, RunRole, SessionId, SessionRole, TurnRequest,
};
use vowp_store::JobFilter;
use vowp_store::session_log::{SessionLog, SessionLogReader};

use crate::cli::PlanArgs;
use crate::{CliError, Ctx, IoStreams};

/// How often the command checks for new planner events.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// What `plan --json` prints.
#[derive(Debug, Serialize)]
struct PlanOutput {
    session: String,
    executor: String,
    jobs: Vec<String>,
    result: Option<String>,
}

/// Runs `vowp plan`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &PlanArgs) -> Result<i32, CliError> {
    let profile_name = args.profile.clone().unwrap_or_else(|| "planner".to_owned());
    let (rung, executor) = select_executor(ctx, &profile_name)?;
    let digest = graph_digest(ctx)?;
    let prompt = planner_prompt(ctx, &args.goal, args.dry_run, &digest);
    let before = job_ids(ctx)?;

    let service = SessionService::new(ctx.deps().clone());
    let session = service.create_session(NewSession {
        project: Some(ctx.project().clone()),
        job_id: None,
        role: SessionRole::Planner,
        title: format!("plan: {}", args.goal),
        cwd: ctx.project_root().to_path_buf(),
        parent: None,
        executor: Some(executor.clone()),
    })?;

    let request = TurnRequest {
        session_id: session.clone(),
        executor: ExecutorRef {
            executor: executor.clone(),
            node: None,
        },
        purpose: RunPurpose::Plan,
        role: RunRole::Planner,
        job_id: None,
        prompt,
        profile: Some(profile_name),
        rung,
        window: Some("planner".to_owned()),
        timeout: None,
    };
    let worker_deps = ctx.deps().clone();
    let cancel = CancelToken::default();
    let handle = thread::spawn(move || SessionService::new(worker_deps).run_turn(request, &cancel));

    stream_events(ctx, io, &session, &handle)?;
    let result = handle
        .join()
        .map_err(|_| CliError::Invalid("planner run thread panicked".to_owned()))??;

    if args.dry_run {
        let text = result.summary.result_text.unwrap_or_default();
        if ctx.json() {
            crate::output::print_json(
                io,
                &PlanOutput {
                    session: session.to_string(),
                    executor: executor.to_string(),
                    jobs: Vec::new(),
                    result: Some(text),
                },
            )?;
        } else {
            writeln!(io.out(), "{text}")?;
        }
        return Ok(0);
    }

    let after = job_ids(ctx)?;
    let created: Vec<String> = after.difference(&before).cloned().collect();
    if ctx.json() {
        crate::output::print_json(
            io,
            &PlanOutput {
                session: session.to_string(),
                executor: executor.to_string(),
                jobs: created,
                result: result.summary.result_text,
            },
        )?;
    } else if created.is_empty() {
        writeln!(io.out(), "planner created no jobs")?;
    } else {
        writeln!(io.out(), "created {} job(s): {}", created.len(), created.join(", "))?;
    }
    Ok(0)
}

/// Picks the first rung whose provider is not limited and whose adapter exists.
fn select_executor(ctx: &Ctx, profile_name: &str) -> Result<(u32, Executor), CliError> {
    let profile = ctx
        .config()
        .profiles
        .get(profile_name)
        .ok_or_else(|| CliError::NotFound(format!("profile {profile_name}")))?;
    let store = ctx.store()?;
    let limited: BTreeSet<String> = store
        .active_limits(ctx.clock().now())?
        .into_iter()
        .map(|limit| limit.provider)
        .collect();

    for (rung, executor) in profile.ladder.iter().enumerate() {
        if limited.contains(&executor.provider()) {
            continue;
        }
        if ctx.deps().adapters.get(executor).is_none() {
            continue;
        }
        return Ok((u32::try_from(rung).unwrap_or(u32::MAX), executor.clone()));
    }

    if profile
        .ladder
        .iter()
        .all(|executor| limited.contains(&executor.provider()))
    {
        return Err(CliError::Limited(format!(
            "every rung of profile {profile_name} is limited"
        )));
    }
    Err(CliError::Invalid(format!(
        "profile {profile_name} has no available executor"
    )))
}

/// Builds the prompt with the goal, framework rules, graph digest and `job add` syntax.
fn planner_prompt(ctx: &Ctx, goal: &str, dry_run: bool, digest: &str) -> String {
    let rules = crate::framework::FILES
        .iter()
        .find(|file| file.target == ".claude/skills/vowp-planner/SKILL.md")
        .map(|file| file.content)
        .unwrap_or("Plan the work as a small DAG of jobs with disjoint owned paths.");
    let mode = if dry_run {
        "Do NOT run any `vowp` command and do not write files. Output the graph as YAML only."
    } else {
        "Create the jobs with `vowp job add`; write one brief per job. Do not start any job."
    };
    format!(
        "You are the VOWP planner for project {project}.\n\n\
         Goal:\n{goal}\n\n\
         {mode}\n\n\
         Planner rules:\n{rules}\n\n\
         Current graph:\n{digest}\n\n\
         Exact job-add syntax:\n\
         vowp job add --id <slug> --title <text> --kind <build|verify|plan|...> [--role <r>] \
         [--profile <p>] [--needs a,b] [--owns glob,...] [--reads glob,...] \
         [--gates <set|short,...>] [--priority n] [--max-attempts n] \
         [--brief-file <path>|--brief -]\n",
        project = ctx.project()
    )
}

/// Returns the current job ids for the graph diff.
fn job_ids(ctx: &Ctx) -> Result<BTreeSet<String>, CliError> {
    let store = ctx.store()?;
    Ok(store
        .list_jobs(&JobFilter {
            project: Some(ctx.project().clone()),
            ..JobFilter::default()
        })?
        .into_iter()
        .map(|job| job.id.to_string())
        .collect())
}

/// Builds a compact one-line-per-job digest of the graph.
fn graph_digest(ctx: &Ctx) -> Result<String, CliError> {
    let store = ctx.store()?;
    let jobs = store.list_jobs(&JobFilter {
        project: Some(ctx.project().clone()),
        ..JobFilter::default()
    })?;
    if jobs.is_empty() {
        return Ok("no jobs yet".to_owned());
    }
    Ok(jobs
        .iter()
        .map(|job| format!("{} [{}] {}", job.id, job.state.as_str(), job.title))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Prints every event the planner writes while its turn is running.
fn stream_events(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    session: &SessionId,
    handle: &thread::JoinHandle<Result<vowp_protocol::TurnResult, vowp_core::CoreError>>,
) -> Result<(), CliError> {
    let reader = SessionLog::open_read(ctx.paths(), session)?;
    // Start at the beginning: `session.created` is harmless and this avoids losing
    // turn events when the planner finishes before the first poll.
    let mut last = 0;
    while !handle.is_finished() {
        last = print_new_events(ctx, io, &reader, last)?;
        thread::sleep(POLL_INTERVAL);
    }
    print_new_events(ctx, io, &reader, last)?;
    Ok(())
}

/// Prints events with `seq > last` in the pretty form and returns the new last seq.
fn print_new_events(ctx: &Ctx, io: &mut dyn IoStreams, reader: &SessionLogReader, last: u64) -> Result<u64, CliError> {
    let mut newest = last;
    for event in reader.read_from(last, None)? {
        newest = event.seq;
        if ctx.json() {
            continue;
        }
        let summary = match event.body() {
            Ok(body) => body.summary(),
            Err(_) => event.event_type.as_str().to_owned(),
        };
        writeln!(io.out(), "#{} {}  {}", event.seq, event.event_type.as_str(), summary)?;
    }
    Ok(newest)
}
