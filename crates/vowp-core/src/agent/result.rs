//! Result return for offloaded runs: fetch or receive, verify, gate and hand the job to the
//! merge queue.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3 step 7, §6.4, §6.5 and §13, and
//! `docs/design/v0.4/hardening.md` §5.3–§5.5 for the git-bundle transport.
//!
//! A `push` assignment: the node pushes `job/<id>` with its own git credentials and reports
//! the head commit in a `RunState` frame and the terminal `TurnEnd` summary. The home half
//! fetches the project's configured remote (never a credential from the agent), checks the
//! fetched branch head against the reported head and against the assigned base commit,
//! checks out a staging worktree outside the main checkout, runs the authoritative
//! `owns_respected` and `secret_scan` gates on the `base..head` diff, and then moves the job
//! to the normal `gating` path.
//!
//! A `bundle` assignment: the node sends `base..job/<id>` over the authenticated agent
//! channel as `BundleStart`/`BundleChunk`/`BundleEnd`. The home stages the bytes under its
//! own `tmp` directory while hashing them, verifies size and sha256, runs `git bundle
//! verify` and the exact one-ref check, fetches the one ref into a private staging bare
//! repository that already has the base, checks the head and its ancestry, imports the
//! objects as `refs/vowp/jobs/<id>`, and then runs the same branch/gate path.
//!
//! It never merges, tags, deploys or pushes the default branch: the merge queue remains the
//! only writer of `main`. Every terminal summary of an offloaded run settles the assignment
//! and run lease, even when the result is rejected. A rejection keeps the branch for
//! inspection, marks the run failed, and leaves the job retryable; a gate rejection also
//! removes its staging worktree so a retry can move the same `job/<id>` branch to the next
//! returned head.
//!
//! The project `runs` table has no dedicated transport/base/head columns in this phase. The
//! same facts are recorded in the run's redacted `detail` field and in one small `kv` record
//! per run (see [`AgentRunResult`]); no schema change is needed. Bundle bytes are never
//! logged, mirrored or appended to a session event, and the durable record stays path-free.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vowp_graph::{JobEvent, TransitionBounds};
use vowp_protocol::{
    Evidence, Gate, JobState, LedgerEntry, LedgerKind, Level, ProjectName, ResultTransport, Run, RunId, RunOutcome,
    Timestamp,
};
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState};
use vowp_store::{StateChange, Store};

use super::dispatch;
use crate::CoreError;
use crate::deps::{AdapterRegistry, CommandSpec, Deps};
use crate::gates::{GateCtx, GateRunner};
use crate::ledger::Ledger;

/// Prefix of the durable per-run result record in the store's `kv` table.
pub const RESULT_KEY_PREFIX: &str = "agent-result:";

/// Largest accepted bundle, in bytes (256 MiB).
pub const BUNDLE_MAX_BYTES: u64 = vowp_protocol::MAX_BUNDLE_BYTES;

/// Transfer deadline for one bundle, measured from `BundleStart` (5 minutes).
pub const BUNDLE_TRANSFER_DEADLINE: Duration = Duration::from_secs(300);

/// Directory below the home root that holds one private staging directory per run.
const BUNDLE_ROOT: &str = "tmp/agent-bundles";

/// Name of the sidecar record in one run's staging directory.
const BUNDLE_MANIFEST: &str = "manifest.json";

/// Name of the private bare repository used to verify a fetched head.
const BUNDLE_STAGING_REPO: &str = "staging.git";

/// Timeout for the small `git` reads and ref updates this module runs directly.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Most failure detail characters kept in the result record and the run detail.
const DETAIL_CAP: usize = 1200;

/// The durable, path-free result record for one agent run.
///
/// It mirrors the transport facts §6.3 asks the run row to carry. The record lives in the
/// existing `kv` table under [`RESULT_KEY_PREFIX`] plus the run id, so the store schema is
/// unchanged. `head_commit` is written as soon as the node reports one and is therefore the
/// fallback when a reconnecting summary arrives without an in-memory `RunState`. A bundle
/// result adds its size and digest; the staging file name and path never enter the record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRunResult {
    /// Result transport requested by the assignment: `push`, `bundle` or `none`.
    pub transport: String,
    /// Base commit the assignment started from.
    pub base_commit: String,
    /// Head commit the node reported, when it reported one.
    pub head_commit: Option<String>,
    /// Size of the received bundle, when the transport is `bundle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_bytes: Option<u64>,
    /// Lowercase hex SHA-256 of the received bundle, when the transport is `bundle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_sha256: Option<String>,
    /// Last spool size the node reported.
    pub spool_bytes: u64,
    /// `reported`, `received`, `returned` or `failed`.
    pub status: String,
    /// Short, path-free explanation of the last transition.
    pub detail: String,
}

/// The terminal outcome and detail a summary should use after result return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// Outcome to write to the run row.
    pub outcome: RunOutcome,
    /// Redacted, path-free detail to write to the run row.
    pub detail: String,
}

/// Returns the `kv` key of one run's result record.
pub fn result_key(run: &RunId) -> String {
    format!("{RESULT_KEY_PREFIX}{run}")
}

/// Loads one run's durable result record, when it has one.
pub fn load(store: &Store, run: &RunId) -> Result<Option<AgentRunResult>, CoreError> {
    let Some(text) = store.kv_get(&result_key(run))? else {
        return Ok(None);
    };
    let record = serde_json::from_str(&text)
        .map_err(|error| CoreError::Invalid(format!("invalid agent result record: {error}")))?;
    Ok(Some(record))
}

/// Writes one run's durable result record.
fn save(store: &Store, run: &RunId, record: &AgentRunResult) -> Result<(), CoreError> {
    let text = serde_json::to_string(record)
        .map_err(|error| CoreError::Command(format!("cannot encode agent result record: {error}")))?;
    store.kv_set(&result_key(run), &text)?;
    Ok(())
}

/// Opens the store bound to `project`, unless the caller's handle already is.
///
/// The gateway may serve a multi-project home with the global database; a run's `runs`,
/// `jobs`, `gate_results` and ledger rows live in the owning project's database. An
/// in-memory test store has no node paths and keeps the caller's handle.
fn project_store(deps: &Deps, store: &Store, project: &ProjectName) -> Result<Option<Store>, CoreError> {
    if store.bound_project() == Some(project) {
        return Ok(None);
    }
    let Some(paths) = store.paths() else {
        return Ok(None);
    };
    Store::create_project_db(paths, project)?;
    Ok(Some(
        Store::open_with_project(paths, project)?.with_clock(deps.clock.clone()),
    ))
}

