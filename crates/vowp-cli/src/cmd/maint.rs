//! `reindex`, `backup`, `schema` and `version`.
//!
//! Spec: `docs/design/cli.md` §6. Reindex reads the authoritative session logs, backup uses
//! SQLite's online backup API, schema prints the exported wire schemas and version prints the
//! binary, git and schema versions.

use std::process::Command;

use serde::Serialize;
use vowp_core::lifecycle::backup::{BackupOptions, backup as write_backup, restore as restore_backup};
use vowp_protocol::BackupReport;
use vowp_store::{NodePaths, SessionFilter, Store};

use crate::cli::{BackupArgs, MaintCommand, ReindexArgs, SchemaArgs};
use crate::output::print_json;
use crate::{CliError, Ctx, IoStreams};

/// Serialisable form of [`ReindexReport`].
#[derive(Debug, Serialize)]
struct ReindexOutput {
    sessions: u32,
    events: u64,
    capsules: u32,
    natives: u32,
    torn_tails: u32,
    problems: Vec<String>,
}

/// Runs one maintenance command.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &MaintCommand) -> Result<i32, CliError> {
    match command {
        MaintCommand::Reindex(args) => reindex(ctx, io, args),
        MaintCommand::Backup(args) => backup(ctx, io, args),
        MaintCommand::Schema(args) => schema(ctx, io, args),
        MaintCommand::Version => version(ctx, io),
    }
}

/// Rebuilds the session index from the logs.
fn reindex(ctx: &Ctx, io: &mut dyn IoStreams, args: &ReindexArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = match &args.session {
        Some(input) => Some(resolve_session(&store, ctx.paths(), input)?),
        None => None,
    };
    let report = vowp_store::reindex::reindex(&store, ctx.paths(), session.as_ref())?;
    let output = ReindexOutput {
        sessions: report.sessions,
        events: report.events,
        capsules: report.capsules,
        natives: report.natives,
        torn_tails: report.torn_tails,
        problems: report.problems,
    };
    if ctx.json() {
        print_json(io, &output)?;
    } else {
        writeln!(
            io.out(),
            "reindexed {} sessions, {} events, {} capsules, {} natives ({} torn tails)",
            output.sessions,
            output.events,
            output.capsules,
            output.natives,
            output.torn_tails
        )?;
        for problem in &output.problems {
            writeln!(io.err(), "warning: {problem}")?;
        }
    }
    Ok(0)
}

/// Writes or restores a backup.
///
/// Restore is routed before [`Ctx::store`] on purpose: rescuing a lost serving home must
/// not require an initialised source home, and restoring never reads the live store.
fn backup(ctx: &Ctx, io: &mut dyn IoStreams, args: &BackupArgs) -> Result<i32, CliError> {
    let opts = BackupOptions {
        to: args.to.as_ref().map(|path| path.display().to_string()),
        offsite: args.offsite.clone(),
        encrypt: args.encrypt.clone(),
        restore: args.restore.clone(),
        to_home: args.to_home.clone(),
        force: args.force,
    };
    let report = if opts.restore.is_some() {
        restore_backup(ctx.deps(), &opts)?
    } else {
        let store = ctx.store()?;
        write_backup(ctx.deps(), &store, &opts)?
    };
    print_report(ctx, io, &report)?;
    Ok(0)
}

/// Prints a backup or restore report.
fn print_report(ctx: &Ctx, io: &mut dyn IoStreams, report: &BackupReport) -> Result<(), CliError> {
    if ctx.json() {
        print_json(io, report)?;
        return Ok(());
    }
    if report.files.len() == 1 && report.files[0].kind == "global-db" {
        writeln!(io.out(), "{} ({} bytes)", report.directory, report.bytes)?;
    } else {
        writeln!(
            io.out(),
            "{} ({} files, {} bytes)",
            report.directory,
            report.files.len(),
            report.bytes
        )?;
    }
    if let Some(target) = &report.offsite {
        writeln!(io.out(), "offsite: {target}")?;
    }
    if report.encrypted {
        writeln!(io.out(), "encrypted with age")?;
    }
    for skipped in &report.skipped {
        writeln!(io.err(), "skipped: {skipped}")?;
    }
    Ok(())
}

/// Lists or prints one JSON Schema.
fn schema(ctx: &Ctx, io: &mut dyn IoStreams, args: &SchemaArgs) -> Result<i32, CliError> {
    match &args.type_name {
        None => {
            let names: Vec<&str> = vowp_protocol::schema::all_schemas().keys().copied().collect();
            if ctx.json() {
                print_json(io, &names)?;
            } else {
                for name in names {
                    writeln!(io.out(), "{name}")?;
                }
            }
        }
        Some(name) => {
            let schema = vowp_protocol::schema::schema_for_name(name)
                .ok_or_else(|| CliError::NotFound(format!("schema {name}")))?;
            print_json(io, &schema)?;
        }
    }
    Ok(0)
}

/// Prints the binary, git and schema versions.
///
/// The printed schema version is the project schema this binary writes, so the command
/// needs no store: it works on a control machine, which has no local database.
fn version(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let schema = vowp_store::SCHEMA_VERSION;
    let git = git_sha();
    if ctx.json() {
        print_json(
            io,
            &serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "git": git,
                "schema": schema,
            }),
        )?;
    } else {
        writeln!(io.out(), "vowp {} ({git}) schema {schema}", env!("CARGO_PKG_VERSION"))?;
    }
    Ok(0)
}

/// Returns the git sha compiled in by the release build, else the checkout's short HEAD.
fn git_sha() -> String {
    if let Some(sha) = option_env!("VOWP_GIT_SHA").or(option_env!("GIT_SHA")) {
        return sha.to_owned();
    }
    match Command::new("git").args(["rev-parse", "--short", "HEAD"]).output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Resolves a session id or unique prefix for `reindex --session`.
fn resolve_session(store: &Store, paths: &NodePaths, input: &str) -> Result<vowp_protocol::SessionId, CliError> {
    if let Ok(id) = vowp_protocol::SessionId::parse(input) {
        if store.session(&id)?.is_some() || paths.session_dir(&id).is_dir() {
            return Ok(id);
        }
        return Err(CliError::NotFound(format!("session {input}")));
    }
    if input.len() < 6 {
        return Err(CliError::Invalid(
            "session id prefix must be at least 6 characters".to_owned(),
        ));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<vowp_protocol::Session> = store
        .list_sessions(&SessionFilter::default())?
        .into_iter()
        .filter(|session| session.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [session] => Ok(session.id.clone()),
        [] => Err(CliError::NotFound(format!("session {input}"))),
        many => Err(CliError::Invalid(format!(
            "ambiguous session prefix {input}: {}",
            many.iter()
                .map(|session| session.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}
