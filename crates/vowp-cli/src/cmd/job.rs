//! `vowp job`: add, update, list, show and the retry/park/cancel/keeper actions.
//!
//! Spec: `docs/design/cli.md` §3. State changes go through `vowp_core::requests`: with a live
//! supervisor the CLI enqueues a request row and waits up to 5 s; without one it applies the
//! same graph event directly. Keeper commands (`nudge`, `restart`, `replan`, `fail`) only
//! enqueue, as `loops.md` §7 requires.

use std::collections::BTreeSet;
use std::thread;
use std::time::{Duration, Instant};

use vowp_core::requests::{self, Request};
use vowp_graph::{Graph, JobEvent, transition};
use vowp_protocol::{EdgeKind, Gate, Job, JobId, JobKind, JobLoop, JobPatch, JobState, NewJob, Role, default_gates};
use vowp_store::{JobFilter, Store};

use crate::cli::{JobAddArgs, JobCommand, JobLsArgs, JobShowArgs, JobUpdateArgs};
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Runs one job subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &JobCommand) -> Result<i32, CliError> {
    match command {
        JobCommand::Add(args) => add(ctx, io, args),
        JobCommand::Update(args) => update(ctx, io, args),
        JobCommand::Ls(args) => list(ctx, io, args),
        JobCommand::Show(args) => show(ctx, io, args),
        JobCommand::Retry(args) => retry(ctx, io, &args.id),
        JobCommand::Park(args) => park(ctx, io, &args.id, args.reason.as_deref()),
        JobCommand::Nudge(args) => keeper(ctx, io, &args.id, KeeperAction::Nudge),
        JobCommand::Restart(args) => keeper(ctx, io, &args.id, KeeperAction::Restart),
        JobCommand::Replan(args) => keeper(ctx, io, &args.id, KeeperAction::Replan),
        JobCommand::Fail(args) => keeper(ctx, io, &args.id, KeeperAction::Fail(args.reason.clone())),
        JobCommand::Cancel(args) => cancel(ctx, io, &args.id),
    }
}

/// Adds one job, writes its brief and prints the row.
fn add(ctx: &Ctx, io: &mut dyn IoStreams, args: &JobAddArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(&args.id)?;
    if store.job(&id)?.is_some() {
        return Err(CliError::Conflict(format!("job {id} already exists")));
    }
    let kind = parse_kind(&args.kind)?;
    let role = match &args.role {
        Some(role) => parse_role(role)?,
        None => default_role(kind),
    };
    if args.title.trim().is_empty() {
        return Err(CliError::Invalid("job title must not be empty".to_owned()));
    }
    if let Some(profile) = &args.profile
        && !ctx.config().profiles.contains_key(profile)
    {
        return Err(CliError::Invalid(format!("unknown profile: {profile}")));
    }
    if args.max_attempts == Some(0) {
        return Err(CliError::Invalid("--max-attempts must be at least 1".to_owned()));
    }
    let needs = parse_job_ids(&args.needs)?;
    for need in &needs {
        if need == &id {
            return Err(CliError::Invalid(format!(
                "job {id} cannot need itself (dependency cycle)"
            )));
        }
        if store.job(need)?.is_none() {
            return Err(CliError::Invalid(format!("unknown need: {need}")));
        }
    }
    let gates = if args.gates.is_empty() {
        None
    } else {
        Some(resolve_gates(ctx, &args.gates)?)
    };
    let loop_ = args.max_attempts.map(|max_attempts| JobLoop {
        max_attempts,
        ..JobLoop::default()
    });
    let new = NewJob {
        id: id.clone(),
        title: args.title.clone(),
        kind,
        role,
        profile: args.profile.clone(),
        needs,
        owns: args.owns.clone(),
        reads: args.reads.clone(),
        gates,
        priority: args.priority,
        loop_,
        brief_text: None,
    };
    let body = brief_body(args, io)?;
    let brief = render_brief(&new, &body);
    let job = store.insert_job(ctx.project(), &new, "cli")?;
    if let Err(error) = write_brief(ctx, &job, &brief) {
        // The job is still pending, so this rolls the row back when the brief cannot be
        // written. Validation/read failures happen before the insert and leave nothing.
        let _ = store.delete_job(&job.id);
        return Err(error);
    }
    if ctx.json() {
        print_json(io, &job)?;
    } else {
        writeln!(io.out(), "added job {} ({})", job.id, job.state.as_str())?;
    }
    Ok(0)
}