/// Builds the project-scoped dependencies result handling must use.
///
/// A gateway on a multi-project home has no single selected project, so the node-level
/// config cannot name the checkout, remote or gate defaults. The checkout's `vowp.toml`
/// is authoritative, exactly as the supervisor builds a project's `Deps`.
fn project_deps(deps: &Deps, project: &vowp_protocol::Project) -> Deps {
    let mut config = dispatch::project_config(&deps.config_now(), project);
    if config.project.path.is_none() {
        config.project.path = Some(project.path.clone());
    }
    config.project.name = project.name.clone();
    let adapters = AdapterRegistry::from_config(&config)
        .map(Arc::new)
        .unwrap_or_else(|_| deps.adapters.clone());
    let mut project_deps = deps.clone();
    project_deps.config = Arc::new(config);
    project_deps.adapters = adapters;
    project_deps
}

/// Records the transport facts of one `RunState` frame.
///
/// Called by the dispatcher; a terminal record is never downgraded to `reported`.
pub(crate) fn record_reported(
    store: &Store,
    assignment: &AgentAssignment,
    head_commit: Option<&str>,
    spool_bytes: u64,
) -> Result<(), CoreError> {
    let mut record = load(store, &assignment.run_id)?.unwrap_or_default();
    if matches!(record.status.as_str(), "returned" | "failed" | "received") {
        return Ok(());
    }
    record.transport = transport_name(assignment);
    record.base_commit = assignment.payload.base_commit.clone();
    if let Some(head) = head_commit.map(str::trim).filter(|head| !head.is_empty()) {
        record.head_commit = Some(head.to_ascii_lowercase());
    }
    record.spool_bytes = spool_bytes;
    record.status = "reported".to_owned();
    save(store, &assignment.run_id, &record)
}

/// One staged bundle's sidecar record; it names no absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BundleManifest {
    /// Run the transfer belongs to.
    run_id: String,
    /// Assignment epoch.
    epoch: u64,
    /// Assigned base commit.
    base: String,
    /// Committed head the bundle returns.
    head: String,
    /// The bundle's single branch, `job/<id>`.
    branch: String,
    /// Declared bundle size.
    bytes: u64,
    /// Declared lowercase hex SHA-256.
    sha256: String,
    /// When the transfer started; the deadline is measured from here.
    started_at: Timestamp,
}

/// Answers one `BundleStart`: the offset to resume from, or a stable refusal reason.
///
/// A same-`(run_id, epoch, sha256, bytes)` start resumes at the stored length, a higher
/// epoch supersedes the old partial, and a lower epoch is refused before any write. An
/// oversized start is refused before the staging file exists.
#[allow(clippy::too_many_arguments)]
pub fn start_bundle(
    deps: &Deps,
    store: &Store,
    node: &vowp_protocol::NodeName,
    run_id: &RunId,
    epoch: u64,
    base: &str,
    head: &str,
    branch: &str,
    bytes: u64,
    sha256: &str,
    now: Timestamp,
) -> Result<u64, String> {
    let head = head.trim();
    let branch = branch.trim();
    let sha256 = sha256.trim();
    let assignment = bundle_assignment(store, node, run_id, epoch)?;
    let job = &assignment.payload.job;
    let expected_branch = format!("job/{job}");
    if branch != expected_branch {
        return Err("bundle_rejected: the bundle branch is not the assigned job branch".to_owned());
    }
    let assigned_base = assignment.payload.base_commit.trim().to_ascii_lowercase();
    if !is_commit_id(&assigned_base) || base.trim().to_ascii_lowercase() != assigned_base {
        return Err("bundle_rejected: the bundle base is not the assigned base".to_owned());
    }
    if !is_commit_id(head) {
        return Err("bundle_rejected: the bundle head is not a commit id".to_owned());
    }
    if head.eq_ignore_ascii_case(&assigned_base) {
        return Err("result_empty: the head equals the assigned base".to_owned());
    }
    if bytes == 0 {
        return Err("bundle_rejected: the bundle is empty".to_owned());
    }
    if bytes > BUNDLE_MAX_BYTES {
        return Err(format!(
            "bundle_failed: the bundle is over the {BUNDLE_MAX_BYTES}-byte cap"
        ));
    }
    if !is_sha256(sha256) {
        return Err("bundle_rejected: the bundle digest is not lowercase hex sha256".to_owned());
    }

    let dir = run_dir(deps, run_id);
    let manifest_path = dir.join(BUNDLE_MANIFEST);
    if manifest_path.exists() {
        let Some(old) = read_manifest(&dir) else {
            discard_dir(&dir);
            return stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now);
        };
        if epoch < old.epoch {
            return Err("bundle_rejected: a newer transfer epoch is already staged".to_owned());
        }
        let same = old.epoch == epoch
            && old.sha256.eq_ignore_ascii_case(sha256)
            && old.bytes == bytes
            && old.head.eq_ignore_ascii_case(head)
            && old.branch == branch
            && old.base.eq_ignore_ascii_case(&assigned_base);
        if same {
            if now.since(old.started_at) >= BUNDLE_TRANSFER_DEADLINE {
                discard_dir(&dir);
                return stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now);
            }
            let Some(file) = find_bundle_file(&dir) else {
                discard_dir(&dir);
                return stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now);
            };
            let offset = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
            if offset > bytes {
                discard_dir(&dir);
                return stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now);
            }
            return Ok(offset);
        }
        if epoch > old.epoch {
            discard_dir(&dir);
            return stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now);
        }
        return Err("bundle_rejected: a staged transfer has different content".to_owned());
    }
    if dir.exists() {
        discard_dir(&dir);
    }
    stage_fresh(deps, run_id, epoch, &assigned_base, head, branch, bytes, sha256, now)
}

/// Creates one fresh staging file and its manifest, returning offset zero.
#[allow(clippy::too_many_arguments)]
fn stage_fresh(
    deps: &Deps,
    run_id: &RunId,
    epoch: u64,
    base: &str,
    head: &str,
    branch: &str,
    bytes: u64,
    sha256: &str,
    now: Timestamp,
) -> Result<u64, String> {
    let dir = run_dir(deps, run_id);
    create_private_dir(&dir).map_err(|_| "bundle_failed: the staging directory could not be created".to_owned())?;
    let name = format!("{}.bundle", ulid::Ulid::generate());
    let file = dir.join(name);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .map_err(|_| "bundle_failed: the staging file could not be created".to_owned())?;
    let manifest = BundleManifest {
        run_id: run_id.as_str().to_owned(),
        epoch,
        base: base.to_owned(),
        head: head.to_ascii_lowercase(),
        branch: branch.to_owned(),
        bytes,
        sha256: sha256.to_ascii_lowercase(),
        started_at: now,
    };
    write_manifest(&dir, &manifest)?;
    Ok(0)
}

