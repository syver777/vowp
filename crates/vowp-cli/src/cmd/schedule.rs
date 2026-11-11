//! `schedule`: add, list, pause, resume and remove L9 schedules.
//!
//! Spec: `docs/design/cli.md` §5 and `docs/design/loops.md` §11. A schedule is a store row;
//! the engine validates the same cron, interval and timezone rules when it runs.

use vowp_core::schedules::ScheduleSpec;
use vowp_protocol::{ActiveState, MissedPolicy, NewJob, Schedule, ScheduleAction, ScheduleId, ScheduleSpecKind};
use vowp_store::Store;

use crate::cli::{ScheduleActionArg, ScheduleAddArgs, ScheduleArgs, ScheduleCommand, ScheduleNameArgs};
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Default timeout for a scheduled command, matching the protocol's serde default.
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 1_800;

/// Runs one schedule subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, request: &ScheduleArgs) -> Result<i32, CliError> {
    if request.all && !matches!(&request.command, ScheduleCommand::Ls) {
        return Err(CliError::Invalid(
            "--all lists every project; change one project's schedules at a time".to_owned(),
        ));
    }
    match &request.command {
        ScheduleCommand::Add(args) => add(ctx, io, args),
        ScheduleCommand::Ls => list(ctx, io, request.all),
        ScheduleCommand::Pause(args) => set_state(ctx, io, args, ActiveState::Paused),
        ScheduleCommand::Resume(args) => set_state(ctx, io, args, ActiveState::Active),
        ScheduleCommand::Rm(args) => remove(ctx, io, args),
    }
}

/// Adds one schedule after validating its spec and action.
fn add(ctx: &Ctx, io: &mut dyn IoStreams, args: &ScheduleAddArgs) -> Result<i32, CliError> {
    let (common, action) = match &args.action {
        ScheduleActionArg::Loop(loop_) => {
            let store = crate::node_ctx::selected_store(ctx)?;
            if store.loop_(&loop_.loop_name)?.is_none() {
                return Err(CliError::NotFound(format!("loop {}", loop_.loop_name)));
            }
            (
                &loop_.common,
                ScheduleAction::Loop {
                    loop_name: loop_.loop_name.clone(),
                },
            )
        }
        ScheduleActionArg::Command(command) => (
            &command.common,
            ScheduleAction::Command {
                argv: command.argv.clone(),
                cwd: None,
                timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
            },
        ),
        ScheduleActionArg::JobTemplate(template) => {
            if template.common.name.trim().is_empty() {
                return Err(CliError::Invalid("schedule name must not be empty".to_owned()));
            }
            let text = std::fs::read_to_string(&template.template_file)?;
            let template_job: NewJob = serde_json::from_str(&text).map_err(|error| {
                CliError::Invalid(format!(
                    "template file {} is not a NewJob: {error}",
                    template.template_file.display()
                ))
            })?;
            if template_job.title.trim().is_empty() {
                return Err(CliError::Invalid("job template title must not be empty".to_owned()));
            }
            (&template.common, ScheduleAction::JobTemplate { template: template_job })
        }
    };
    if common.name.trim().is_empty() {
        return Err(CliError::Invalid("schedule name must not be empty".to_owned()));
    }
    if matches!(&action, ScheduleAction::Command { argv, .. } if argv.is_empty()) {
        return Err(CliError::Invalid(
            "command schedule needs at least one argument".to_owned(),
        ));
    }
    let project = crate::node_ctx::selected_project(ctx)?;
    let store = crate::node_ctx::selected_store(ctx)?;
    if store.schedule(&common.name)?.is_some() {
        return Err(CliError::Conflict(format!("schedule {} already exists", common.name)));
    }
    let tz = common.tz.clone().unwrap_or_else(|| "UTC".to_owned());
    let missed_policy = parse_missed(common.missed.as_deref())?;
    let kind = spec_kind(&args.spec);
    let parsed = ScheduleSpec::parse(kind, &args.spec, &tz)?;
    let now = ctx.clock().now();
    let next_run_at = parsed.next_after(now);
    let schedule = Schedule {
        id: ScheduleId::new(),
        project: Some(project),
        name: common.name.clone(),
        spec_kind: kind,
        spec: args.spec.clone(),
        tz,
        action,
        missed_policy,
        state: ActiveState::Active,
        last_run_at: None,
        next_run_at,
        last_status: String::new(),
        created_at: now,
    };
    store.insert_schedule(&schedule)?;
    if ctx.json() {
        print_json(io, &schedule)?;
    } else {
        writeln!(
            io.out(),
            "added schedule {} (next {})",
            schedule.name,
            schedule
                .next_run_at
                .map(|ts| ts.as_rfc3339())
                .unwrap_or_else(|| "never".to_owned())
        )?;
    }
    Ok(0)
}

