//! `promote`, `rollback` and `releases`: guarded Work→Production through `vowp-core::promote`.
//!
//! Spec: `docs/design/cli.md` §5 and `docs/design/crates/vowp-core.md` §8. The core
//! [`Promoter`] owns the pipeline (guards, serving bank, diff, approval, deploy, probe,
//! record); this module only resolves arguments, renders the resulting [`Release`] and maps
//! its state onto the documented exit code.

use vowp_core::promote::{PromoteOpts, Promoter};
use vowp_protocol::{InboxId, InboxKind, ProjectName, Release, ReleaseId, ReleaseState};
use vowp_store::Store;

use crate::cli::{PromoteArgs, PromoteCommand, ReleasesArgs, RollbackArgs};
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Number of release rows printed when `--limit` is omitted.
const DEFAULT_RELEASE_LIMIT: u32 = 20;

/// Runs one release command.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &PromoteCommand) -> Result<i32, CliError> {
    match command {
        PromoteCommand::Promote(args) => promote(ctx, io, args),
        PromoteCommand::Rollback(args) => rollback(ctx, io, args),
        PromoteCommand::Releases(args) => releases(ctx, io, args),
    }
}

/// Runs `vowp promote <change> --to <env>` through the core pipeline.
///
/// Exit 0 for a live release, a staged release awaiting human approval (the inbox id is
/// printed) and a dry run; exit 5 for a refused or failed release. The release row is printed
/// in both human and `--json` output so the operator can see why it stopped.
fn promote(ctx: &Ctx, io: &mut dyn IoStreams, args: &PromoteArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let promoter = Promoter::new(ctx.deps().clone());
    let opts = PromoteOpts {
        dry_run: args.dry_run,
        yes: args.yes,
        approved_by: None,
    };
    let release = promoter.promote(&store, ctx.project(), &args.to, &args.change, opts)?;
    let code = exit_code(release.state);

    if ctx.json() {
        print_json(io, &release)?;
        return Ok(code);
    }

    match release.state {
        ReleaseState::Live => writeln!(
            io.out(),
            "promoted {} to {} (release {})",
            release.change_ref,
            release.env,
            release.id
        )?,
        ReleaseState::Banked => match approval_inbox(&store, &release)? {
            Some(inbox) => writeln!(
                io.out(),
                "staged {} for {} (release {}); approval: inbox {}",
                release.change_ref,
                release.env,
                release.id,
                inbox
            )?,
            None => writeln!(
                io.out(),
                "staged {} for {} (release {}); approval pending",
                release.change_ref,
                release.env,
                release.id
            )?,
        },
        ReleaseState::Guarded => writeln!(io.out(), "dry run passed for {} (release {})", release.env, release.id)?,
        ReleaseState::Refused => writeln!(
            io.out(),
            "release {} refused for {}: {}",
            release.id,
            release.env,
            release.note
        )?,
        ReleaseState::Failed => writeln!(
            io.out(),
            "release {} failed for {}: {}",
            release.id,
            release.env,
            release.note
        )?,
        other => writeln!(
            io.out(),
            "release {} {} for {}",
            release.id,
            state_name(other),
            release.env
        )?,
    }
    Ok(code)
}

/// Runs `vowp rollback <release>` after resolving a unique id prefix.
///
/// Exit 0 when the banked serving copy was restored (`rolled_back`), exit 5 when the rollback
/// command failed. The new release row is printed in human and `--json` output.
fn rollback(ctx: &Ctx, io: &mut dyn IoStreams, args: &RollbackArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let id = resolve_release(ctx.project(), &store, &args.release)?;
    let promoter = Promoter::new(ctx.deps().clone());
    let opts = PromoteOpts {
        yes: args.yes,
        ..PromoteOpts::default()
    };
    let release = promoter.rollback(&store, &id, opts)?;
    let code = exit_code(release.state);

    if ctx.json() {
        print_json(io, &release)?;
        return Ok(code);
    }

    match release.state {
        ReleaseState::RolledBack => writeln!(io.out(), "rolled back release {} (release {})", id, release.id)?,
        ReleaseState::Failed => writeln!(
            io.out(),
            "rollback of release {} failed (release {}): {}",
            id,
            release.id,
            release.note
        )?,
        other => writeln!(
            io.out(),
            "release {} {} for {}",
            release.id,
            state_name(other),
            release.env
        )?,
    }
    Ok(code)
}