/// Appends one validated chunk at `offset`; every byte is written exactly once.
///
/// The caller already checked the frame shape; this function re-checks the staged transfer,
/// the offset, the cap and the deadline, and removes the partial on a protocol failure so a
/// `bundle_failed` run cannot leave a misleading resumable file.
#[allow(clippy::too_many_arguments)]
pub fn append_bundle_chunk(
    deps: &Deps,
    store: &Store,
    node: &vowp_protocol::NodeName,
    run_id: &RunId,
    epoch: u64,
    offset: u64,
    data: &str,
    now: Timestamp,
) -> Result<(), String> {
    let assignment = bundle_assignment(store, node, run_id, epoch)?;
    let _ = assignment;
    let dir = run_dir(deps, run_id);
    let Some(manifest) = read_manifest(&dir) else {
        return Err("bundle_rejected: no staged transfer for this run".to_owned());
    };
    if manifest.epoch != epoch {
        return Err("bundle_rejected: the chunk epoch does not match the staged transfer".to_owned());
    }
    let Some(chunk) = decode_hex(data) else {
        discard_dir(&dir);
        return Err("bundle_failed: the chunk is not lowercase hex".to_owned());
    };
    if chunk.is_empty() || chunk.len() > vowp_protocol::MAX_BUNDLE_CHUNK_BYTES {
        discard_dir(&dir);
        return Err("bundle_failed: the chunk size is outside the wire contract".to_owned());
    }
    if now.since(manifest.started_at) >= BUNDLE_TRANSFER_DEADLINE {
        discard_dir(&dir);
        return Err("bundle_failed: the bundle transfer deadline expired".to_owned());
    }
    let Some(file) = find_bundle_file(&dir) else {
        return Err("bundle_rejected: no staged transfer file for this run".to_owned());
    };
    let stored = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
    if offset != stored {
        discard_dir(&dir);
        return Err("bundle_failed: the chunk offset does not match the stored bytes".to_owned());
    }
    let next = stored.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
    if next > manifest.bytes || next > BUNDLE_MAX_BYTES {
        discard_dir(&dir);
        return Err("bundle_failed: the bundle is bigger than it declared".to_owned());
    }
    let mut handle = OpenOptions::new()
        .append(true)
        .open(&file)
        .map_err(|_| "bundle_failed: the staging file could not be opened".to_owned())?;
    handle.write_all(&chunk).and_then(|()| handle.flush()).map_err(|_| {
        discard_dir(&dir);
        "bundle_failed: the staging file could not be written".to_owned()
    })?;
    Ok(())
}

/// Finishes one bundle: re-checks size and digest, then marks the result received.
///
/// A size or digest mismatch removes the staging file and returns `bundle_failed`; the run
/// is not finished here. `TurnEnd` still carries the terminal summary, which settles the
/// run and applies the verified bundle.
#[allow(clippy::too_many_arguments)]
pub fn finish_bundle(
    deps: &Deps,
    store: &Store,
    node: &vowp_protocol::NodeName,
    run_id: &RunId,
    epoch: u64,
    bytes: u64,
    sha256: &str,
    now: Timestamp,
) -> Result<(), String> {
    let assignment = bundle_assignment(store, node, run_id, epoch)?;
    let dir = run_dir(deps, run_id);
    let Some(manifest) = read_manifest(&dir) else {
        return Err("bundle_rejected: no staged transfer for this run".to_owned());
    };
    if manifest.epoch != epoch || manifest.bytes != bytes || !manifest.sha256.eq_ignore_ascii_case(sha256) {
        discard_dir(&dir);
        return Err("bundle_failed: the end frame does not match the staged transfer".to_owned());
    }
    if now.since(manifest.started_at) >= BUNDLE_TRANSFER_DEADLINE {
        discard_dir(&dir);
        return Err("bundle_failed: the bundle transfer deadline expired".to_owned());
    }
    let Some(file) = find_bundle_file(&dir) else {
        return Err("bundle_failed: the bundle staging file is missing".to_owned());
    };
    let (size, digest) = hash_file(&file).map_err(|_| {
        discard_dir(&dir);
        "bundle_failed: the bundle staging file could not be read".to_owned()
    })?;
    if size != manifest.bytes || !digest.eq_ignore_ascii_case(&manifest.sha256) {
        discard_dir(&dir);
        return Err("bundle_failed: the bundle size or digest did not match".to_owned());
    }

    let stored = load(store, run_id).map_err(|_| "bundle_failed: the result record could not be read".to_owned())?;
    let mut record = stored.unwrap_or_default();
    record.transport = "bundle".to_owned();
    record.base_commit = manifest.base.clone();
    record.head_commit = Some(manifest.head.clone());
    record.bundle_bytes = Some(manifest.bytes);
    record.bundle_sha256 = Some(manifest.sha256.clone());
    record.status = "received".to_owned();
    record.detail = format!(
        "base={} head={} bundle {} bytes",
        short(&manifest.base),
        short(&manifest.head),
        manifest.bytes
    );
    save(store, run_id, &record).map_err(|_| "bundle_failed: the result record could not be written".to_owned())?;

    // The run is `finishing` from `BundleEnd` until the gates settle.
    store
        .advance_assignment(
            &assignment.id,
            AgentAssignmentState::Running,
            Some(now),
            &format!(
                "finishing: bundle received ({} bytes, {})",
                manifest.bytes,
                short(&manifest.sha256)
            ),
        )
        .map_err(|_| "bundle_failed: the assignment could not be marked finishing".to_owned())?;
    store
        .renew_assignment(
            &assignment.id,
            now,
            now.plus(super::dispatch::RUN_LEASE_TTL),
            &record.detail,
        )
        .map_err(|_| "bundle_failed: the run lease could not be renewed".to_owned())?;
    let owned = project_store(deps, store, &assignment.project)
        .map_err(|_| "bundle_failed: the project database could not be opened".to_owned())?;
    let project_store = owned.as_ref().unwrap_or(store);
    project_store
        .set_job_state(
            &assignment.payload.job,
            JobState::Running,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .map_err(|_| "bundle_failed: the job state could not be recorded".to_owned())?;
    Ok(())
}

/// Removes one run's staging file and private staging repository.
///
/// Best-effort by design: the result path reports success or a stable failure, and a stale
/// file under the home's own `tmp` directory is not a reason to fail a run.
pub(crate) fn discard_staged_bundle(deps: &Deps, run_id: &RunId) {
    discard_dir(&run_dir(deps, run_id));
}

/// Removes a staged transfer when the agent aborts at or above its epoch.
///
/// A stale abort for a superseded epoch never removes a newer transfer: the manifest's
/// epoch must be at most the frame's. The node must still hold a live assignment for the
/// run, so one connection cannot delete another node's staging file.
pub(crate) fn abort_bundle(deps: &Deps, store: &Store, node: &vowp_protocol::NodeName, run_id: &RunId, epoch: u64) {
    if bundle_assignment(store, node, run_id, epoch).is_err() {
        return;
    }
    let dir = run_dir(deps, run_id);
    if let Some(manifest) = read_manifest(&dir)
        && manifest.epoch <= epoch
    {
        discard_dir(&dir);
    }
}

/// Returns the staging directory of one run.
fn run_dir(deps: &Deps, run_id: &RunId) -> PathBuf {
    deps.paths.root().join(BUNDLE_ROOT).join(run_id.as_str())
}

/// Creates a 0700 directory and the parents it needs.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let Some(parent) = dir.parent() else {
        return Err(std::io::Error::other("staging directory has no parent"));
    };
    fs::create_dir_all(parent)?;
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Writes the manifest 0600 beside the bundle file.
fn write_manifest(dir: &Path, manifest: &BundleManifest) -> Result<(), String> {
    let text =
        serde_json::to_vec(manifest).map_err(|_| "bundle_failed: the manifest could not be encoded".to_owned())?;
    let path = dir.join(BUNDLE_MANIFEST);
    let mut handle = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|_| "bundle_failed: the manifest could not be created".to_owned())?;
    handle
        .write_all(&text)
        .and_then(|()| handle.flush())
        .map_err(|_| "bundle_failed: the manifest could not be written".to_owned())?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Reads one run's manifest, when it exists and parses.