/// Applies a partial update after validating graph and state constraints.
fn update(ctx: &Ctx, io: &mut dyn IoStreams, args: &JobUpdateArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(&args.id)?;
    let job = store.require_job(&id)?;
    if let Some(title) = &args.title
        && title.trim().is_empty()
    {
        return Err(CliError::Invalid("job title must not be empty".to_owned()));
    }
    if let Some(profile) = &args.profile
        && !ctx.config().profiles.contains_key(profile)
    {
        return Err(CliError::Invalid(format!("unknown profile: {profile}")));
    }
    let changing_globs = args.owns.is_some() || args.gates.is_some();
    if changing_globs
        && matches!(
            job.state,
            JobState::Running | JobState::Gating | JobState::Reviewing | JobState::Merging
        )
    {
        return Err(CliError::Conflict(format!(
            "job {id} is {} and its owns/gates cannot change now",
            job.state.as_str()
        )));
    }

    let gates = match &args.gates {
        Some(tokens) => Some(resolve_gates(ctx, tokens)?),
        None => None,
    };
    let add_needs = parse_job_ids(&args.add_needs)?;
    let remove_needs = parse_job_ids(&args.remove_needs)?;
    for need in &add_needs {
        if need == &id {
            return Err(CliError::Invalid(format!(
                "job {id} cannot need itself (dependency cycle)"
            )));
        }
        if store.job(need)?.is_none() {
            return Err(CliError::Invalid(format!("unknown need: {need}")));
        }
    }
    let mut needs = job.needs.clone();
    for need in &add_needs {
        if !needs.contains(need) {
            needs.push(need.clone());
        }
    }
    for need in &remove_needs {
        needs.retain(|current| current != need);
    }
    validate_graph_with(ctx, &job, &needs, &remove_needs)?;

    let patch = JobPatch {
        title: args.title.clone(),
        priority: args.priority,
        profile: args.profile.clone(),
        owns: args.owns.clone(),
        reads: args.reads.clone(),
        gates,
        loop_: None,
        add_needs,
        remove_needs,
        note: args.note.clone(),
    };
    let updated = store.patch_job(&id, &patch)?;
    if ctx.json() {
        print_json(io, &updated)?;
    } else {
        writeln!(io.out(), "updated job {}", updated.id)?;
    }
    Ok(0)
}

/// Validates the graph that would exist after an update.
fn validate_graph_with(ctx: &Ctx, job: &Job, needs: &[JobId], removed: &[JobId]) -> Result<(), CliError> {
    let store = ctx.store()?;
    let mut jobs = store.list_jobs(&JobFilter {
        project: Some(ctx.project().clone()),
        ..JobFilter::default()
    })?;
    if let Some(target) = jobs.iter_mut().find(|candidate| candidate.id == job.id) {
        target.needs = needs.to_vec();
    }
    let mut edges = store.list_edges(Some(ctx.project()))?;
    edges.retain(|edge| !(edge.kind == EdgeKind::Needs && edge.src == job.id && removed.contains(&edge.dst)));
    Graph::new(jobs, edges)?;
    Ok(())
}

