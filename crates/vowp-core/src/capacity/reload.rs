//! Live reload of the effective `VowpConfig` used by dispatch and capacity.
//!
//! The supervisor keeps the config in a [`ConfigCell`]. Once per project tick it compares
//! `vowp.toml` and the node-level `config.toml` with the fingerprint of the applied config,
//! parses and validates a changed file into a complete [`VowpConfig`], and swaps the cell
//! atomically. An invalid file keeps the previous config, logs the path and error, and opens
//! one node-inbox notice; no partial config is ever installed.
//!
//! Active runs keep their provider slot because admission counts open runs against the new
//! cap: raising `[providers.<name>].max_concurrent` frees slots on the next tick, while
//! lowering it only stops new admissions. Reload never finishes, interrupts or pauses a run.
//!
//! The applied state is published in the node `kv` table so `vowp capacity report` can show
//! the effective provider caps and whether the on-disk file still differs. A CLI reload
//! writes `capacity:reload_request:<project>:<token>` and polls
//! `capacity:reload:<token>`; the supervisor serves that request during its config step.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vowp_protocol::{
    CapacityTarget, InboxId, InboxItem, InboxKind, InboxState, LedgerEntry, LedgerKind, Level, NodeFile, ProjectName,
    Run, Timestamp, VowpConfig,
};
use vowp_store::Store;

use crate::{CoreError, Deps};

use super::targets;

/// One config file's content hash plus its mtime and length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    /// Whole seconds of the file's modification time.
    pub modified_secs: u64,
    /// Nanosecond part of the modification time.
    pub modified_nanos: u32,
    /// File length in bytes.
    pub length: u64,
    /// Lowercase SHA-256 of the file bytes.
    pub sha256: String,
}

/// The project config file plus the optional node defaults file that feed one effective config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFingerprint {
    /// Fingerprint of the project's `vowp.toml`.
    pub project: FileStamp,
    /// Fingerprint of `<node>/config.toml`, when that file exists.
    pub node: Option<FileStamp>,
}

/// The effective config plus the fingerprint that was last applied.
pub struct ConfigCell {
    current: RwLock<Arc<VowpConfig>>,
    applied: Mutex<Option<ConfigFingerprint>>,
}

impl ConfigCell {
    /// Creates a cell holding `config`.
    fn new(config: Arc<VowpConfig>) -> Self {
        Self {
            current: RwLock::new(config),
            applied: Mutex::new(None),
        }
    }

    /// Returns the current effective config.
    pub fn current(&self) -> Arc<VowpConfig> {
        self.current.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Returns the fingerprint of the last applied config, when one has been recorded.
    pub fn applied(&self) -> Option<ConfigFingerprint> {
        self.applied.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Replaces the current config in one write and returns the previous value.
    fn swap(&self, next: Arc<VowpConfig>) -> Arc<VowpConfig> {
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut current, next)
    }

    /// Records the fingerprint of the config now in effect.
    fn set_applied(&self, fingerprint: ConfigFingerprint) {
        *self.applied.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
    }
}

/// The map key is the node root plus the project name: one cell per project on a node.
type CellKey = (PathBuf, ProjectName);

/// Returns the process-wide cell map.
///
/// Entries are kept for the process lifetime: a `Deps` value carries only the startup
/// snapshot, so dropping the cell would silently undo a reload between two calls. One small
/// cell per project on a node is bounded and harmless.
fn cells() -> &'static Mutex<BTreeMap<CellKey, Arc<ConfigCell>>> {
    static CELLS: OnceLock<Mutex<BTreeMap<CellKey, Arc<ConfigCell>>>> = OnceLock::new();
    CELLS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Returns the shared config cell for `deps`, creating it from the startup config.
pub fn cell_for(deps: &Deps) -> Arc<ConfigCell> {
    let key = (deps.paths.root().to_path_buf(), deps.config.project.name.clone());
    let mut cells = cells().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(cell) = cells.get(&key) {
        return cell.clone();
    }
    let cell = Arc::new(ConfigCell::new(deps.config.clone()));
    cells.insert(key, cell.clone());
    cell
}

/// Returns the effective config, falling back to the startup config in `deps`.
pub fn effective_config(deps: &Deps) -> Arc<VowpConfig> {
    cell_for(deps).current()
}

/// Returns the provider concurrency left for new dispatch, keyed by configured provider.
///
/// Open runs are counted against the current cap. A lowered cap therefore yields zero free
/// slots without touching the runs that already hold one.
pub fn provider_free(config: &VowpConfig, open_runs: &[Run]) -> BTreeMap<String, u32> {
    let mut free = BTreeMap::new();
    for profile in config.profiles.values() {
        for executor in &profile.ladder {
            let provider = executor.provider();
            if free.contains_key(&provider) {
                continue;
            }
            let Some(provider_config) = config.providers.get(&provider) else {
                continue;
            };
            let running = open_runs
                .iter()
                .filter(|run| run.provider.as_deref() == Some(provider.as_str()))
                .count()
                .try_into()
                .unwrap_or(u32::MAX);
            free.insert(provider, provider_config.max_concurrent.saturating_sub(running));
        }
    }
    free
}

/// Persistent state of the last applied or attempted reload, stored in the node `kv` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReloadState {
    /// Absolute path of the project `vowp.toml`.
    pub config_path: Option<String>,
    /// Fingerprint of the applied config.
    pub fingerprint: Option<ConfigFingerprint>,
    /// Effective provider caps of the applied config.
    pub provider_caps: BTreeMap<String, u32>,
    /// Effective worker target when the config was applied.
    pub target_workers: u32,
    /// When the config was applied.
    pub applied_at: Option<Timestamp>,
    /// Last validation or I/O error; the previous config stays effective.
    pub last_error: Option<String>,
}