fn read_manifest(dir: &Path) -> Option<BundleManifest> {
    let text = fs::read(dir.join(BUNDLE_MANIFEST)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Returns the single staged bundle file of one run.
fn find_bundle_file(dir: &Path) -> Option<PathBuf> {
    let mut found = None;
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("bundle") {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

/// Removes one staging directory if it exists.
fn discard_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Returns the lowercase hex SHA-256 of `bytes`.
///
/// This is the digest spelling `BundleStart`, `BundleEnd` and the durable result record
/// use; no base64 and no dependency on the staging path.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Returns the size and lowercase hex SHA-256 of one file.
fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok((total, hex_lower(&hasher.finalize())))
}

/// Encodes bytes as lowercase hex without adding a dependency.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    text
}

/// Decodes lowercase hex; uppercase, odd lengths and non-hex bytes return `None`.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !vowp_protocol::is_bundle_chunk_data(text) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(out)
}

/// Maps one lowercase hex digit to its value.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Returns the newest active assignment that matches the node and frame epoch.
///
/// A frame for a stale or unknown epoch is refused with a stable reason; the caller sends
/// `BundleAbort`, so the agent never keeps writing into a superseded transfer.
fn bundle_assignment(
    store: &Store,
    node: &vowp_protocol::NodeName,
    run_id: &RunId,
    epoch: u64,
) -> Result<AgentAssignment, String> {
    let assignments = store
        .assignments_for_run(run_id)
        .map_err(|_| "bundle_failed: the assignment could not be read".to_owned())?;
    let active = assignments
        .iter()
        .filter(|assignment| assignment.node == *node && assignment.state.is_active())
        .collect::<Vec<_>>();
    let Some(assignment) = active.iter().find(|assignment| assignment.epoch == epoch) else {
        let newest = active.iter().map(|assignment| assignment.epoch).max();
        return Err(match newest {
            Some(newest) if newest > epoch => {
                "bundle_rejected: the transfer epoch is stale; a newer assignment supersedes it".to_owned()
            }
            _ => "bundle_rejected: no live bundle assignment for this run and epoch".to_owned(),
        });
    };
    if assignment.payload.result_transport != ResultTransport::Bundle {
        return Err("bundle_rejected: the assignment does not use the bundle transport".to_owned());
    }
    Ok((**assignment).clone())
}

/// Settles the terminal summary of an offloaded run.
///
/// Returns the outcome and detail the caller must write with the run. A run without a live
/// offload assignment is left untouched. When the summary claims success, the assignment's
/// transport decides: a `push` branch is fetched, or a `bundle` is verified and imported.
/// Either result is gated before the run may move to `gating`; every other path keeps the
/// job retryable and still releases the run lease and the assignment slot.
pub fn settle_terminal(
    deps: &Deps,
    store: &Store,
    run: &Run,
    reported_head: Option<&str>,
    outcome: RunOutcome,
    detail: String,
) -> Result<Settled, CoreError> {
    let Some(assignment) = live_assignment(store, &run.id)? else {
        return Ok(Settled { outcome, detail });
    };
    if Some(&assignment.payload.job) != run.job_id.as_ref() {
        return Err(CoreError::Invalid(format!(
            "run {} belongs to job {:?}, not assignment job {}",
            run.id, run.job_id, assignment.payload.job
        )));
    }
    let project = store.require_project(&assignment.project)?;
    let project_deps = project_deps(deps, &project);
    let owned = project_store(&project_deps, store, &assignment.project)?;
    let project_store = owned.as_ref().unwrap_or(store);
    let deps = &project_deps;
    let mut record = load(store, &run.id)?.unwrap_or_default();
    record.transport = transport_name(&assignment);
    record.base_commit = assignment.payload.base_commit.clone();
    let reported = reported_head
        .map(str::trim)
        .filter(|head| !head.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| record.head_commit.clone());

    let mut settled = Settled { outcome, detail };
    let mut accepted = false;
    let transport = assignment.payload.result_transport;
    if settled.outcome == RunOutcome::Ok {
        match transport {
            ResultTransport::Push => {
                if record.transport != "push" {
                    settled = failed(
                        RunOutcome::Error,
                        "git_push_failed: the assignment did not name a push remote",
                    );
                } else {
                    match return_pushed(deps, project_store, run, &assignment, reported.as_deref(), &project) {
                        Ok(returned) => {
                            accepted = true;
                            record.head_commit = Some(returned.head.clone());
                            record.base_commit = returned.base.clone();
                            record.status = "returned".to_owned();
                            record.detail = format!("base={} head={}", returned.base, returned.head);
                            settled.detail = append_marker(
                                &settled.detail,
                                &format!("result: transport=push base={} head={}", returned.base, returned.head),
                            );
                        }
                        Err(reason) => {
                            settled = failed(RunOutcome::Error, &reason);
                        }
                    }
                }
            }
            ResultTransport::Bundle => {
                let base = assignment.payload.base_commit.trim().to_ascii_lowercase();
                let head = record.head_commit.clone().or_else(|| reported.clone());
                if head.as_deref() == Some(base.as_str()) {
                    // The run committed nothing: the bundle would be empty, which is a
                    // successful-looking summary with no work. That is `result_empty`.
                    settled = failed(RunOutcome::Error, "result_empty: the run produced no commit");
                } else if record.status != "received" || record.bundle_bytes.is_none() || record.bundle_sha256.is_none()
                {
                    settled = failed(
                        RunOutcome::Error,
                        "bundle_failed: the run ended without a complete bundle",
                    );
                } else {
                    match return_bundled(
                        deps,
                        project_store,
                        run,
                        &assignment,
                        &project,
                        &record,
                        reported.as_deref(),
                    ) {
                        Ok(returned) => {
                            accepted = true;
                            record.head_commit = Some(returned.head.clone());
                            record.base_commit = returned.base.clone();
                            record.status = "returned".to_owned();
                            record.detail = format!(
                                "base={} head={} bundle {} bytes",
                                returned.base,
                                returned.head,
                                record.bundle_bytes.unwrap_or_default()
                            );
                            settled.detail = append_marker(
                                &settled.detail,
                                &format!("result: transport=bundle base={} head={}", returned.base, returned.head),
                            );
                        }
                        Err(reason) => {
                            settled = failed(RunOutcome::Error, &reason);
                        }
                    }
                }
            }
        }
    } else if settled.detail.contains("bundle_failed") {
        // The node already classified the bundle. Keep the stable name and drop any local
        // detail that could name a path or a git stderr line.
        settled.detail = "bundle_failed: the agent could not return the bundle".to_owned();
    } else if settled.detail.contains("git_push_failed") {
        // The node already classified the push. Keep the stable name and drop its stderr,
        // which may contain a credential URL or an absolute local path.
        settled.detail = "git_push_failed: the agent could not push the job branch".to_owned();
    }

    // A bundle staging file and its private staging repo never outlive the settle, whether
    // the bundle was accepted or rejected. Only the step-4 inspection ref can remain.
    if transport == ResultTransport::Bundle {
        discard_staged_bundle(deps, &run.id);
    }

    if !accepted {
        record.status = "failed".to_owned();
        record.detail = cap(&settled.detail);
    }
    save(store, &run.id, &record)?;

    let job = project_store.require_job(&assignment.payload.job)?;
    dispatch::clear_waiting(project_store, &job.id)?;
    if accepted {
        if job.state == JobState::Running {
            let bounds = TransitionBounds::default();
            let applied =
                crate::loops::apply_event(project_store, &job.id, &JobEvent::ReportReady, bounds, None, None)?;
            let _ = applied.effects;
        }
    } else if job.state == JobState::Running {
        project_store.set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: Some(format!("result return failed: {}", cap(&settled.detail))),
                parked_reason: None,
            },
        )?;
    }

    finish_assignment(store, &assignment, accepted)?;
    dispatch::release_run_lease(store, &assignment.session_id)?;
    // A missing audit row must not turn an accepted result into a failed run; the run row
    // still carries the same facts.
    if let Err(error) = record_ledger(deps, project_store, run, &assignment, &record, accepted) {
        deps.log.log(
            Level::Warn,
            "result",
            &format!("result ledger for {} failed: {:?}", run.id, error.kind()),
        );
    }
    Ok(settled)
}