/// Lists jobs with the documented table and JSON array shape.
fn list(ctx: &Ctx, io: &mut dyn IoStreams, args: &JobLsArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let states = if args.state.is_empty() {
        if args.all {
            Vec::new()
        } else {
            vec![
                JobState::Pending,
                JobState::Ready,
                JobState::Running,
                JobState::Stalled,
                JobState::Gating,
                JobState::Reviewing,
                JobState::Fixing,
                JobState::Merging,
                JobState::Failed,
                JobState::Parked,
            ]
        }
    } else {
        args.state
            .iter()
            .map(|state| parse_state(state))
            .collect::<Result<Vec<_>, _>>()?
    };
    let filter = JobFilter {
        project: Some(ctx.project().clone()),
        states,
        kind: args.kind.as_deref().map(parse_kind).transpose()?,
        role: args.role.as_deref().map(parse_role).transpose()?,
        id_prefix: None,
        limit: None,
    };
    let jobs = store.list_jobs(&filter)?;

    if ctx.json() {
        print_json(io, &jobs)?;
        return Ok(0);
    }

    let all_jobs = store.list_jobs(&JobFilter {
        project: Some(ctx.project().clone()),
        ..JobFilter::default()
    })?;
    let cancelled: BTreeSet<JobId> = all_jobs
        .iter()
        .filter(|job| job.state == JobState::Cancelled)
        .map(|job| job.id.clone())
        .collect();
    let now = ctx.clock().now();
    let mut table = Table::new(&[
        "ID", "STATE", "KIND", "ROLE", "PRIO", "NEEDS", "ATTEMPTS", "UPDATED", "NOTE",
    ]);
    for job in &jobs {
        let mut note = job.note.clone();
        if job.needs.iter().any(|need| cancelled.contains(need)) {
            if !note.is_empty() {
                note.push(' ');
            }
            note.push_str("blocked-by-cancelled");
        }
        let needs: Vec<String> = job.needs.iter().map(ToString::to_string).collect();
        table.row(vec![
            job.id.to_string(),
            job.state.as_str().to_owned(),
            kind_str(job.kind).to_owned(),
            role_str(job.role).to_owned(),
            job.priority.to_string(),
            needs.join(","),
            job.attempts.to_string(),
            local_time_at(job.updated_at, now, ctx.tz_offset_minutes()),
            note,
        ]);
    }
    write!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Shows one job and optionally its runs or gate evidence.
fn show(ctx: &Ctx, io: &mut dyn IoStreams, args: &JobShowArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(&args.id)?;
    let job = store.require_job(&id)?;

    if args.brief || args.report {
        let relative = if args.brief { &job.brief } else { &job.report };
        let path = ctx.project_root().join(relative);
        let text = std::fs::read_to_string(&path)?;
        write!(io.out(), "{text}")?;
        if !text.ends_with('\n') {
            writeln!(io.out())?;
        }
        return Ok(0);
    }

    if ctx.json() {
        print_json(io, &job)?;
        return Ok(0);
    }

    let now = ctx.clock().now();
    writeln!(io.out(), "job {}", job.id)?;
    writeln!(io.out(), "  title: {}", job.title)?;
    writeln!(
        io.out(),
        "  kind: {}  role: {}  profile: {}  state: {}  priority: {}",
        kind_str(job.kind),
        role_str(job.role),
        job.profile,
        job.state.as_str(),
        job.priority
    )?;
    writeln!(io.out(), "  needs: {}", join_ids(&job.needs))?;
    writeln!(io.out(), "  owns: {}", job.owns.join(", "))?;
    writeln!(io.out(), "  reads: {}", job.reads.join(", "))?;
    writeln!(io.out(), "  brief: {}", job.brief)?;
    writeln!(io.out(), "  report: {}", job.report)?;
    writeln!(
        io.out(),
        "  attempts: {}  continues: {}  hang_restarts: {}",
        job.attempts,
        job.continues,
        job.hang_restarts
    )?;
    if let Some(reason) = job.parked_reason {
        writeln!(io.out(), "  parked_reason: {reason:?}")?;
    }
    if !job.note.is_empty() {
        writeln!(io.out(), "  note: {}", job.note)?;
    }
    writeln!(
        io.out(),
        "  updated: {}",
        local_time_at(job.updated_at, now, ctx.tz_offset_minutes())
    )?;
    writeln!(io.out(), "edges:")?;
    for edge in store.edges_of(&job.id)? {
        writeln!(
            io.out(),
            "  {} --{}--> {}",
            edge.src,
            edge_kind_str(edge.kind),
            edge.dst
        )?;
    }

    if args.runs {
        writeln!(io.out(), "runs:")?;
        for run in store.runs_of_job(&job.id)? {
            let executor = run
                .executor
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned());
            let outcome = run
                .outcome
                .map(|outcome| format!("{outcome:?}"))
                .unwrap_or_else(|| "running".to_owned());
            let cost = run
                .cost
                .usd
                .map(|usd| format!("${usd:.2}"))
                .unwrap_or_else(|| "?".to_owned());
            writeln!(
                io.out(),
                "  {}  {}  {}  {}  {}",
                run.id,
                executor,
                outcome,
                cost,
                local_time_at(run.started_at, now, ctx.tz_offset_minutes())
            )?;
        }
    }

    if args.gates {
        writeln!(io.out(), "gates:")?;
        for result in store.latest_gate_results(&job.id)? {
            let evidence = serde_json::to_string(&result.evidence).unwrap_or_default();
            let evidence: String = evidence
                .chars()
                .rev()
                .take(200)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            writeln!(
                io.out(),
                "  {}  {}  {}ms  {}",
                gate_kind_str(result.gate.kind()),
                if result.passed { "pass" } else { "fail" },
                result.duration_ms,
                evidence
            )?;
        }
    }
    Ok(0)
}