/// One reload attempt's result, written to the request key and rendered by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReloadOutcome {
    /// True when this attempt installed a new config.
    pub applied: bool,
    /// Absolute path of the project `vowp.toml`, when one exists.
    pub config_path: Option<String>,
    /// Provider caps now effective.
    pub provider_caps: BTreeMap<String, u32>,
    /// Target after the attempt, when it could be read.
    pub target: Option<CapacityTarget>,
    /// Keys that changed, with `(added)` / `(removed)` markers for missing sides.
    pub changed: Vec<String>,
    /// True when the on-disk config still differs from the applied config.
    pub pending: bool,
    /// Validation or I/O error; the previous config remains effective.
    pub error: Option<String>,
}

/// The result of a read-only `vowp capacity reload --check`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPreview {
    /// Absolute path of the project `vowp.toml`.
    pub config_path: String,
    /// Provider caps the file would apply.
    pub provider_caps: BTreeMap<String, u32>,
    /// Target the capacity manager would report with the file applied.
    pub target: CapacityTarget,
    /// Keys that differ from the effective config.
    pub changed: Vec<String>,
    /// True when the on-disk file still differs from the applied config.
    pub pending: bool,
    /// Fingerprint of the on-disk file.
    pub fingerprint: ConfigFingerprint,
    /// The parsed and validated config document.
    pub config: VowpConfig,
}

/// Returns the `kv` key that holds one project's [`ReloadState`].
pub fn state_key(project: &ProjectName) -> String {
    format!("capacity:config:{project}")
}

/// Returns the `kv` key a CLI uses for the result of one reload request.
pub fn result_key(token: &str) -> String {
    format!("capacity:reload:{token}")
}

/// Returns the `kv` prefix for pending reload requests of one project.
pub fn request_prefix(project: &ProjectName) -> String {
    format!("capacity:reload_request:{project}:")
}

/// Reads one project's published reload state.
///
/// A corrupt value is treated as absent: the report falls back to the effective config
/// rather than failing because a diagnostic row is unreadable.
pub fn read_state(store: &Store, project: &ProjectName) -> Result<Option<ReloadState>, CoreError> {
    let Some(text) = store.kv_get(&state_key(project))? else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&text).ok())
}

/// Writes one project's reload state.
fn write_state(store: &Store, project: &ProjectName, state: &ReloadState) -> Result<(), CoreError> {
    let text = serde_json::to_string(state).map_err(|error| CoreError::Invalid(error.to_string()))?;
    store.kv_set(&state_key(project), &text)?;
    Ok(())
}

/// Returns the project `vowp.toml` path the supervisor would load at startup.
pub fn config_path(deps: &Deps, store: &Store) -> Result<Option<PathBuf>, CoreError> {
    let name = &deps.config.project.name;
    let root = if let Some(entry) = store.project_entry(name)? {
        PathBuf::from(entry.path)
    } else if let Some(path) = &deps.config.project.path {
        PathBuf::from(path)
    } else {
        return Ok(None);
    };
    let path = root.join("vowp.toml");
    Ok(path.is_file().then_some(path))
}