/// Lists schedules, from every registered project when `all` is set.
fn list(ctx: &Ctx, io: &mut dyn IoStreams, all: bool) -> Result<i32, CliError> {
    let schedules = if all {
        all_schedules(ctx)?
    } else {
        crate::node_ctx::selected_store(ctx)?.list_schedules()?
    };
    if ctx.json() {
        print_json(io, &schedules)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut headers = Vec::new();
    if all {
        headers.push("PROJECT");
    }
    headers.extend(["NAME", "SPEC", "ACTION", "STATE", "LAST", "NEXT", "STATUS"]);
    let mut table = Table::new(&headers);
    for schedule in &schedules {
        let mut row = Vec::new();
        if all {
            row.push(
                schedule
                    .project
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".to_owned()),
            );
        }
        row.extend([
            schedule.name.clone(),
            schedule.spec.clone(),
            action_name(&schedule.action),
            active_state(schedule.state).to_owned(),
            schedule
                .last_run_at
                .map(|ts| local_time_at(ts, now, ctx.tz_offset_minutes()))
                .unwrap_or_else(|| "-".to_owned()),
            schedule
                .next_run_at
                .map(|ts| local_time_at(ts, now, ctx.tz_offset_minutes()))
                .unwrap_or_else(|| "-".to_owned()),
            if schedule.last_status.is_empty() {
                "-".to_owned()
            } else {
                schedule.last_status.clone()
            },
        ]);
        table.row(row);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Collects schedules from every registered project, sorted by project then name.
fn all_schedules(ctx: &Ctx) -> Result<Vec<Schedule>, CliError> {
    let global = Store::open_global(ctx.paths())?;
    let mut schedules = Vec::new();
    for entry in vowp_core::loops::registry::project_entries(&global)? {
        if entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        let store = match Store::open_with_project(ctx.paths(), &entry.name) {
            Ok(store) => store,
            Err(vowp_store::StoreError::NotFound { .. }) => continue,
            Err(error) => return Err(error.into()),
        };
        schedules.extend(store.list_schedules()?);
    }
    schedules.sort_by(|left, right| {
        left.project
            .as_ref()
            .map(ToString::to_string)
            .cmp(&right.project.as_ref().map(ToString::to_string))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(schedules)
}

/// Pauses or resumes one schedule.
fn set_state(ctx: &Ctx, io: &mut dyn IoStreams, args: &ScheduleNameArgs, state: ActiveState) -> Result<i32, CliError> {
    let store = crate::node_ctx::selected_store(ctx)?;
    let schedule = store
        .schedule(&args.name)?
        .ok_or_else(|| CliError::NotFound(format!("schedule {}", args.name)))?;
    store.set_schedule_state(&schedule.id, state)?;
    let updated = store
        .schedule(schedule.id.as_str())?
        .ok_or_else(|| CliError::NotFound(format!("schedule {}", schedule.id)))?;
    if ctx.json() {
        print_json(io, &updated)?;
    } else {
        let verb = if state == ActiveState::Paused {
            "paused"
        } else {
            "resumed"
        };
        writeln!(io.out(), "{verb} schedule {}", updated.name)?;
    }
    Ok(0)
}

/// Removes one schedule.
fn remove(ctx: &Ctx, io: &mut dyn IoStreams, args: &ScheduleNameArgs) -> Result<i32, CliError> {
    let store = crate::node_ctx::selected_store(ctx)?;
    let schedule = store
        .schedule(&args.name)?
        .ok_or_else(|| CliError::NotFound(format!("schedule {}", args.name)))?;
    if !store.remove_schedule(schedule.id.as_str())? {
        return Err(CliError::NotFound(format!("schedule {}", args.name)));
    }
    if ctx.json() {
        print_json(io, &schedule)?;
    } else {
        writeln!(io.out(), "removed schedule {}", schedule.name)?;
    }
    Ok(0)
}

/// Returns whether the spec is an interval (`every …` or `<n><unit>`) or a cron string.
fn spec_kind(spec: &str) -> ScheduleSpecKind {
    let text = spec.trim();
    let interval = text.starts_with("every")
        || text
            .strip_suffix(['s', 'm', 'h', 'd'])
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()));
    if interval {
        ScheduleSpecKind::Interval
    } else {
        ScheduleSpecKind::Cron
    }
}

/// Parses `--missed`.
fn parse_missed(value: Option<&str>) -> Result<MissedPolicy, CliError> {
    match value.unwrap_or("run_once") {
        "run_once" => Ok(MissedPolicy::RunOnce),
        "skip" => Ok(MissedPolicy::Skip),
        other => Err(CliError::Invalid(format!(
            "unknown missed policy: {other} (run_once or skip)"
        ))),
    }
}

/// Returns the display name of a schedule action.
fn action_name(action: &ScheduleAction) -> String {
    match action {
        ScheduleAction::Loop { loop_name } => format!("loop:{loop_name}"),
        ScheduleAction::Command { argv, .. } => format!("command:{}", argv.join(" ")),
        ScheduleAction::JobTemplate { template } => format!("job-template:{}", template.id),
    }
}

/// Returns the wire name of an active state.
fn active_state(state: ActiveState) -> &'static str {
    match state {
        ActiveState::Active => "active",
        ActiveState::Paused => "paused",
    }
}