/// Retries a failed, stalled or parked job by state.
fn retry(ctx: &Ctx, io: &mut dyn IoStreams, id: &str) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(id)?;
    let job = store.require_job(&id)?;
    let (request, expected) = match job.state {
        JobState::Failed => (Request::RetryJob { job: id.clone() }, vec![JobState::Ready]),
        JobState::Parked => (Request::ResolveJob { job: id.clone() }, vec![JobState::Ready]),
        JobState::Stalled => (Request::RetryJob { job: id.clone() }, vec![JobState::Running]),
        other => {
            return Err(CliError::Conflict(format!(
                "job {id} is {} and cannot be retried",
                other.as_str()
            )));
        }
    };
    submit(ctx, &request, &id, &expected)?;
    print_job_result(ctx, io, &id, "retried")?;
    Ok(0)
}

/// Parks a non-terminal job.
fn park(ctx: &Ctx, io: &mut dyn IoStreams, id: &str, reason: Option<&str>) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(id)?;
    let job = store.require_job(&id)?;
    if job.state.is_terminal() {
        return Err(CliError::Conflict(format!(
            "job {id} is {} and cannot be parked",
            job.state.as_str()
        )));
    }
    let reason = reason.unwrap_or("parked from the command line").to_owned();
    submit(
        ctx,
        &Request::ParkJob {
            job: id.clone(),
            reason,
        },
        &id,
        &[JobState::Parked],
    )?;
    print_job_result(ctx, io, &id, "parked")?;
    Ok(0)
}

/// Cancels a non-terminal job.
fn cancel(ctx: &Ctx, io: &mut dyn IoStreams, id: &str) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(id)?;
    let job = store.require_job(&id)?;
    if job.state.is_terminal() {
        return Err(CliError::Conflict(format!(
            "job {id} is {} and cannot be cancelled",
            job.state.as_str()
        )));
    }
    submit(
        ctx,
        &Request::CancelJob { job: id.clone() },
        &id,
        &[JobState::Cancelled],
    )?;
    print_job_result(ctx, io, &id, "cancelled")?;
    Ok(0)
}

/// One keeper command.
enum KeeperAction {
    /// Continue a stalled job without a restart.
    Nudge,
    /// Restart a stalled job with a fresh process.
    Restart,
    /// Replace the job through a planner turn.
    Replan,
    /// Give up on a stalled job.
    Fail(String),
}

/// Validates and enqueues one keeper request (never applied directly).
fn keeper(ctx: &Ctx, io: &mut dyn IoStreams, id: &str, action: KeeperAction) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = JobId::parse(id)?;
    let job = store.require_job(&id)?;
    let (event, request, queued_label, refusal_label) = match action {
        KeeperAction::Nudge => (
            JobEvent::KeeperNudge { text: None },
            Request::NudgeJob { job: id.clone() },
            "nudge",
            "nudged",
        ),
        KeeperAction::Restart => (
            JobEvent::KeeperRestart,
            Request::RestartJob { job: id.clone() },
            "restart",
            "restarted",
        ),
        KeeperAction::Replan => (
            JobEvent::KeeperReplan,
            Request::ReplanJob { job: id.clone() },
            "replan",
            "replanned",
        ),
        KeeperAction::Fail(reason) => (
            JobEvent::KeeperFail,
            Request::FailJob {
                job: id.clone(),
                reason,
            },
            "fail",
            "failed",
        ),
    };
    if transition(&job, &event).is_err() {
        return Err(CliError::Conflict(format!(
            "job {id} is {} and cannot be {refusal_label}",
            job.state.as_str(),
        )));
    }
    requests::push(&store, &request)?;
    print_job_result(ctx, io, &id, &format!("queued {queued_label} for"))?;
    Ok(0)
}