/// Returns the newest live assignment for a run, when the run is offloaded.
fn live_assignment(store: &Store, run: &RunId) -> Result<Option<AgentAssignment>, CoreError> {
    let mut assignments = store.assignments_for_run(run)?;
    assignments.sort_by_key(|assignment| assignment.epoch);
    Ok(assignments
        .into_iter()
        .rev()
        .find(|assignment| assignment.state.is_active()))
}

/// Returns the assignment's transport name for the durable record.
///
/// The assignment's value wins over the current config: a config change mid-run never
/// switches a push result to a bundle or the other way. `none` means an older push
/// assignment without a remote URL.
fn transport_name(assignment: &AgentAssignment) -> String {
    match assignment.payload.result_transport {
        ResultTransport::Bundle => "bundle".to_owned(),
        ResultTransport::Push if assignment.payload.remote_url.trim().is_empty() => "none".to_owned(),
        ResultTransport::Push => "push".to_owned(),
    }
}

/// One accepted result.
struct Returned {
    /// Assigned base commit.
    base: String,
    /// Verified fetched head commit.
    head: String,
}

/// Fetches and verifies one pushed job branch, gates it and stages its worktree.
///
/// The error is a short, path-free reason for the run detail. No credential URL, git stderr
/// or absolute local path is ever included.
fn return_pushed(
    deps: &Deps,
    store: &Store,
    run: &Run,
    assignment: &AgentAssignment,
    reported: Option<&str>,
    project: &vowp_protocol::Project,
) -> Result<Returned, String> {
    let config = deps.config_now();
    let Some(reported) = reported else {
        return Err("git_push_failed: the agent reported no head commit".to_owned());
    };
    if !is_commit_id(reported) {
        return Err("result_rejected: the reported head is not a commit id".to_owned());
    }
    let remote = assignment.payload.remote.trim();
    if remote.is_empty() {
        return Err("git_push_failed: the assignment did not name a remote".to_owned());
    }
    if remote != config.project.remote {
        return Err("result_rejected: the assignment names a remote that is not the project remote".to_owned());
    }
    let Some(expected_url) = project
        .remote_url
        .as_deref()
        .map(dispatch::strip_userinfo)
        .filter(|url| !url.is_empty())
    else {
        return Err("result_rejected: the project has no remote URL to verify".to_owned());
    };
    let repo = crate::loops::main_repo(deps, store, &assignment.project)
        .map_err(|_| "result_rejected: the project checkout is not available".to_owned())?;
    if !repo.is_dir() {
        return Err("result_rejected: the project checkout is not available".to_owned());
    }
    for push in [false, true] {
        let Some(url) = remote_url(deps, &repo, remote, push) else {
            return Err("result_rejected: the project remote could not be read".to_owned());
        };
        if dispatch::strip_userinfo(url.trim()) != expected_url {
            return Err("result_rejected: the project remote does not match the project URL".to_owned());
        }
    }

    deps.git
        .fetch(&repo, remote)
        .map_err(|_| "git_push_failed: the project remote could not be fetched".to_owned())?;
    let branch = format!("job/{}", assignment.payload.job);
    let fetched = deps
        .git
        .head_sha(&repo, &format!("{remote}/{branch}"))
        .map_err(|_| "git_push_failed: the job branch was not found on the project remote".to_owned())?;
    let fetched = fetched.trim().to_ascii_lowercase();
    if fetched != reported {
        return Err(format!(
            "result_rejected: the reported head {} is not the fetched head {}",
            short(reported),
            short(&fetched)
        ));
    }
    let base = assignment.payload.base_commit.trim().to_owned();
    if base.is_empty() {
        return Err("result_rejected: the assignment did not name a base commit".to_owned());
    }
    if deps.git.head_sha(&repo, &base).is_err() {
        return Err("result_rejected: the assigned base commit is not in the home repository".to_owned());
    }
    let backwards = deps
        .git
        .commits_between(&repo, &fetched, &base)
        .map_err(|_| "result_rejected: the base history could not be read".to_owned())?;
    if !backwards.is_empty() {
        return Err(format!(
            "result_rejected: the fetched head {} does not descend from the assigned base {}",
            short(&fetched),
            short(&base)
        ));
    }

    let worktree = staging_path(deps, run);
    if worktree.exists() {
        return Err("result_rejected: the staging worktree already exists".to_owned());
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "result_rejected: the staging directory could not be created".to_owned())?;
    }
    git_ref_update(deps, &repo, &branch, &fetched)?;
    deps.git
        .worktree_add(&repo, &worktree, &branch, &fetched)
        .map_err(|_| "result_rejected: the staging worktree could not be created".to_owned())?;
    apply_verified_head(deps, store, run, assignment, &repo, &worktree, &branch, &base, &fetched)
}

