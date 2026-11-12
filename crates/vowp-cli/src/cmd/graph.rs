//! `vowp graph`: ASCII, Mermaid, JSON, YAML export and YAML import.
//!
//! Spec: `docs/design/cli.md` §3 and `docs/design/crates/vowp-graph.md` §6–7. Rendering is
//! exactly `vowp_graph::render_ascii`/`render_mermaid`; export and import use the stable YAML
//! format and never overwrite an existing job.

use std::collections::BTreeSet;

use serde::Serialize;
use vowp_graph::{Graph, export_yaml, import_yaml, render_ascii, render_mermaid};
use vowp_protocol::{Edge, Job, JobId};
use vowp_store::JobFilter;

use crate::cli::GraphArgs;
use crate::{CliError, Ctx, IoStreams};

/// JSON shape for `vowp graph --import --json`.
#[derive(Debug, Serialize)]
struct ImportOutput {
    imported: Vec<String>,
    conflicts: Vec<String>,
}

/// JSON shape for `vowp graph --json`.
///
/// Jobs are ordered by id and serialised exactly like the `Job` objects of
/// `vowp job ls --json`; edges keep the store/graph insertion order.
#[derive(Debug, Serialize)]
struct GraphOutput<'a> {
    jobs: Vec<&'a Job>,
    edges: &'a [Edge],
}

/// Runs `vowp graph`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &GraphArgs) -> Result<i32, CliError> {
    if let Some(path) = &args.import {
        return import(ctx, io, path, args.all);
    }
    let graph = load_graph(ctx, args.all)?;

    if let Some(path) = &args.export {
        let yaml = export_yaml(&graph)?;
        if path == "-" {
            write!(io.out(), "{yaml}")?;
        } else {
            std::fs::write(path, yaml)?;
            if ctx.json() {
                crate::output::print_json(io, &serde_json::json!({ "exported": path }))?;
            } else {
                writeln!(io.out(), "exported graph to {path}")?;
            }
        }
        return Ok(0);
    }

    if args.mermaid {
        write!(io.out(), "{}", render_mermaid(&graph))?;
    } else if ctx.json() {
        let output = GraphOutput {
            jobs: graph.jobs().collect(),
            edges: graph.edges(),
        };
        crate::output::print_json(io, &output)?;
    } else {
        write!(io.out(), "{}", render_ascii(&graph))?;
    }
    Ok(0)
}

/// Loads the graph for one project, or every project with `--all`.
fn load_graph(ctx: &Ctx, all: bool) -> Result<Graph, CliError> {
    let store = ctx.store()?;
    let project = if all { None } else { Some(ctx.project().clone()) };
    let jobs = store.list_jobs(&JobFilter {
        project: project.clone(),
        ..JobFilter::default()
    })?;
    let edges = store.list_edges(project.as_ref())?;
    Ok(Graph::new(jobs, edges)?)
}

/// Validates a YAML file and inserts the missing jobs and edges.
///
/// Export sorts jobs by id, so a dependent may appear before the job it needs. Jobs are
/// therefore inserted in dependency order: an import of any valid exported graph works.
fn import(ctx: &Ctx, io: &mut dyn IoStreams, path: &std::path::Path, _all: bool) -> Result<i32, CliError> {
    let text = std::fs::read_to_string(path)?;
    let imported = import_yaml(&text)?;
    let store = ctx.store()?;
    let mut inserted = Vec::new();
    let mut conflicts = BTreeSet::new();

    let mut available: BTreeSet<JobId> = store
        .list_jobs(&JobFilter {
            project: Some(ctx.project().clone()),
            ..JobFilter::default()
        })?
        .into_iter()
        .map(|job| job.id)
        .collect();
    let mut pending = imported.jobs;
    while !pending.is_empty() {
        let mut remaining = Vec::new();
        let mut progress = false;
        for job in pending {
            if available.contains(&job.id) {
                conflicts.insert(job.id.to_string());
                continue;
            }
            if !job.needs.iter().all(|need| available.contains(need)) {
                remaining.push(job);
                continue;
            }
            progress = true;
            let brief = job.brief_text.clone();
            let stored = store.insert_job(ctx.project(), &job, "cli")?;
            if let Some(body) = brief {
                let brief_path = ctx.project_root().join(&stored.brief);
                if let Some(parent) = brief_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&brief_path, body)?;
            }
            available.insert(stored.id.clone());
            inserted.push(stored.id.to_string());
        }
        if !progress && !remaining.is_empty() {
            return Err(CliError::Invalid(
                "cannot order imported jobs: unresolved dependencies".to_owned(),
            ));
        }
        pending = remaining;
    }

    for edge in imported.edges {
        if store.job(&edge.src)?.is_none() || store.job(&edge.dst)?.is_none() {
            conflicts.insert(format!("{} -> {}", edge.src, edge.dst));
            continue;
        }
        store.add_edge(&edge)?;
    }

    let conflicts: Vec<String> = conflicts.into_iter().collect();
    if ctx.json() {
        crate::output::print_json(
            io,
            &ImportOutput {
                imported: inserted,
                conflicts: conflicts.clone(),
            },
        )?;
    } else {
        writeln!(
            io.out(),
            "imported {} jobs; {} conflicts",
            inserted.len(),
            conflicts.len()
        )?;
        for conflict in &conflicts {
            writeln!(io.out(), "conflict: {conflict}")?;
        }
    }
    Ok(if conflicts.is_empty() { 0 } else { 4 })
}