/// Applies a request directly or enqueues it when a supervisor holds the lock.
fn submit(ctx: &Ctx, request: &Request, job_id: &JobId, expected: &[JobState]) -> Result<(), CliError> {
    let store = ctx.store()?;
    if crate::ctx::supervisor_pid(ctx.paths()).is_some() {
        requests::push(&store, request)?;
        wait_for_state(&store, job_id, expected, Duration::from_secs(5));
    } else {
        requests::apply_direct(ctx.deps(), &store, request)?;
    }
    Ok(())
}

/// Polls the job row for up to `timeout`; a supervisor that is slow leaves the request queued.
fn wait_for_state(store: &Store, job_id: &JobId, expected: &[JobState], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match store.require_job(job_id) {
            Ok(job) if expected.contains(&job.state) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Prints the job after a mutation, or one human summary line.
fn print_job_result(ctx: &Ctx, io: &mut dyn IoStreams, id: &JobId, verb: &str) -> Result<(), CliError> {
    let store = ctx.store()?;
    let job = store.require_job(id)?;
    if ctx.json() {
        print_json(io, &job)?;
    } else {
        writeln!(io.out(), "{verb} job {} ({})", job.id, job.state.as_str())?;
    }
    Ok(())
}

/// Reads the brief body from `--brief-file`, `--brief`, stdin, or the embedded template.
fn brief_body(args: &JobAddArgs, io: &mut dyn IoStreams) -> Result<String, CliError> {
    if let Some(path) = &args.brief_file {
        return Ok(std::fs::read_to_string(path)?);
    }
    if let Some(value) = &args.brief {
        if value == "-" {
            return crate::read_stdin(io);
        }
        return Ok(std::fs::read_to_string(value)?);
    }
    Ok(crate::framework::brief_template_body().to_owned())
}

/// Renders the generated front-matter plus the caller's body.
fn render_brief(new: &NewJob, body: &str) -> String {
    let gates = new.gates.clone().unwrap_or_else(|| default_gates(new.kind));
    let gates: Vec<String> = gates.iter().map(|gate| yaml_string(&gate.short())).collect();
    let owns: Vec<String> = new.owns.iter().map(|glob| yaml_string(glob)).collect();
    let needs: Vec<String> = new.needs.iter().map(|need| yaml_string(need.as_str())).collect();
    format!(
        "---\njob: {}\nowns: [{}]\nmust_not_touch: [\"everything else\"]\nneeds_done: [{}]\ngates: [{}]\ndone_when: \"report has status: done and every gate passes\"\n---\n\n{}\n",
        new.id,
        owns.join(", "),
        needs.join(", "),
        gates.join(", "),
        body.trim_start()
    )
}

/// Writes one job's brief file, creating `.vowp/jobs/<id>/`.
fn write_brief(ctx: &Ctx, job: &Job, brief: &str) -> Result<(), CliError> {
    let path = ctx.project_root().join(&job.brief);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, brief)?;
    Ok(())
}

/// Resolves `--gates` tokens: a named set first, then the short gate form.
fn resolve_gates(ctx: &Ctx, tokens: &[String]) -> Result<Vec<Gate>, CliError> {
    let mut gates = Vec::new();
    for token in tokens {
        if let Some(set) = ctx.config().gates.get(token) {
            gates.extend(set.iter().cloned());
        } else {
            gates.push(Gate::from_short(token)?);
        }
    }
    if gates.is_empty() {
        return Err(CliError::Invalid("--gates needs at least one gate".to_owned()));
    }
    Ok(gates)
}

/// Quotes one YAML scalar using JSON's double-quoted form, which is valid YAML.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Parses a comma list of job ids.
fn parse_job_ids(values: &[String]) -> Result<Vec<JobId>, CliError> {
    values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Ok(JobId::parse(value.trim())?))
        .collect()
}

