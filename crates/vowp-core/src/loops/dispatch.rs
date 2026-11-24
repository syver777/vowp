//! Dispatch helpers: worktrees, briefs, child jobs and the Start-vs-Resume decision.
//!
//! Spec: `docs/design/loops.md` §1.1 step 8 and §5.1. `plan_dispatch` itself lives in
//! `vowp-graph`; this module performs the store and filesystem side of one choice.

use std::path::{Path, PathBuf};

use vowp_protocol::{Brief, Edge, EdgeKind, Gate, Job, JobId, JobKind, NewJob, ReportGate, Role, RunPurpose};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

/// Returns the kv key holding a job's explicit executor override.
pub fn job_executor_key(job: &JobId) -> String {
    format!("job_executor:{job}")
}

/// Reads a job's explicit executor override (`vowp job set-executor`), when one is set.
///
/// An unparsable stored value is ignored so a hand-edited row cannot block dispatch.
pub fn job_executor_override(store: &Store, job: &JobId) -> Result<Option<vowp_protocol::Executor>, CoreError> {
    Ok(store
        .kv_get(&job_executor_key(job))?
        .and_then(|value| vowp_protocol::Executor::parse(&value).ok()))
}

/// Returns false when the job must wait; writes the waiting reason to `jobs.note`.
///
/// The body builds the node-wide admission request, asks the capacity manager, records
/// `waiting: <reason>` on the job so the board and queue show it, and clears a stale waiting
/// note once the job is admitted.
pub fn admit(deps: &Deps, store: &Store, job: &Job) -> Result<bool, CoreError> {
    let request = crate::capacity::request_for(deps, store, job)?;
    let manager = crate::capacity::CapacityManager::new(deps.clone());
    match manager.admit(store, &request, deps.clock.now())? {
        vowp_protocol::AdmitDecision::Admit => {
            let note = store.require_job(&job.id)?.note;
            if note.trim_start().starts_with("waiting:") {
                store.set_job_note(&job.id, "")?;
            }
            Ok(true)
        }
        vowp_protocol::AdmitDecision::Wait(reason) => {
            store.set_job_note(&job.id, &format!("waiting: {}", reason.detail))?;
            Ok(false)
        }
    }
}

/// Decides Start versus Resume using the store.
///
/// `vowp_graph::transition` only sees `job.continues`; a Ready job with a prior run or
/// session must continue on its stored native session even when the effect says `Start`.
pub(crate) fn purpose_for_dispatch(store: &Store, job: &Job) -> Result<RunPurpose, CoreError> {
    if matches!(job.kind, JobKind::Fix | JobKind::Merge) {
        return Ok(RunPurpose::Fix);
    }
    if job.continues > 0
        || super::latest_session(store, &job.id)?.is_some()
        || store.last_run_of_job(&job.id)?.is_some()
    {
        Ok(RunPurpose::Continue)
    } else {
        Ok(RunPurpose::Start)
    }
}

/// Creates the job worktree (or reuses the parent's for verify/fix/merge) and records it.
pub(crate) fn ensure_worktree(deps: &Deps, store: &Store, job: &Job, repo: &Path) -> Result<Job, CoreError> {
    if let (Some(branch), Some(worktree)) = (&job.branch, &job.worktree) {
        if Path::new(worktree).is_dir() {
            return Ok(job.clone());
        }
        deps.git
            .worktree_add(repo, Path::new(worktree), branch, super::default_branch(deps))?;
        return Ok(job.clone());
    }

    if matches!(job.kind, JobKind::Verify | JobKind::Fix | JobKind::Merge) {
        if let Some((parent, worktree, branch)) = parent_workspace(store, job)? {
            let _ = parent;
            store.set_job_workspace(&job.id, Some(&branch), Some(&worktree))?;
            return Ok(store.require_job(&job.id)?);
        }
    }

    let root = super::worktree_root(deps, repo);
    let path = root.join(job.id.as_str());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let branch = job.branch.clone().unwrap_or_else(|| format!("job/{}", job.id.as_str()));
    deps.git
        .worktree_add(repo, &path, &branch, super::default_branch(deps))?;
    store.set_job_workspace(&job.id, Some(&branch), Some(&path.to_string_lossy()))?;
    Ok(store.require_job(&job.id)?)
}

