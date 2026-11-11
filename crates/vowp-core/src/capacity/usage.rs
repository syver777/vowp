//! Per-run usage rows: validating, storing and importing adapter measurements.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1. A finished run is measured either by
//! [`super::scope::RunScope`] or by the adapter process meter, which leaves a small JSON sidecar
//! beside the raw run log; the sampler drains those sidecars on its next tick.

use std::path::Path;

use vowp_adapters::proc::ProcUsage;
use vowp_protocol::{JobKind, ProjectName, Run, RunUsage, Timestamp, UsageSource};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

use super::scope::{DEFAULT_MEAN_CPU_CORES, DEFAULT_PEAK_RSS_BYTES, infer_build_mode};

/// Stores one finished run's measured usage.
pub fn record(store: &Store, usage: &RunUsage) -> Result<(), CoreError> {
    store.upsert_run_usage(usage)?;
    Ok(())
}

/// Builds the conservative default row when no measurement was possible.
pub fn default_for(run: &Run, project: &ProjectName, job_kind: Option<JobKind>, ended_at: Timestamp) -> RunUsage {
    let duration_ms = u64::try_from(ended_at.since(run.started_at).as_millis()).unwrap_or(u64::MAX);
    RunUsage {
        run_id: run.id.clone(),
        project: project.clone(),
        job_id: run.job_id.clone(),
        job_kind,
        role: run.role,
        harness: run.executor.as_ref().map(|executor| executor.harness),
        model: run.executor.as_ref().map(|executor| executor.model.clone()),
        provider: run.provider.clone(),
        node: run.node.clone(),
        started_at: run.started_at,
        ended_at,
        duration_ms,
        peak_rss_bytes: DEFAULT_PEAK_RSS_BYTES,
        anon_peak_bytes: None,
        file_peak_bytes: None,
        kernel_peak_bytes: None,
        cpu_usec: 0,
        mean_cpu_cores: DEFAULT_MEAN_CPU_CORES,
        max_cpu_cores: DEFAULT_MEAN_CPU_CORES,
        build_mode: vowp_protocol::BuildMode::Unknown,
        source: UsageSource::Default,
        oom_killed: false,
        exit_code: run.exit_code,
    }
}

/// Imports every adapter usage sidecar for a run that now exists in a project database.
///
/// Returns the number of rows written. A sidecar whose run row is not visible yet is left in
/// place for the next tick; a sidecar that cannot be parsed is renamed to `.bad` so a corrupt
/// file cannot make every sample retry it forever.
pub fn import_sidecars(deps: &Deps, store: &Store) -> Result<u32, CoreError> {
    let dir = deps.paths.runs_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(CoreError::Io {
                path: dir.display().to_string(),
                source,
            });
        }
    };
    let mut imported = 0_u32;
    for entry in entries {
        let entry = entry.map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".usage.json"))
        {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let sidecar: ProcUsage = match serde_json::from_str(&text) {
            Ok(record) => record,
            Err(error) => {
                deps.log.log(
                    vowp_protocol::Level::Warn,
                    "capacity",
                    &format!("ignoring unreadable usage sidecar {}: {error}", path.display()),
                );
                let bad = path.with_extension("usage.bad");
                let _ = std::fs::rename(&path, bad);
                continue;
            }
        };
        let Some(FoundRun {
            run,
            project,
            job_kind,
            job_worktree,
        }) = find_run(store, deps, &sidecar.run_id)?
        else {
            continue;
        };
        let build_mode = if sidecar.build_mode == vowp_protocol::BuildMode::Unknown {
            // The adapter never sees the job's real target directory when a shim or an external
            // CARGO_TARGET_DIR is used. Infer from the job worktree, never the session cwd; when
            // there is no job worktree the honest answer is `unknown`.
            job_worktree
                .as_deref()
                .map(Path::new)
                .map(infer_build_mode)
                .unwrap_or(vowp_protocol::BuildMode::Unknown)
        } else {
            sidecar.build_mode
        };
        let usage = RunUsage {
            run_id: run.id.clone(),
            project,
            job_id: run.job_id.clone(),
            job_kind,
            role: run.role,
            harness: run.executor.as_ref().map(|executor| executor.harness),
            model: run.executor.as_ref().map(|executor| executor.model.clone()),
            provider: run.provider.clone(),
            node: run.node.clone(),
            started_at: sidecar.started_at,
            ended_at: sidecar.ended_at,
            duration_ms: sidecar.duration_ms,
            peak_rss_bytes: sidecar.peak_rss_bytes.max(1),
            anon_peak_bytes: sidecar.anon_peak_bytes,
            file_peak_bytes: sidecar.file_peak_bytes,
            kernel_peak_bytes: sidecar.kernel_peak_bytes,
            cpu_usec: sidecar.cpu_usec,
            mean_cpu_cores: sidecar.mean_cpu_cores,
            max_cpu_cores: sidecar.max_cpu_cores,
            build_mode,
            source: sidecar.source,
            oom_killed: sidecar.oom_killed,
            exit_code: sidecar.exit_code.or(run.exit_code),
        };
        record(store, &usage)?;
        let _ = std::fs::remove_file(&path);
        imported = imported.saturating_add(1);
    }
    Ok(imported)
}

/// A run row plus the job data that sidecar import needs.
struct FoundRun {
    run: Run,
    project: ProjectName,
    job_kind: Option<JobKind>,
    job_worktree: Option<String>,
}

/// Finds a run row, its project, job kind and job worktree, searching the bound project first.
fn find_run(store: &Store, deps: &Deps, run_id: &str) -> Result<Option<FoundRun>, CoreError> {
    let id = match vowp_protocol::RunId::parse(run_id) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    if let Some(project) = store.bound_project() {
        if let Some(run) = store.run(&id)? {
            let (job_kind, job_worktree) = job_kind_and_worktree(store, &run)?;
            return Ok(Some(FoundRun {
                run,
                project: project.clone(),
                job_kind,
                job_worktree,
            }));
        }
    }
    for project in store.list_projects()? {
        if store.bound_project() == Some(&project.name) {
            continue;
        }
        let project_store = match Store::open_with_project(&deps.paths, &project.name) {
            Ok(project_store) => project_store.with_clock(deps.clock.clone()),
            Err(_) => continue,
        };
        if let Some(run) = project_store.run(&id)? {
            let (job_kind, job_worktree) = job_kind_and_worktree(&project_store, &run)?;
            return Ok(Some(FoundRun {
                run,
                project: project.name,
                job_kind,
                job_worktree,
            }));
        }
    }
    Ok(None)
}

/// Returns a run's job kind and worktree, when the run belongs to a job.
fn job_kind_and_worktree(store: &Store, run: &Run) -> Result<(Option<JobKind>, Option<String>), CoreError> {
    let Some(job_id) = &run.job_id else {
        return Ok((None, None));
    };
    let Some(job) = store.job(job_id)? else {
        return Ok((None, None));
    };
    Ok((Some(job.kind), job.worktree))
}