/// Joins job ids for human output.
fn join_ids(ids: &[JobId]) -> String {
    ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

/// Parses a [`JobKind`] from its snake_case name.
pub fn parse_kind(value: &str) -> Result<JobKind, CliError> {
    match value {
        "plan" => Ok(JobKind::Plan),
        "spec" => Ok(JobKind::Spec),
        "build" => Ok(JobKind::Build),
        "test" => Ok(JobKind::Test),
        "verify" => Ok(JobKind::Verify),
        "fix" => Ok(JobKind::Fix),
        "merge" => Ok(JobKind::Merge),
        "docs" => Ok(JobKind::Docs),
        "research" => Ok(JobKind::Research),
        "promote" => Ok(JobKind::Promote),
        "release" => Ok(JobKind::Release),
        "decision" => Ok(JobKind::Decision),
        other => Err(CliError::Invalid(format!("unknown job kind: {other}"))),
    }
}

/// Returns the snake_case name of a [`JobKind`].
pub fn kind_str(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Plan => "plan",
        JobKind::Spec => "spec",
        JobKind::Build => "build",
        JobKind::Test => "test",
        JobKind::Verify => "verify",
        JobKind::Fix => "fix",
        JobKind::Merge => "merge",
        JobKind::Docs => "docs",
        JobKind::Research => "research",
        JobKind::Promote => "promote",
        JobKind::Release => "release",
        JobKind::Decision => "decision",
    }
}

/// Parses a [`Role`] from its snake_case name.
pub fn parse_role(value: &str) -> Result<Role, CliError> {
    match value {
        "planner" => Ok(Role::Planner),
        "keeper" => Ok(Role::Keeper),
        "worker" => Ok(Role::Worker),
        "verifier" => Ok(Role::Verifier),
        other => Err(CliError::Invalid(format!("unknown role: {other}"))),
    }
}

/// Returns the snake_case name of a [`Role`].
pub fn role_str(role: Role) -> &'static str {
    match role {
        Role::Planner => "planner",
        Role::Keeper => "keeper",
        Role::Worker => "worker",
        Role::Verifier => "verifier",
    }
}

/// Returns the role `cli.md` assigns to a kind.
fn default_role(kind: JobKind) -> Role {
    match kind {
        JobKind::Plan => Role::Planner,
        JobKind::Verify => Role::Verifier,
        JobKind::Decision => Role::Keeper,
        _ => Role::Worker,
    }
}

/// Parses a [`JobState`] from its snake_case name.
fn parse_state(value: &str) -> Result<JobState, CliError> {
    match value {
        "pending" => Ok(JobState::Pending),
        "ready" => Ok(JobState::Ready),
        "running" => Ok(JobState::Running),
        "stalled" => Ok(JobState::Stalled),
        "gating" => Ok(JobState::Gating),
        "reviewing" => Ok(JobState::Reviewing),
        "fixing" => Ok(JobState::Fixing),
        "merging" => Ok(JobState::Merging),
        "done" => Ok(JobState::Done),
        "failed" => Ok(JobState::Failed),
        "parked" => Ok(JobState::Parked),
        "replanned" => Ok(JobState::Replanned),
        "cancelled" => Ok(JobState::Cancelled),
        other => Err(CliError::Invalid(format!("unknown job state: {other}"))),
    }
}

/// Returns the debug-free snake_case name of an edge kind.
fn edge_kind_str(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Needs => "needs",
        EdgeKind::Verifies => "verifies",
        EdgeKind::Fixes => "fixes",
        EdgeKind::Escalates => "escalates",
        EdgeKind::Blocks => "blocks",
    }
}

/// Returns the snake_case name of a gate kind.
fn gate_kind_str(kind: vowp_protocol::GateKind) -> &'static str {
    match kind {
        vowp_protocol::GateKind::Report => "report",
        vowp_protocol::GateKind::Cmd => "cmd",
        vowp_protocol::GateKind::OwnsRespected => "owns_respected",
        vowp_protocol::GateKind::Pushed => "pushed",
        vowp_protocol::GateKind::Review => "review",
        vowp_protocol::GateKind::Script => "script",
        vowp_protocol::GateKind::Deploy => "deploy",
        vowp_protocol::GateKind::SecretScan => "secret_scan",
    }
}