/// Fingerprints `<project_root>/vowp.toml` plus `<node_home>/config.toml`.
///
/// Returns `None` when the project file does not exist.
pub fn fingerprint_at(project_root: &Path, node_home: &Path) -> Result<Option<ConfigFingerprint>, CoreError> {
    let project = project_root.join("vowp.toml");
    if !project.is_file() {
        return Ok(None);
    }
    let node = node_home.join("config.toml");
    Ok(Some(ConfigFingerprint {
        project: stamp(&project)?,
        node: if node.is_file() { Some(stamp(&node)?) } else { None },
    }))
}

/// Returns the fingerprint of one file.
fn stamp(path: &Path) -> Result<FileStamp, CoreError> {
    let metadata = std::fs::metadata(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let (modified_secs, modified_nanos) = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    let bytes = std::fs::read(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let digest = Sha256::digest(&bytes);
    let sha256 = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(FileStamp {
        modified_secs,
        modified_nanos,
        length: metadata.len(),
        sha256,
    })
}

/// Parses and validates a project config exactly like supervisor startup does.
fn load_config(deps: &Deps, path: &Path) -> Result<VowpConfig, CoreError> {
    let text = std::fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut config = VowpConfig::from_toml(&text)?;
    let node_path = deps.paths.root().join("config.toml");
    if node_path.is_file() {
        let node_text = std::fs::read_to_string(&node_path).map_err(|source| CoreError::Io {
            path: node_path.display().to_string(),
            source,
        })?;
        let node_file = NodeFile::from_toml(&node_text)?;
        config = config.with_node_defaults(&node_file);
    }
    config.validate()?;
    if config.project.name != deps.config.project.name {
        return Err(CoreError::Invalid(format!(
            "{} names project {} but the supervisor is running {}",
            path.display(),
            config.project.name,
            deps.config.project.name
        )));
    }
    if config.project.path.is_none() {
        config.project.path = path.parent().map(|parent| parent.display().to_string());
    }
    Ok(config)
}

/// Returns whether the on-disk config differs from the applied one.
pub fn pending(deps: &Deps, store: &Store) -> Result<bool, CoreError> {
    let Some(path) = config_path(deps, store)? else {
        return Ok(false);
    };
    let Some(root) = path.parent() else {
        return Ok(false);
    };
    let Some(disk) = fingerprint_at(root, deps.paths.root())? else {
        return Ok(false);
    };
    let state = read_state(store, &deps.config.project.name)?;
    let applied = state
        .as_ref()
        .and_then(|state| state.fingerprint.clone())
        .or_else(|| cell_for(deps).applied());
    Ok(state.as_ref().is_some_and(|state| state.last_error.is_some())
        || applied.as_ref().is_some_and(|applied| applied != &disk))
}

/// Loads the on-disk config without writing anything, for `capacity reload --check`.
pub fn preview(deps: &Deps, store: &Store, now: Timestamp) -> Result<ConfigPreview, CoreError> {
    let Some(path) = config_path(deps, store)? else {
        return Err(CoreError::NotFound(
            "no project vowp.toml to reload for this project".to_owned(),
        ));
    };
    let Some(root) = path.parent() else {
        return Err(CoreError::Invalid(format!(
            "config path {} has no parent directory",
            path.display()
        )));
    };
    let fingerprint = fingerprint_at(root, deps.paths.root())?
        .ok_or_else(|| CoreError::NotFound(format!("config file disappeared: {}", path.display())))?;
    let config = load_config(deps, &path)?;
    let state = read_state(store, &deps.config.project.name)?;
    let applied = state
        .as_ref()
        .and_then(|state| state.fingerprint.clone())
        .or_else(|| cell_for(deps).applied());
    let pending = state.as_ref().is_some_and(|state| state.last_error.is_some())
        || applied.as_ref().is_some_and(|applied| applied != &fingerprint);
    let changed = changed_keys(&effective_config(deps), &config);
    let target = targets::current_with_config(&config, store, now)?;
    Ok(ConfigPreview {
        config_path: path.display().to_string(),
        provider_caps: config.provider_caps(),
        target,
        changed,
        pending,
        fingerprint,
        config,
    })
}

/// Checks and, when changed, atomically applies the on-disk config.
///
/// `force` makes the caller's explicit request attempt a parse even when the fingerprint is
/// unchanged; it still installs nothing on error. A periodic tick passes `false`. When the
/// parsed document matches the effective config, neither value swaps the cell or writes an
/// applied-reload ledger row.
pub fn reload_project(deps: &Deps, store: &Store, now: Timestamp, force: bool) -> Result<ReloadOutcome, CoreError> {
    let project = deps.config.project.name.clone();
    let Some(path) = config_path(deps, store)? else {
        let config = effective_config(deps);
        let target = targets::current_for(deps, store, now)?;
        return Ok(ReloadOutcome {
            applied: false,
            config_path: None,
            provider_caps: config.provider_caps(),
            target: Some(target),
            changed: Vec::new(),
            pending: false,
            error: None,
        });
    };
    let path_text = path.display().to_string();
    let Some(root) = path.parent() else {
        return Err(CoreError::Invalid(format!(
            "config path {path_text} has no parent directory"
        )));
    };
    let disk = fingerprint_at(root, deps.paths.root())?
        .ok_or_else(|| CoreError::NotFound(format!("config file disappeared: {path_text}")))?;
    let cell = cell_for(deps);
    let state = read_state(store, &project)?;
    let applied = state
        .as_ref()
        .and_then(|state| state.fingerprint.clone())
        .or_else(|| cell.applied());

    if applied.as_ref() == Some(&disk) && !force {
        return finish_unchanged(deps, store, now, &path, &disk, state);
    }

    let new_config = match load_config(deps, &path) {
        Ok(config) => config,
        Err(error) => {
            let old = effective_config(deps);
            let failed = ReloadState {
                config_path: Some(path_text.clone()),
                fingerprint: applied.clone(),
                provider_caps: old.provider_caps(),
                target_workers: state.as_ref().map(|state| state.target_workers).unwrap_or_default(),
                applied_at: state.as_ref().and_then(|state| state.applied_at),
                last_error: Some(error.to_string()),
            };
            write_state(store, &project, &failed)?;
            open_notice(store, &project, &path, &error)?;
            deps.log.log(
                Level::Error,
                "capacity",
                &format!("config {path_text} is invalid: {error}; keeping the previous config"),
            );
            let target = targets::current_for(deps, store, now)?;
            return Ok(ReloadOutcome {
                applied: false,
                config_path: Some(path_text),
                provider_caps: old.provider_caps(),
                target: Some(target),
                changed: Vec::new(),
                pending: true,
                error: Some(error.to_string()),
            });
        }
    };

    let old_config = effective_config(deps);
    let old_target = targets::current_with_config(&old_config, store, now)?;
    let changed = changed_keys(&old_config, &new_config);

    if *old_config == new_config {
        // The file parses, but nothing effective changed: a forced request for an
        // already-current config lands here. Record the fingerprint as applied and publish
        // the state, without swapping the cell or writing a misleading ledger row.
        cell.set_applied(disk.clone());
        return finish_unchanged(deps, store, now, &path, &disk, state);
    }

    cell.swap(Arc::new(new_config.clone()));
    cell.set_applied(disk.clone());
    let target = targets::current_with_config(&new_config, store, now)?;
    let old_caps = old_config.provider_caps();
    let new_caps = new_config.provider_caps();
    record_ledger(
        store,
        &project,
        now,
        &path,
        &old_caps,
        &new_caps,
        &old_target,
        &target,
        &changed,
    )?;
    let published = ReloadState {
        config_path: Some(path_text.clone()),
        fingerprint: Some(disk),
        provider_caps: new_caps.clone(),
        target_workers: target.target_workers,
        applied_at: Some(now),
        last_error: None,
    };
    write_state(store, &project, &published)?;
    let detail = if changed.is_empty() {
        "provider caps".to_owned()
    } else {
        changed.join(", ")
    };
    deps.log.log(
        Level::Info,
        "capacity",
        &format!(
            "config {path_text} reloaded: {detail}; target {}",
            target.target_workers
        ),
    );
    Ok(ReloadOutcome {
        applied: true,
        config_path: Some(path_text),
        provider_caps: new_caps,
        target: Some(target),
        changed,
        pending: false,
        error: None,
    })
}

/// Publishes a fresh baseline or clears a stale error when the file is already applied.
fn finish_unchanged(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    path: &Path,
    disk: &ConfigFingerprint,
    state: Option<ReloadState>,
) -> Result<ReloadOutcome, CoreError> {
    let project = deps.config.project.name.clone();
    let config = effective_config(deps);
    let target = targets::current_for(deps, store, now)?;
    let needs_publish = state.as_ref().is_none_or(|state| {
        state.last_error.is_some()
            || state.fingerprint.as_ref() != Some(disk)
            || state.provider_caps != config.provider_caps()
            || state.target_workers != target.target_workers
    });
    if needs_publish {
        let published = ReloadState {
            config_path: Some(path.display().to_string()),
            fingerprint: Some(disk.clone()),
            provider_caps: config.provider_caps(),
            target_workers: target.target_workers,
            applied_at: state.as_ref().and_then(|state| state.applied_at).or(Some(now)),
            last_error: None,
        };
        write_state(store, &project, &published)?;
    }
    Ok(ReloadOutcome {
        applied: false,
        config_path: Some(path.display().to_string()),
        provider_caps: config.provider_caps(),
        target: Some(target),
        changed: Vec::new(),
        pending: false,
        error: None,
    })
}

/// Serves every pending CLI reload request for this project.
pub fn serve_reload_requests(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError> {
    let project = deps.config.project.name.clone();
    let prefix = request_prefix(&project);
    let mut applied = 0;
    for (key, _) in store.kv_prefix(&prefix)? {
        let token = key.strip_prefix(&prefix).unwrap_or_default().to_owned();
        let outcome = match reload_project(deps, store, now, true) {
            Ok(outcome) => {
                if outcome.applied {
                    applied += 1;
                }
                outcome
            }
            Err(error) => ReloadOutcome {
                applied: false,
                config_path: None,
                provider_caps: effective_config(deps).provider_caps(),
                target: None,
                changed: Vec::new(),
                pending: true,
                error: Some(error.to_string()),
            },
        };
        let text = serde_json::to_string(&outcome).map_err(|error| CoreError::Invalid(error.to_string()))?;
        store.kv_set(&result_key(&token), &text)?;
        store.kv_delete(&key)?;
    }
    Ok(applied)
}

/// Appends one ledger note for an applied reload with old/new caps and target.
#[allow(clippy::too_many_arguments)]
fn record_ledger(
    store: &Store,
    project: &ProjectName,
    now: Timestamp,
    path: &Path,
    old_caps: &BTreeMap<String, u32>,
    new_caps: &BTreeMap<String, u32>,
    old_target: &CapacityTarget,
    new_target: &CapacityTarget,
    changed: &[String],
) -> Result<(), CoreError> {
    let entry = LedgerEntry {
        id: None,
        ts: now,
        kind: LedgerKind::Note,
        project: Some(project.clone()),
        job_id: None,
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("capacity config reloaded: {}", changed.join(", ")),
        detail: serde_json::json!({
            "path": path.display().to_string(),
            "old_provider_caps": old_caps,
            "new_provider_caps": new_caps,
            "old_target_workers": old_target.target_workers,
            "new_target_workers": new_target.target_workers,
            "changed": changed,
        }),
    };
    store.append_ledger(&entry)?;
    Ok(())
}

/// Opens one node-inbox notice for an invalid config, without duplicating an open notice.
fn open_notice(store: &Store, project: &ProjectName, path: &Path, error: &CoreError) -> Result<(), CoreError> {
    let title = format!("config reload failed for {project}");
    let already_open = store
        .list_inbox(Some(InboxState::Open))?
        .iter()
        .any(|item| item.project.as_ref() == Some(project) && item.title == title);
    if already_open {
        return Ok(());
    }
    let now = store.clock().now();
    store.insert_node_inbox(&InboxItem {
        id: InboxId::new(),
        project: Some(project.clone()),
        job_id: None,
        kind: InboxKind::Notice,
        title,
        body: format!("{}: {error}; the previous config stays in effect", path.display()),
        options: Vec::new(),
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now,
        answered_at: None,
    })?;
    Ok(())
}

/// Returns the config key paths whose values differ, with added/removed markers.
fn changed_keys(old: &VowpConfig, new: &VowpConfig) -> Vec<String> {
    let Ok(old_table) = config_table(old) else {
        return Vec::new();
    };
    let Ok(new_table) = config_table(new) else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    diff_table("", &old_table, &new_table, &mut changed);
    changed.sort();
    changed.dedup();
    changed.truncate(32);
    changed
}

/// Serialises a config to a TOML table for key comparison.
fn config_table(config: &VowpConfig) -> Result<toml::Table, CoreError> {
    let text = toml::to_string(config).map_err(|error| CoreError::Invalid(error.to_string()))?;
    toml::from_str(&text).map_err(|error| CoreError::Invalid(error.to_string()))
}

/// Recursively collects changed table keys; leaves are named by their full dotted path.
fn diff_table(prefix: &str, old: &toml::Table, new: &toml::Table, out: &mut Vec<String>) {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for key in keys {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (old.get(key), new.get(key)) {
            (Some(toml::Value::Table(old)), Some(toml::Value::Table(new))) => {
                diff_table(&path, old, new, out);
            }
            (Some(_), None) => out.push(format!("{path} (removed)")),
            (None, Some(_)) => out.push(format!("{path} (added)")),
            (Some(old), Some(new)) if old != new => out.push(path),
            _ => {}
        }
    }
}