/// Lists a project's releases, newest first, optionally restricted to one environment.
fn releases(ctx: &Ctx, io: &mut dyn IoStreams, args: &ReleasesArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let limit = args.limit.unwrap_or(DEFAULT_RELEASE_LIMIT);
    let promoter = Promoter::new(ctx.deps().clone());
    let releases = promoter.releases(&store, ctx.project(), args.env.as_deref(), limit)?;

    if ctx.json() {
        print_json(io, &releases)?;
        return Ok(0);
    }

    let now = ctx.clock().now();
    let mut table = Table::new(&["ID", "ENV", "CHANGE", "STATE", "FROM→TO", "CREATED"]);
    for release in &releases {
        table.row(vec![
            release.id.to_string(),
            release.env.clone(),
            release.change_ref.clone(),
            state_name(release.state).to_owned(),
            format!(
                "{}→{}",
                short_commit(release.from_commit.as_deref()),
                short_commit(release.to_commit.as_deref())
            ),
            local_time_at(release.created_at, now, ctx.tz_offset_minutes()),
        ]);
    }
    write!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Resolves a release id or its unique prefix of at least six characters.
fn resolve_release(project: &ProjectName, store: &Store, value: &str) -> Result<ReleaseId, CliError> {
    let prefix = value.trim();
    if prefix.chars().count() < 6 {
        return Err(CliError::Invalid(format!(
            "release id prefix must be at least 6 characters: {value}"
        )));
    }
    let matches: Vec<Release> = store
        .list_releases(project, None)?
        .into_iter()
        .filter(|release| release.id.as_str().starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(CliError::NotFound(format!("release {value}"))),
        [release] => Ok(release.id.clone()),
        _ => {
            let candidates: Vec<&str> = matches.iter().map(|release| release.id.as_str()).collect();
            Err(CliError::Invalid(format!(
                "ambiguous release prefix {value}: {}",
                candidates.join(", ")
            )))
        }
    }
}

/// Returns the inbox approval item created for a banked release, when there is one.
fn approval_inbox(store: &Store, release: &Release) -> Result<Option<InboxId>, CliError> {
    let marker = format!("release: {}\n", release.id);
    Ok(store
        .list_inbox(None)?
        .into_iter()
        .find(|item| item.kind == InboxKind::Approval && item.body.contains(&marker))
        .map(|item| item.id))
}

/// Maps a release state onto the documented exit code: 0 for a completed step, 5 otherwise.
fn exit_code(state: ReleaseState) -> i32 {
    match state {
        ReleaseState::Live | ReleaseState::Guarded | ReleaseState::Banked | ReleaseState::RolledBack => 0,
        _ => 5,
    }
}

/// Returns the snake_case release state used in human tables.
fn state_name(state: ReleaseState) -> &'static str {
    match state {
        ReleaseState::Planned => "planned",
        ReleaseState::Guarded => "guarded",
        ReleaseState::Banked => "banked",
        ReleaseState::Approved => "approved",
        ReleaseState::Deploying => "deploying",
        ReleaseState::Probing => "probing",
        ReleaseState::Live => "live",
        ReleaseState::Failed => "failed",
        ReleaseState::RolledBack => "rolled_back",
        ReleaseState::Refused => "refused",
    }
}

/// Shows a commit as its seven-character prefix, or `-` when it is unknown.
fn short_commit(commit: Option<&str>) -> String {
    match commit {
        Some(sha) if !sha.is_empty() => sha.chars().take(7).collect(),
        _ => "-".to_owned(),
    }
}