/// Gates a verified head and publishes its staging worktree.
///
/// Shared by the push and bundle paths: the branch already points at `head` and the staging
/// worktree exists. A gate failure removes the worktree but leaves the branch for
/// inspection; only a passing head records the job workspace and returns.
#[allow(clippy::too_many_arguments)]
fn apply_verified_head(
    deps: &Deps,
    store: &Store,
    run: &Run,
    assignment: &AgentAssignment,
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    head: &str,
) -> Result<Returned, String> {
    let mut gate_job = store
        .require_job(&assignment.payload.job)
        .map_err(|_| "result_rejected: the job is not available".to_owned())?;
    gate_job.branch = Some(branch.to_owned());
    gate_job.worktree = Some(worktree.display().to_string());
    // The gate helpers treat `project.default_branch` as the diff base. Pin it to the
    // assigned base commit for this evaluation so the diff is exactly `base..head` even
    // when the home's `main` advanced while the run was on the node.
    let mut gate_config = (*deps.config_now()).clone();
    gate_config.project.default_branch = base.to_owned();
    let mut gate_deps = deps.clone();
    gate_deps.config = Arc::new(gate_config);
    let ctx = GateCtx {
        deps: &gate_deps,
        job: &gate_job,
        run_id: Some(run.id.clone()),
        worktree,
        repo,
        store,
        attempt: gate_job.attempts,
    };
    let runner = GateRunner::new(gate_deps.clone());
    let ledger = Ledger::new(deps.clone());
    let mut failures = Vec::new();
    for (name, gate) in [
        ("owns_respected", Gate::OwnsRespected(true)),
        ("secret_scan", Gate::SecretScan(true)),
    ] {
        let index = gate_index(&gate_job, &gate);
        let mut result = runner
            .evaluate(&ctx, index, &gate)
            .map_err(|_| "result_rejected: the gate runner failed".to_owned())?;
        let id = store
            .insert_gate_result(&result)
            .map_err(|_| "result_rejected: the gate result could not be recorded".to_owned())?;
        result.id = Some(id);
        ledger
            .record_gate(store, &result)
            .map_err(|_| "result_rejected: the gate ledger row could not be written".to_owned())?;
        if !result.passed {
            failures.push(gate_failure(name, &result.evidence));
        }
    }
    if !failures.is_empty() {
        // The branch stays for inspection; the worktree is removed so a retry can move the
        // same `job/<id>` branch to the next pushed or bundled head.
        let _ = deps.git.worktree_remove(repo, worktree);
        return Err(format!("result_gate_failed: {}", failures.join("; ")));
    }

    store
        .set_job_workspace(&gate_job.id, gate_job.branch.as_deref(), gate_job.worktree.as_deref())
        .map_err(|_| "result_rejected: the job workspace could not be recorded".to_owned())?;
    Ok(Returned {
        base: base.to_owned(),
        head: head.to_owned(),
    })
}

