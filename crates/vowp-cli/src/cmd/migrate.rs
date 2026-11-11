//! `vowp migrate` — the v0.1 to v0.2 home migration.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.9. The split itself lives in
//! `vowp_store::migrate`; this wrapper only picks the mode, prints the report and maps
//! store refusals onto the CLI's exit codes. `vowp up` calls the same store function
//! automatically after its pre-migration backup, unless `--no-migrate` is given.

use std::path::PathBuf;

use clap::Args;
use vowp_protocol::VowpConfig;
use vowp_store::MigrationReport;

use crate::cli::Global;
use crate::output::{Table, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp migrate`.
#[derive(Debug, Clone, Default, Args)]
pub struct MigrateArgs {
    /// Report what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Restore from this pre-migration backup instead of migrating.
    #[arg(long, value_name = "BACKUP")]
    pub rollback: Option<PathBuf>,
    /// Confirm the migration (`--dry-run` needs no confirmation).
    #[arg(long)]
    pub yes: bool,
}

/// Runs `vowp migrate` with a project context.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &MigrateArgs) -> Result<i32, CliError> {
    migrate(ctx.paths(), ctx.json(), ctx.color(), io, args)
}

/// Runs `vowp migrate` before a project context exists.
///
/// A v0.1 home cannot be opened through [`Ctx::build`]: the store refuses its schema until
/// this command has split it. The wrapper is therefore dispatched straight from the node
/// paths, which is also what lets it run from `~/.vowp` itself (the systemd unit's
/// `WorkingDirectory`) without a checkout or a `--project`.
pub fn run_node(global: &Global, io: &mut dyn IoStreams, args: &MigrateArgs) -> Result<i32, CliError> {
    let paths = crate::ctx::node_paths(global)?;
    crate::ctx::require_initialised_home(&paths)?;
    let color = crate::ctx::color_enabled(global, io);
    migrate(&paths, global.json, color, io, args)
}

/// Runs one migration mode and prints its report.
fn migrate(
    paths: &vowp_store::NodePaths,
    json: bool,
    color: bool,
    io: &mut dyn IoStreams,
    args: &MigrateArgs,
) -> Result<i32, CliError> {
    let report = match (&args.rollback, args.dry_run) {
        (Some(backup), false) => vowp_store::migrate_rollback(paths, backup)?,
        (Some(_), true) => {
            return Err(CliError::Invalid(
                "--dry-run and --rollback cannot be combined".to_owned(),
            ));
        }
        (None, true) => vowp_store::migrate_v01_dry_run(paths)?,
        (None, false) => {
            if !args.yes {
                // Let the store's preflight classify a v0.2 or missing home first, so a
                // caller who cannot migrate at all still gets the specific schema error.
                // On a real v0.1 source, refuse before the backup/swap writes anything.
                let _ = vowp_store::migrate_v01_dry_run(paths)?;
                return Err(CliError::Guard(
                    "refusing to migrate the home without --yes; re-run with --yes, or --dry-run to preview".to_owned(),
                ));
            }
            vowp_store::migrate_v01(paths)?
        }
    };
    if !report.dry_run {
        if report.to_version == 2 {
            crate::node_ctx::ensure_home_project_file(paths)?;
        } else if report.to_version == 1 {
            remove_node_project_file(paths);
        }
    }
    print_report(json, color, io, &report)?;
    Ok(0)
}

/// Removes the node-level `vowp.toml` when a rollback returns the home to v0.1.
///
/// Only the reserved `_node` file is removed; a user file with another project name is
/// left alone.
fn remove_node_project_file(paths: &vowp_store::NodePaths) {
    let path = paths.root().join("vowp.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if VowpConfig::from_toml(&text)
        .is_ok_and(|config| config.project.name.as_str() == vowp_store::RESERVED_NODE_PROJECT)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Prints one migration report as JSON or a short human summary with per-table counts.
fn print_report(json: bool, color: bool, io: &mut dyn IoStreams, report: &MigrationReport) -> Result<(), CliError> {
    if json {
        print_json(io, report)?;
        return Ok(());
    }
    let verb = if report.dry_run {
        "would migrate"
    } else if report.to_version == 1 {
        "rolled back"
    } else {
        "migrated"
    };
    writeln!(
        io.out(),
        "{verb} schema {} -> {} ({} project(s)); backup: {}",
        report.from_version,
        report.to_version,
        report.projects.len(),
        report.backup
    )?;
    if !report.projects.is_empty() {
        let names = report
            .projects
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(io.out(), "projects: {names}")?;
    }
    if !report.tables.is_empty() {
        let mut table = Table::new(&["TABLE", "BEFORE", "AFTER"]);
        for (name, count) in &report.tables {
            table.row(vec![name.clone(), count.before.to_string(), count.after.to_string()]);
        }
        writeln!(io.out(), "{}", table.render(color))?;
    }
    if report.dropped_workers > 0 {
        writeln!(
            io.out(),
            "dropped {} unassignable worker row(s): {}",
            report.dropped_workers,
            report.dropped_worker_rows.join(", ")
        )?;
    }
    if !report.primary_runs.is_empty() {
        writeln!(
            io.out(),
            "job-less runs moved to the primary project: {}",
            report.primary_runs.len()
        )?;
    }
    for warning in &report.warnings {
        writeln!(io.out(), "warning: {warning}")?;
    }
    if report.dry_run {
        writeln!(io.out(), "nothing was written")?;
    }
    Ok(())
}