/// Returns the parent worktree/branch through a fixes or verifies edge.
fn parent_workspace(store: &Store, job: &Job) -> Result<Option<(JobId, String, String)>, CoreError> {
    for edge in store.edges_of(&job.id)? {
        if edge.src != job.id {
            continue;
        }
        if !matches!(edge.kind, EdgeKind::Fixes | EdgeKind::Verifies) {
            continue;
        }
        let parent = store.require_job(&edge.dst)?;
        if let (Some(worktree), Some(branch)) = (parent.worktree, parent.branch) {
            return Ok(Some((parent.id, worktree, branch)));
        }
    }
    Ok(None)
}

/// Writes the job brief if the worktree does not already have one.
///
/// Precedence:
/// 1. an existing worktree brief is never overwritten;
/// 2. otherwise the project-checkout copy at `<repo>/<job.brief>` is copied byte-for-byte,
///    generated front matter included;
/// 3. otherwise a generated one-line fallback is written.
///
/// `repo` is the project's main checkout, passed by the caller (`Supervisor::start_run`
/// already resolves it for the worktree).
pub(crate) fn write_brief(job: &Job, worktree: &Path, repo: &Path) -> Result<(), CoreError> {
    let path = worktree.join(&job.brief);
    if path.is_file() {
        return Ok(());
    }
    let source = repo.join(&job.brief);
    if source.is_file() {
        ensure_parent(&path)?;
        std::fs::copy(&source, &path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
        return Ok(());
    }
    write_generated_brief(job, &path)
}

/// Writes the generated one-line fallback when the brief path is missing.
fn write_generated_brief(job: &Job, path: &Path) -> Result<(), CoreError> {
    if path.is_file() {
        return Ok(());
    }
    ensure_parent(path)?;
    let body = format!(
        "Do job {job}. Read AGENTS.md first, change only the paths in `owns`, commit on the job branch, run the gates, and write the report at {report}.\n",
        job = job.id,
        report = job.report,
    );
    let brief = Brief::from_job(job, &body);
    std::fs::write(path, brief.render()).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Creates the brief's parent directory when it is missing.
fn ensure_parent(path: &Path) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Creates the fix or merge child job of `parent`.
///
/// The child shares the parent's worktree and branch and owns the same paths. Its own gate is
/// the report gate; the parent re-runs its command gates after the child returns (T22), per
/// loops.md §2 and D5/D17.
pub(crate) fn create_child_job(
    deps: &Deps,
    store: &Store,
    parent: &Job,
    kind: JobKind,
    findings: &[String],
) -> Result<Job, CoreError> {
    // A retry can reset the parent's attempt counter, so the natural child id may already
    // name a finished fix. Reusing that row would make the supervisor send NeedsSatisfied to
    // a terminal job. Keep any live child, but allocate a fresh suffix after a terminal one
    // so this round gets its own instructions and run.
    let base = super::l2_worker::child_id(parent, kind)?;
    let mut id = JobId::parse(&base)?;
    let mut suffix = 1u32;
    loop {
        match store.job(&id)? {
            None => break,
            Some(existing) if !existing.state.is_terminal() => return Ok(existing),
            Some(_) => {
                suffix += 1;
                id = JobId::parse(&format!("{base}-r{suffix}"))?;
            }
        }
    }
    // A forced-gating parent may not have a worktree yet; never write the child brief into
    // the supervisor's current directory (that would dirty the source tree).
    let worktree = match &parent.worktree {
        Some(worktree) => PathBuf::from(worktree),
        None => super::main_repo(deps, store, &parent.project)?,
    };
    let report = format!(".vowp/jobs/{id}/report.md");
    // The fix/merge child's own gate is its report; the parent re-runs its full command
    // gates when the child returns (T22). Re-running them on the child too would let one
    // failing cheap gate kill the parent before the loop can spend its next round.
    let gates = vec![Gate::Report(ReportGate::default())];
    let mut body = format!(
        "Job {parent} failed its checks (round {attempt} of {max}). Findings:\n",
        parent = parent.id,
        attempt = parent.attempts.max(1),
        max = parent.loop_.max_attempts.max(1),
    );
    if findings.is_empty() {
        body.push_str("1. (no findings recorded)\n");
    } else {
        for (index, finding) in findings.iter().enumerate() {
            body.push_str(&format!("{}. {}\n", index + 1, finding));
        }
    }
    if kind == JobKind::Merge {
        body.push_str(
            "Run `git rebase origin/main`, resolve the listed conflicts, `git add` the files, `git rebase --continue`, rerun the gates, commit, and update the report. Do not widen scope.\n",
        );
    }
    body.push_str(&format!(
        "Fix exactly these problems in the same worktree, re-run the failing commands, commit, and update {report}. Do not widen scope."
    ));
    if let Some(note) = super::l2_worker::failing_report_note(findings, &report) {
        body.push('\n');
        body.push_str(&note);
    }

    let mut owns = parent.owns.clone();
    if kind == JobKind::Merge {
        for finding in findings {
            let Some(rest) = finding.strip_prefix("rebase conflict in ") else {
                continue;
            };
            let path = rest.split(';').next().unwrap_or(rest).trim();
            if !path.is_empty() && !owns.iter().any(|own| own == path) {
                owns.push(path.to_owned());
            }
        }
    }
    let new = NewJob {
        id: id.clone(),
        title: format!("{kind:?} {}", parent.id),
        kind,
        role: parent.role,
        profile: Some(parent.profile.clone()),
        needs: Vec::new(),
        owns,
        reads: parent.reads.clone(),
        gates: Some(gates),
        priority: Some(parent.priority),
        loop_: Some(parent.loop_.clone()),
        brief_text: Some(body.clone()),
    };
    let child = store.insert_job(&parent.project, &new, "l1")?;
    // The child's round is the parent's attempt: the fix prompt, the gates attempt and the
    // child id all use the same number.
    store.set_job_counters(&child.id, Some(parent.attempts.max(1)), None, None)?;
    if !findings.is_empty() {
        store.set_job_note(&child.id, &findings.join("\n"))?;
    }
    store.add_edge(&Edge {
        src: child.id.clone(),
        dst: parent.id.clone(),
        kind: EdgeKind::Fixes,
    })?;
    store.set_job_workspace(&child.id, parent.branch.as_deref(), parent.worktree.as_deref())?;
    let child = store.require_job(&child.id)?;
    let brief = Brief::from_job(&child, &body);
    let brief_path = worktree.join(&child.brief);
    if let Some(dir) = brief_path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
    }
    std::fs::write(&brief_path, brief.render()).map_err(|source| CoreError::Io {
        path: brief_path.display().to_string(),
        source,
    })?;
    let _ = deps;
    Ok(child)
}

/// Creates the decision job for an exhausted `parent` and blocks its dependents.
pub(crate) fn create_decision_job(_deps: &Deps, store: &Store, parent: &Job) -> Result<Job, CoreError> {
    let id = JobId::parse(&format!("{}-decision", parent.id))?;
    if let Some(existing) = store.job(&id)? {
        return Ok(existing);
    }
    let new = NewJob {
        id: id.clone(),
        title: format!("decision for {}", parent.id),
        kind: JobKind::Decision,
        role: Role::Worker,
        profile: Some("worker".to_owned()),
        needs: Vec::new(),
        owns: parent.owns.clone(),
        reads: Vec::new(),
        gates: Some(Vec::new()),
        priority: Some(parent.priority),
        loop_: Some(parent.loop_.clone()),
        brief_text: Some(format!(
            "Decide what to do about job {}: retry, replan or cancel it.\n",
            parent.id
        )),
    };
    let decision = store.insert_job(&parent.project, &new, "l1")?;
    store.add_edge(&Edge {
        src: decision.id.clone(),
        dst: parent.id.clone(),
        kind: EdgeKind::Escalates,
    })?;
    for dependent in store.dependents_of(&parent.id)? {
        store.add_edge(&Edge {
            src: decision.id.clone(),
            dst: dependent,
            kind: EdgeKind::Blocks,
        })?;
    }
    if let Some(worktree) = &parent.worktree {
        let path = PathBuf::from(worktree);
        if path.is_dir() {
            // A decision job has no planner-written project copy in hand, so it keeps the
            // generated fallback rather than consulting the parent's project checkout.
            write_generated_brief(&decision, &path.join(&decision.brief))?;
        }
    }
    Ok(store.require_job(&decision.id)?)
}

/// Formats one failed gate result as a fix-prompt finding.
///
/// A report-gate finding names the failing job and its report path, so a fix worker knows
/// which file failed even though its own report is a different file.
pub(crate) fn gate_finding(job: &Job, result: &vowp_protocol::GateResult) -> String {
    gate_finding_for(job.id.as_str(), &job.report, result)
}

/// Formats a gate result for the job `job_id` whose report lives at `report`.
fn gate_finding_for(job_id: &str, report: &str, result: &vowp_protocol::GateResult) -> String {
    use vowp_protocol::Evidence;
    match &result.evidence {
        Evidence::Cmd {
            command,
            exit_code,
            tail,
            ..
        }
        | Evidence::Script {
            command,
            exit_code,
            tail,
            ..
        } => {
            let last = tail
                .lines()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            format!("gate {} ({command}) exited {exit_code:?}:\n{last}", result.gate_index)
        }
        Evidence::Report {
            problems,
            missing_fields,
            ..
        } => {
            let mut parts = problems.clone();
            if !missing_fields.is_empty() {
                parts.push(format!("missing fields: {}", missing_fields.join(", ")));
            }
            format!(
                "gate {} (report) for {job_id} at {report}: {}",
                result.gate_index,
                parts.join("; ")
            )
        }
        Evidence::OwnsRespected { violations, .. } => format!(
            "gate {} (owns_respected): changed outside owns: {}",
            result.gate_index,
            violations.join(", ")
        ),
        Evidence::Pushed { remote, branch, .. } => {
            format!(
                "gate {} (pushed): {remote}/{branch} is not the local head",
                result.gate_index
            )
        }
        Evidence::Review { findings, .. } => {
            format!("gate {} (review): {}", result.gate_index, findings.join("; "))
        }
        Evidence::Deploy { command, exit_code, .. } => {
            format!("gate {} (deploy {command}) exited {exit_code:?}", result.gate_index)
        }
        Evidence::SecretScan { hits, .. } => format!(
            "gate {} (secret_scan): {} hit(s): {}",
            result.gate_index,
            hits.len(),
            hits.iter()
                .map(|hit| format!("{}:{} {}", hit.path, hit.line, hit.rule))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vowp_protocol::{Evidence, GateResult, Timestamp};

    fn report_failure() -> GateResult {
        GateResult {
            id: None,
            job_id: JobId::parse("parent-job").unwrap(),
            run_id: None,
            attempt: 1,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: false,
            evidence: Evidence::Report {
                front_matter: None,
                missing_fields: vec!["commits".into(), "tests".into()],
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 0,
            ts: Timestamp::from_unix_secs(0),
        }
    }

    #[test]
    fn report_finding_names_job_and_report_path() {
        let path = ".vowp/jobs/parent-job/report.md";
        let finding = gate_finding_for("parent-job", path, &report_failure());
        assert_eq!(
            finding,
            "gate 0 (report) for parent-job at .vowp/jobs/parent-job/report.md: missing fields: commits, tests"
        );
    }

    #[test]
    fn fix_brief_tells_worker_to_edit_failing_report_and_own() {
        let path = ".vowp/jobs/parent-job/report.md";
        let findings = vec![gate_finding_for("parent-job", path, &report_failure())];
        let own = ".vowp/jobs/parent-job-fix-1/report.md";
        let note =
            super::super::l2_worker::failing_report_note(&findings, own).expect("a report finding yields a note");
        assert!(note.contains(path), "{note}");
        assert!(note.contains("edit"), "{note}");
        assert!(note.contains(own), "{note}");
        let none = super::super::l2_worker::failing_report_note(&["gate 1 (cargo test) exited Some(1):".into()], own);
        assert!(none.is_none());
    }
}