/// Verifies one received bundle and imports only its verified head.
///
/// Spec: hardening.md §5.3 steps 2–4. The bundle is verified in the project repository, its
/// exact ref set and prerequisite are checked, then only `refs/heads/job/<id>` is fetched
/// into a private staging bare repository that already has the base. The fetched head must
/// equal the reported head and descend from the base; only then are the objects imported
/// into the project repository as `refs/vowp/jobs/<id>` and the existing branch, staging
/// worktree and gates run. Every failure before the import leaves no project ref.
fn return_bundled(
    deps: &Deps,
    store: &Store,
    run: &Run,
    assignment: &AgentAssignment,
    _project: &vowp_protocol::Project,
    record: &AgentRunResult,
    reported: Option<&str>,
) -> Result<Returned, String> {
    let Some(head) = record.head_commit.clone() else {
        return Err("bundle_rejected: the bundle head was not recorded".to_owned());
    };
    let head = head.trim().to_ascii_lowercase();
    if !is_commit_id(&head) {
        return Err("bundle_rejected: the bundle head is not a commit id".to_owned());
    }
    if let Some(reported) = reported.map(str::trim).filter(|head| !head.is_empty())
        && !reported.eq_ignore_ascii_case(&head)
    {
        return Err("bundle_rejected: the reported head does not match the bundle head".to_owned());
    }
    let base = assignment.payload.base_commit.trim().to_ascii_lowercase();
    if !is_commit_id(&base) {
        return Err("bundle_rejected: the assignment did not name a base commit".to_owned());
    }
    if head == base {
        return Err("result_empty: the run produced no commit".to_owned());
    }

    let repo = crate::loops::main_repo(deps, store, &assignment.project)
        .map_err(|_| "result_rejected: the project checkout is not available".to_owned())?;
    if !repo.is_dir() {
        return Err("result_rejected: the project checkout is not available".to_owned());
    }
    let dir = run_dir(deps, &run.id);
    let Some(file) = find_bundle_file(&dir) else {
        return Err("bundle_failed: the bundle staging file is missing".to_owned());
    };
    let Some(manifest) = read_manifest(&dir) else {
        return Err("bundle_failed: the bundle manifest is missing".to_owned());
    };
    if manifest.epoch != assignment.epoch
        || !manifest.head.eq_ignore_ascii_case(&head)
        || !manifest.base.eq_ignore_ascii_case(&base)
    {
        return Err("bundle_rejected: the staged manifest does not match the assignment".to_owned());
    }
    let declared_bytes = record
        .bundle_bytes
        .ok_or_else(|| "bundle_failed: the bundle size was not recorded".to_owned())?;
    let declared_sha = record
        .bundle_sha256
        .as_deref()
        .ok_or_else(|| "bundle_failed: the bundle digest was not recorded".to_owned())?;
    if manifest.bytes != declared_bytes || !manifest.sha256.eq_ignore_ascii_case(declared_sha) {
        return Err("bundle_rejected: the staged manifest changed after the transfer".to_owned());
    }
    let (size, digest) = hash_file(&file).map_err(|_| "bundle_failed: the bundle could not be read".to_owned())?;
    if size != manifest.bytes || !digest.eq_ignore_ascii_case(&manifest.sha256) {
        return Err("bundle_failed: the bundle size or digest did not match".to_owned());
    }
    let Some(file_text) = file.to_str() else {
        return Err("bundle_rejected: the staging path is not valid UTF-8".to_owned());
    };

    let verified = run_git(
        deps,
        &repo,
        vec![
            "git".to_owned(),
            "bundle".to_owned(),
            "verify".to_owned(),
            file_text.to_owned(),
        ],
    )
    .map_err(|_| "bundle_rejected: `git bundle verify` failed".to_owned())?;
    let mut prerequisites = Vec::new();
    let mut expecting = false;
    for line in verified.lines() {
        let line = line.trim();
        if line.starts_with("The bundle requires") {
            expecting = true;
            continue;
        }
        if expecting {
            if line.is_empty() {
                continue;
            }
            if line.starts_with("The bundle ") {
                expecting = false;
                continue;
            }
            if let Some(sha) = line.split_whitespace().next() {
                prerequisites.push(sha.to_ascii_lowercase());
            }
        }
    }
    if prerequisites != vec![base.clone()] {
        return Err("bundle_rejected: the assigned base is not the bundle's prerequisite".to_owned());
    }

    let heads = run_git(
        deps,
        &repo,
        vec![
            "git".to_owned(),
            "bundle".to_owned(),
            "list-heads".to_owned(),
            file_text.to_owned(),
        ],
    )
    .map_err(|_| "bundle_rejected: `git bundle list-heads` failed".to_owned())?;
    let listed = heads.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>();
    let expected_ref = format!("refs/heads/job/{}", assignment.payload.job);
    let exact = match listed.as_slice() {
        [line] => {
            let mut parts = line.split_whitespace();
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(sha), Some(reference), None)
                    if reference == expected_ref && sha.eq_ignore_ascii_case(&head)
            )
        }
        _ => false,
    };
    if !exact {
        return Err("bundle_rejected: the bundle does not name exactly the job branch at the head".to_owned());
    }

    // Stage in a private bare repository that already has the base. Fetching the bundle
    // refs there can never move a project ref or touch the main checkout.
    let Some(dir_text) = dir.to_str() else {
        return Err("bundle_rejected: the staging path is not valid UTF-8".to_owned());
    };
    let staging = dir.join(BUNDLE_STAGING_REPO);
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .map_err(|_| "bundle_failed: the old staging repo could not be removed".to_owned())?;
    }
    let Some(staging_text) = staging.to_str() else {
        return Err("bundle_rejected: the staging path is not valid UTF-8".to_owned());
    };
    run_git(
        deps,
        Path::new(dir_text),
        vec![
            "git".to_owned(),
            "init".to_owned(),
            "--bare".to_owned(),
            staging_text.to_owned(),
        ],
    )
    .map_err(|_| "bundle_rejected: the staging repository could not be initialized".to_owned())?;
    let Some(repo_text) = repo.to_str() else {
        return Err("result_rejected: the project checkout is not valid UTF-8".to_owned());
    };
    run_git(
        deps,
        &staging,
        vec![
            "git".to_owned(),
            "fetch".to_owned(),
            "--no-tags".to_owned(),
            repo_text.to_owned(),
            base.clone(),
        ],
    )
    .map_err(|_| "bundle_rejected: the base commit could not be added to the staging repository".to_owned())?;
    let job = assignment.payload.job.as_str();
    let bundle_refspec = format!("refs/heads/job/{job}:refs/vowp/jobs/{job}");
    run_git(
        deps,
        &staging,
        vec![
            "git".to_owned(),
            "fetch".to_owned(),
            "--no-tags".to_owned(),
            file_text.to_owned(),
            bundle_refspec,
        ],
    )
    .map_err(|_| "bundle_rejected: the bundle ref could not be fetched into the staging repository".to_owned())?;
    let fetched = run_git(
        deps,
        &staging,
        vec![
            "git".to_owned(),
            "rev-parse".to_owned(),
            format!("refs/vowp/jobs/{job}"),
        ],
    )
    .map_err(|_| "bundle_rejected: the fetched bundle ref is missing".to_owned())?;
    if !fetched.trim().eq_ignore_ascii_case(&head) {
        return Err("bundle_rejected: the fetched head is not the reported head".to_owned());
    }
    run_git(
        deps,
        &staging,
        vec![
            "git".to_owned(),
            "merge-base".to_owned(),
            "--is-ancestor".to_owned(),
            base.clone(),
            head.clone(),
        ],
    )
    .map_err(|_| "bundle_rejected: the fetched head does not descend from the assigned base".to_owned())?;

    // Step 4: import the verified objects, then let the existing path point the job branch.
    let import_refspec = format!("+refs/vowp/jobs/{job}:refs/vowp/jobs/{job}");
    run_git(
        deps,
        &repo,
        vec![
            "git".to_owned(),
            "fetch".to_owned(),
            "--no-tags".to_owned(),
            staging_text.to_owned(),
            import_refspec,
        ],
    )
    .map_err(|_| "bundle_rejected: the verified objects could not be imported".to_owned())?;

    let branch = format!("job/{job}");
    let worktree = staging_path(deps, run);
    if worktree.exists() {
        return Err("result_rejected: the staging worktree already exists".to_owned());
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "result_rejected: the staging directory could not be created".to_owned())?;
    }
    git_ref_update(deps, &repo, &branch, &head)?;
    deps.git
        .worktree_add(&repo, &worktree, &branch, &head)
        .map_err(|_| "result_rejected: the staging worktree could not be created".to_owned())?;
    apply_verified_head(deps, store, run, assignment, &repo, &worktree, &branch, &base, &head)
}

/// Returns the staging worktree path for one run.
fn staging_path(deps: &Deps, run: &Run) -> std::path::PathBuf {
    deps.paths.root().join("agent-results").join(run.id.as_str())
}

/// Returns the gate index in the job's gate list, or `-1` when the job does not list it.
fn gate_index(job: &vowp_protocol::Job, gate: &Gate) -> i32 {
    job.gates
        .iter()
        .position(|candidate| candidate.kind() == gate.kind())
        .map(|index| i32::try_from(index).unwrap_or(i32::MAX))
        .unwrap_or(-1)
}

/// Describes one failed gate with relative paths and rule names only.
fn gate_failure(name: &str, evidence: &Evidence) -> String {
    match evidence {
        Evidence::OwnsRespected { violations, .. } => {
            let paths = violations.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            format!("{name}: {paths}")
        }
        Evidence::SecretScan { hits, .. } => {
            let found = hits
                .iter()
                .take(5)
                .map(|hit| format!("{} at {}:{}", hit.rule, hit.path, hit.line))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}: {found}")
        }
        _ => format!("{name}: failed"),
    }
}

/// Runs `git remote get-url [--push] <remote>` in `repo`.
fn remote_url(deps: &Deps, repo: &Path, remote: &str, push: bool) -> Option<String> {
    let mut argv = vec!["git".to_owned(), "remote".to_owned(), "get-url".to_owned()];
    if push {
        argv.push("--push".to_owned());
    }
    argv.push(remote.to_owned());
    run_git(deps, repo, argv).ok()
}

/// Points the local job branch at `head` and reports a stable failure on conflict.
fn git_ref_update(deps: &Deps, repo: &Path, branch: &str, head: &str) -> Result<(), String> {
    let argv = vec![
        "git".to_owned(),
        "branch".to_owned(),
        "-f".to_owned(),
        branch.to_owned(),
        head.to_owned(),
    ];
    run_git(deps, repo, argv).map_err(|_| {
        "result_rejected: the job branch is checked out in another worktree or could not be updated".to_owned()
    })?;
    Ok(())
}

/// Runs a `git` argv and returns trimmed stdout.
fn run_git(deps: &Deps, repo: &Path, argv: Vec<String>) -> Result<String, CoreError> {
    let output = deps.cmd.run(&CommandSpec {
        shell: None,
        argv,
        cwd: Some(repo.to_path_buf()),
        env: Vec::new(),
        timeout: GIT_TIMEOUT,
    })?;
    if output.exit_code != Some(0) {
        return Err(CoreError::Command("a git result-return read failed".to_owned()));
    }
    Ok(output.text().trim().to_owned())
}

/// Marks the assignment terminal without moving a row that was already fenced.
fn finish_assignment(store: &Store, assignment: &AgentAssignment, accepted: bool) -> Result<(), CoreError> {
    if assignment.state.is_terminal() {
        return Ok(());
    }
    let detail = if accepted {
        "result returned to the home"
    } else {
        "result return failed; the job stays retryable"
    };
    store.set_assignment_state(
        &assignment.id,
        AgentAssignmentState::Finished,
        assignment.claimed_at,
        detail,
    )?;
    Ok(())
}

/// Appends one ledger note with the transport facts and the gate outcome.
fn record_ledger(
    deps: &Deps,
    store: &Store,
    run: &Run,
    assignment: &AgentAssignment,
    record: &AgentRunResult,
    accepted: bool,
) -> Result<(), CoreError> {
    let ledger = Ledger::new(deps.clone());
    ledger.record(
        store,
        LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Note,
            project: Some(assignment.project.clone()),
            job_id: Some(assignment.payload.job.clone()),
            run_id: Some(run.id.clone()),
            session_id: run.session_id.clone(),
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!(
                "result return {}: {}",
                run.id,
                if accepted { "returned" } else { "rejected" }
            ),
            detail: serde_json::json!({
                "transport": record.transport,
                "base_commit": record.base_commit,
                "head_commit": record.head_commit,
                "bundle_bytes": record.bundle_bytes,
                "bundle_sha256": record.bundle_sha256,
                "spool_bytes": record.spool_bytes,
                "status": record.status,
                "accepted": accepted,
                "detail": record.detail,
            }),
        },
    )?;
    Ok(())
}

/// Builds a failed settlement with stable detail.
fn failed(outcome: RunOutcome, detail: &str) -> Settled {
    Settled {
        outcome,
        detail: cap(detail),
    }
}

/// Appends `marker` to a summary detail, keeping the store's tail truncation in mind.
fn append_marker(detail: &str, marker: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        marker.to_owned()
    } else {
        format!("{detail}; {marker}")
    }
}

/// Caps a detail at [`DETAIL_CAP`] characters and drops any trailing partial line noise.
fn cap(detail: &str) -> String {
    detail.chars().take(DETAIL_CAP).collect()
}

/// Returns whether `value` looks like a full or abbreviated git commit id.
fn is_commit_id(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Returns whether `value` is exactly one lowercase hex SHA-256 digest.
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Returns at most twelve characters of a commit id.
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}
