//! Secret storage, injection and redaction.
//!
//! Values are files, never rows: basic mode keeps them in `<VOWP_HOME>/secrets/<NAME>` owned
//! by the current user, and vault-user mode keeps them in
//! `$VOWP_VAULT_HOME/secrets/<NAME>` (default `/home/vowp-vault/secrets`) owned by
//! `vowp-vault`. Both are mode 0600. The mode file is `<VOWP_HOME>/secrets/mode`.
//!
//! Metadata lives in the global `secret_index` table: name, last four characters and
//! timestamps. Values never enter the database. Rows left by the earlier vault implementation
//! in the `kv` table under `secret:<NAME>` are migrated into `secret_index` on first access.
//!
//! Tests and packaging may override `VOWP_VAULT_HOME`, `VOWP_VAULT_SUDO` and
//! `VOWP_VAULT_HELPER`. Production uses `/home/vowp-vault`, `sudo` and the running binary.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9 and §13.2.

pub mod request;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use vowp_protocol::{
    Capsule, EventBody, HarnessKind, JobId, Level, SecretInfo, SecretName, Timestamp, TurnRequest, VaultMode,
};
use vowp_store::Store;
use vowp_store::session_files::write_capsule_file;
use vowp_store::session_log::NewEvent;

use crate::CoreError;
use crate::deps::Deps;

pub use request::{REQUEST_TTL, SecretRequestServer, request_secret};

/// Default home of the `vowp-vault` user.
const DEFAULT_VAULT_HOME: &str = "/home/vowp-vault";

/// Default installed path of the root-owned vault helper.
const DEFAULT_HELPER: &str = "/usr/local/lib/vowp/__vault-admin";

/// Minimal valid project config for the hidden helper's private node home.
///
/// The helper runs as `vowp-vault`, which cannot normally read the owner's home or project
/// checkout on a default Linux or macOS install. This config only has to satisfy `Ctx::build`
/// and `VowpConfig::validate`; it names no provider or secret.
const HELPER_CONFIG: &str = concat!(
    "[project]\n",
    "name = \"vowp-vault-helper\"\n",
    "\n",
    "[profiles.planner]\n",
    "ladder = [\"opencode:deepseek/deepseek-flash\"]\n",
    "on_quota = \"park\"\n",
    "\n",
    "[profiles.keeper]\n",
    "ladder = [\"opencode:deepseek/deepseek-flash\"]\n",
    "on_quota = \"park\"\n",
    "\n",
    "[profiles.worker]\n",
    "ladder = [\"opencode:deepseek/deepseek-flash\"]\n",
    "on_quota = \"park\"\n",
    "\n",
    "[profiles.verifier]\n",
    "ladder = [\"opencode:deepseek/deepseek-flash\"]\n",
    "on_quota = \"park\"\n",
);

/// Counter used to make atomic-write temporary names unique inside one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the node's vault mode.
///
/// A missing mode file means basic mode. Any value other than `vault` or `basic` is a
/// configuration error rather than a silent fallback.
pub fn mode(deps: &Deps) -> Result<VaultMode, CoreError> {
    let path = deps.paths.root().join("secrets").join("mode");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(VaultMode::Basic),
        Err(source) => return Err(io_error(&path, source)),
    };
    match text.trim() {
        "vault" => Ok(VaultMode::VaultUser),
        "basic" => Ok(VaultMode::Basic),
        other => Err(CoreError::Invalid(format!(
            "{} must contain `vault` or `basic`, found `{other}`",
            path.display()
        ))),
    }
}

/// Resolves exactly the configured secret names for one harness turn.
///
/// Only `[harnesses.<name>].env_pass` names are resolved; a missing secret is a warning that
/// names the variable and never a hard failure. A configured name that is not a valid
/// [`SecretName`] is skipped with a warning.
pub fn env_for(deps: &Deps, req: &TurnRequest) -> Result<Vec<(String, String)>, CoreError> {
    let executor = &req.executor.executor;
    let Some(config) = harness_config(deps, executor.harness, &executor.model) else {
        return Ok(Vec::new());
    };
    let layout = Layout::from_deps(deps);
    let mut result: Vec<(String, String)> = Vec::new();
    for configured in &config.env_pass {
        let name = match SecretName::parse(configured) {
            Ok(name) => name,
            Err(_) => {
                deps.log.log(
                    Level::Warn,
                    "vault",
                    &format!("ignoring invalid secret name in env_pass: {configured}"),
                );
                continue;
            }
        };
        if result.iter().any(|(existing, _)| existing == name.as_str()) {
            continue;
        }
        match read_value_impl(deps, &layout, &name)? {
            Some(value) => result.push((name.to_string(), value)),
            None => deps
                .log
                .log(Level::Warn, "vault", &format!("secret {name} is not set; not injected")),
        }
    }
    Ok(result)
}

/// Stores one secret value and returns its metadata.
///
/// Basic mode writes the file atomically with mode 0600; vault-user mode sends the value on
/// stdin to the hidden `__vault-admin` helper, so it never appears in argv. The index row is
/// updated either way.
pub fn write_secret(deps: &Deps, name: &SecretName, value: &str) -> Result<SecretInfo, CoreError> {
    let layout = Layout::from_deps(deps);
    write_secret_impl(deps, &layout, name, value)
}

/// Removes one secret and returns whether anything existed.
pub fn remove_secret(deps: &Deps, name: &SecretName) -> Result<bool, CoreError> {
    let layout = Layout::from_deps(deps);
    remove_secret_impl(deps, &layout, name)
}

/// Lists stored secret metadata, ordered by name. Values are never read.
pub fn list_secrets(deps: &Deps) -> Result<Vec<SecretInfo>, CoreError> {
    list_index(deps)
}

/// Redacts exact secret values from one event before it is appended.
pub fn redact_event(deps: &Deps, ev: NewEvent) -> Result<NewEvent, CoreError> {
    let redactor = Redactor::load(deps)?;
    if redactor.is_empty() {
        return Ok(ev);
    }
    let NewEvent {
        actor,
        source,
        body,
        raw,
        ts,
    } = ev;
    let event_type = body.event_type();
    let mut body_value = body.to_value();
    redactor.redact_value(&mut body_value);
    let body = EventBody::from_parts(event_type, body_value)?;
    let raw = raw.map(|mut value| {
        redactor.redact_value(&mut value);
        value
    });
    Ok(NewEvent {
        actor,
        source,
        body,
        raw,
        ts,
    })
}

/// Redacts exact secret values from plain text.
///
/// If the secrets cannot be loaded, the whole text is replaced instead of risking a leak;
/// the error is logged without a value.
pub fn redact_text(deps: &Deps, text: &str) -> String {
    match Redactor::load(deps) {
        Ok(redactor) => redactor.redact(text),
        Err(error) => {
            deps.log
                .log(Level::Error, "vault", &format!("redaction unavailable: {error}"));
            "[REDACTED: secret values unavailable]".to_owned()
        }
    }
}

/// Redacts exact secret values from a capsule and rewrites its file and row.
pub fn redact_capsule(deps: &Deps, store: &Store, capsule: &Capsule) -> Result<Capsule, CoreError> {
    let redactor = Redactor::load(deps)?;
    if redactor.is_empty() {
        return Ok(capsule.clone());
    }
    let mut value = serde_json::to_value(capsule).map_err(|error| CoreError::Invalid(error.to_string()))?;
    redactor.redact_value(&mut value);
    let redacted: Capsule = serde_json::from_value(value).map_err(|error| CoreError::Invalid(error.to_string()))?;
    if &redacted == capsule {
        return Ok(capsule.clone());
    }
    write_capsule_file(&deps.paths, &redacted)?;
    store.update_capsule_body(&redacted)?;
    Ok(redacted)
}

/// Redacts exact secret values from the job artifacts of the last turn.
///
/// Both the job's `report.md` (in its worktree and in the project checkout when that is a
/// different path) and the latest run's raw mirror are covered.
pub fn redact_job_artifacts(deps: &Deps, store: &Store, job: Option<&JobId>) -> Result<(), CoreError> {
    let Some(job_id) = job else {
        return Ok(());
    };
    let Some(job) = store.job(job_id)? else {
        return Ok(());
    };
    let redactor = Redactor::load(deps)?;
    if redactor.is_empty() {
        return Ok(());
    }
    let mut paths = BTreeSet::new();
    let report = Path::new(&job.report);
    if report.is_absolute() {
        paths.insert(report.to_path_buf());
    } else {
        if let Some(worktree) = &job.worktree {
            paths.insert(Path::new(worktree).join(report));
        }
        if let Some(project) = store.project(&job.project)? {
            paths.insert(Path::new(&project.path).join(report));
        }
    }
    if let Some(run) = store.last_run_of_job(&job.id)? {
        paths.insert(deps.paths.run_raw_path(&run.id));
    }
    for path in paths {
        redact_file(&path, &redactor)?;
    }
    Ok(())
}

/// Reads one secret for the hidden `__vault-exec` helper.
///
/// Returns `None` when the name is not stored. The hidden helper runs as `vowp-vault` and
/// cannot read the mode file in the `vowp` user's home, so it tries the vault directory
/// directly before falling back to a normal mode-aware read.
pub fn read_value_for_exec(deps: &Deps, name: &SecretName) -> Result<Option<String>, CoreError> {
    let layout = Layout::from_deps(deps);
    match read_value_at(&layout.vault_dir(), name) {
        Ok(Some(value)) => return Ok(Some(value)),
        Ok(None) => {}
        Err(CoreError::Io { path: _, source }) if source.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error),
    }
    read_value_impl(deps, &layout, name)
}

/// Writes one secret file as the `vowp-vault` user.
///
/// This is the body of the hidden `__vault-admin set` helper; it always writes the vault
/// directory and never touches the index (the calling `vowp` process owns that row).
pub fn admin_set(deps: &Deps, name: &SecretName, value: &str) -> Result<SecretInfo, CoreError> {
    let layout = Layout::from_deps(deps);
    admin_set_at(&layout.vault_dir(), name, value, deps.clock.now())
}

/// Removes one secret file as the `vowp-vault` user.
pub fn admin_remove(deps: &Deps, name: &SecretName) -> Result<bool, CoreError> {
    let layout = Layout::from_deps(deps);
    admin_remove_at(&layout.vault_dir(), name)
}

/// Checks that the helper can write and clean up inside the vault directory.
///
/// This is the readiness probe: it exercises the real helper context and directory without
/// touching any secret name.
pub fn admin_probe(deps: &Deps) -> Result<(), CoreError> {
    let layout = Layout::from_deps(deps);
    admin_probe_at(&layout.vault_dir())
}

/// What `vowp secret upgrade` should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upgrade {
    /// Secrets are already stored as the vault user and the helper can run.
    AlreadyVault,
    /// The vault user was found and the mode file was switched.
    Switched,
    /// The owner must run this one shell command, then re-run `vowp secret upgrade`.
    NeedsCommand(String),
    /// The `sudo` program is missing, so the vault user cannot be created here.
    NoSudo(String),
}

/// Plans or completes the switch from basic mode to vault-user mode.
///
/// When the vault user and helper are already ready, every basic secret is moved into the
/// vault through the passwordless sudo helper and then the mode file is written. Otherwise the
/// exact command the owner must run is returned and local state is left unchanged.
pub fn upgrade(deps: &Deps) -> Result<Upgrade, CoreError> {
    let layout = Layout::from_deps(deps);
    upgrade_impl(deps, &layout)
}

/// Completes the upgrade through an explicit layout (also used by the module tests).
fn upgrade_impl(deps: &Deps, layout: &Layout) -> Result<Upgrade, CoreError> {
    let current = mode(deps)?;
    if vault_user_ready(layout) {
        if current == VaultMode::VaultUser {
            return Ok(Upgrade::AlreadyVault);
        }
        migrate_basic_to_vault(layout)?;
        write_mode(deps, VaultMode::VaultUser)?;
        return Ok(Upgrade::Switched);
    }
    if sudo_present(layout) {
        Ok(Upgrade::NeedsCommand(upgrade_command(layout)))
    } else {
        Ok(Upgrade::NoSudo(
            "sudo is not installed; install it and re-run `vowp secret upgrade` to get the vault setup command"
                .to_owned(),
        ))
    }
}

/// Copies every basic-mode secret into the vault through the helper, then removes the copy.
fn migrate_basic_to_vault(layout: &Layout) -> Result<u32, CoreError> {
    let entries = match fs::read_dir(&layout.basic_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(io_error(&layout.basic_dir, source)),
    };
    let mut moved = 0;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(&layout.basic_dir, source))?;
        let Ok(name) = SecretName::parse(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let value = read_value_at(&layout.basic_dir, &name)?
            .ok_or_else(|| CoreError::Invalid(format!("secret {name} disappeared during upgrade")))?;
        let mut command = helper_command(layout, &["__vault-admin", "set", name.as_str()]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_helper_with_value(command, value.as_bytes(), "migrate")?;
        remove_file_if_present(&layout.basic_dir.join(name.as_str()))?;
        moved += 1;
    }
    Ok(moved)
}

/// The exact-value redactor. Loads the stored values, longest first.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    values: Vec<(String, String)>,
}

impl Redactor {
    /// Loads the configured secret values, longest first.
    pub fn load(deps: &Deps) -> Result<Redactor, CoreError> {
        let layout = Layout::from_deps(deps);
        let mode = mode(deps)?;
        let mut names: BTreeSet<String> = BTreeSet::new();
        match mode {
            // Basic storage is complete on disk; avoid a store query on every event.
            VaultMode::Basic => collect_secret_names(&layout.basic_dir, &mut names),
            VaultMode::VaultUser => {
                names.extend(list_index(deps)?.into_iter().map(|info| info.name.to_string()));
                for dir in storage_dirs(&layout, mode) {
                    collect_secret_names(&dir, &mut names);
                }
            }
        }
        let mut values = Vec::new();
        for name in names {
            let name = SecretName::parse(&name)?;
            if let Some(value) = read_value_impl(deps, &layout, &name)? {
                if !value.is_empty() {
                    values.push((name.to_string(), value));
                }
            }
        }
        values.sort_by(|left, right| right.1.len().cmp(&left.1.len()).then_with(|| left.0.cmp(&right.0)));
        Ok(Redactor { values })
    }

    /// Replaces every exact occurrence with `[REDACTED:<NAME>]`.
    pub fn redact(&self, text: &str) -> String {
        let mut result = text.to_owned();
        for (name, value) in &self.values {
            if value.is_empty() {
                continue;
            }
            result = result.replace(value, &format!("[REDACTED:{name}]"));
        }
        result
    }

    /// Returns true when no secret value is loaded.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Redacts string values inside one JSON value without touching object keys.
    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_value(item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// Redacts exact byte sequences, for artifacts that may not be valid UTF-8.
    fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let mut result = bytes.to_vec();
        for (name, value) in &self.values {
            if value.is_empty() {
                continue;
            }
            result = replace_bytes(&result, value.as_bytes(), format!("[REDACTED:{name}]").as_bytes());
        }
        result
    }
}

/// Basic and vault storage directories for one effective mode.
fn storage_dirs(layout: &Layout, mode: VaultMode) -> Vec<PathBuf> {
    match mode {
        VaultMode::Basic => vec![layout.basic_dir.clone()],
        VaultMode::VaultUser => vec![layout.basic_dir.clone(), layout.vault_dir()],
    }
}

/// Adds every valid secret file name in `dir` to `names`, ignoring unreadable directories.
fn collect_secret_names(dir: &Path, names: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Ok(name) = SecretName::parse(&entry.file_name().to_string_lossy()) {
            names.insert(name.to_string());
        }
    }
}

/// The on-disk locations and helper programs used by one vault operation.
#[derive(Debug, Clone)]
struct Layout {
    basic_dir: PathBuf,
    vault_home: PathBuf,
    sudo: PathBuf,
    helper: PathBuf,
}

impl Layout {
    /// Builds the layout from the node home and the documented environment overrides.
    fn from_deps(deps: &Deps) -> Self {
        let vault_home = std::env::var_os("VOWP_VAULT_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_VAULT_HOME));
        let sudo = std::env::var_os("VOWP_VAULT_SUDO")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("sudo"));
        let helper = std::env::var_os("VOWP_VAULT_HELPER")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let installed = PathBuf::from(DEFAULT_HELPER);
                installed.is_file().then_some(installed)
            })
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("vowp"));
        Self {
            basic_dir: deps.paths.root().join("secrets"),
            vault_home,
            sudo,
            helper,
        }
    }

    /// Returns the vault user's secret directory.
    fn vault_dir(&self) -> PathBuf {
        self.vault_home.join("secrets")
    }

    /// Returns the private node home the hidden helper receives instead of the owner's home.
    fn helper_home(&self) -> PathBuf {
        self.vault_home.join("helper")
    }
}

/// Stores one secret through an explicit layout (also used by the module tests).
fn write_secret_impl(deps: &Deps, layout: &Layout, name: &SecretName, value: &str) -> Result<SecretInfo, CoreError> {
    let name = SecretName::parse(name.as_str())?;
    if value.is_empty() {
        return Err(CoreError::Invalid("secret value must not be empty".to_owned()));
    }
    let now = deps.clock.now();
    let created_at = list_index(deps)?
        .into_iter()
        .find(|info| info.name == name)
        .map(|info| info.created_at)
        .unwrap_or(now);
    match mode(deps)? {
        VaultMode::Basic => {
            ensure_private_dir(&layout.basic_dir)?;
            write_atomic(&layout.basic_dir.join(name.as_str()), value.as_bytes(), 0o600)?;
        }
        VaultMode::VaultUser => {
            let mut command = helper_command(layout, &["__vault-admin", "set", name.as_str()]);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            run_helper_with_value(command, value.as_bytes(), "set")?;
        }
    }
    let info = SecretInfo {
        name,
        last4: last4(value),
        created_at,
        updated_at: now,
    };
    put_index(deps, &info)?;
    Ok(info)
}

/// Removes one secret through an explicit layout.
fn remove_secret_impl(deps: &Deps, layout: &Layout, name: &SecretName) -> Result<bool, CoreError> {
    let name = SecretName::parse(name.as_str())?;
    let indexed = list_index(deps)?.into_iter().any(|info| info.name == name);
    let file_existed = match mode(deps)? {
        VaultMode::Basic => remove_file_if_present(&layout.basic_dir.join(name.as_str()))?,
        VaultMode::VaultUser => {
            let command = helper_command(layout, &["__vault-admin", "rm", name.as_str()]);
            let output = run_helper_capture(command, None, "rm")?;
            String::from_utf8_lossy(&output).trim_start().starts_with("removed")
        }
    };
    let removed_index = remove_index(deps, &name)?;
    Ok(file_existed || removed_index || indexed)
}

/// Reads one secret file, falling back to the sudo helper in vault mode.
fn read_value_impl(deps: &Deps, layout: &Layout, name: &SecretName) -> Result<Option<String>, CoreError> {
    match mode(deps)? {
        VaultMode::Basic => read_value_at(&layout.basic_dir, name),
        VaultMode::VaultUser => match read_value_at(&layout.vault_dir(), name) {
            Ok(value) => Ok(value),
            Err(CoreError::Io { path: _, source }) if source.kind() == std::io::ErrorKind::PermissionDenied => {
                read_via_helper(layout, name).map_err(|error| {
                    deps.log.log(
                        Level::Error,
                        "vault",
                        &format!("secret {name} is in the vault but could not be read: {error}"),
                    );
                    CoreError::Invalid(format!("secret {name} is not readable through the vault helper"))
                })
            }
            Err(error) => Err(error),
        },
    }
}

/// Reads a vault value through `__vault-exec` running as `vowp-vault`.
fn read_via_helper(layout: &Layout, name: &SecretName) -> Result<Option<String>, CoreError> {
    let mut command = helper_command(layout, &["__vault-exec", "--env", name.as_str()]);
    command
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!("printf %s \"${{{name}}}\""));
    let output = run_helper_capture(command, Some(b""), "read")?;
    let value = String::from_utf8(output).map_err(|_| CoreError::Invalid(format!("secret {name} is not UTF-8")))?;
    Ok(Some(value))
}

/// Writes one secret file into `dir`; the helper and tests share this implementation.
fn admin_set_at(dir: &Path, name: &SecretName, value: &str, now: Timestamp) -> Result<SecretInfo, CoreError> {
    let name = SecretName::parse(name.as_str())?;
    if value.is_empty() {
        return Err(CoreError::Invalid("secret value must not be empty".to_owned()));
    }
    ensure_private_dir(dir)?;
    write_atomic(&dir.join(name.as_str()), value.as_bytes(), 0o600)?;
    Ok(SecretInfo {
        name,
        last4: last4(value),
        created_at: now,
        updated_at: now,
    })
}

/// Removes one secret file from `dir`; the helper and tests share this implementation.
fn admin_remove_at(dir: &Path, name: &SecretName) -> Result<bool, CoreError> {
    let name = SecretName::parse(name.as_str())?;
    remove_file_if_present(&dir.join(name.as_str()))
}

/// Creates and removes one private probe file in `dir`; the helper and tests share this.
fn admin_probe_at(dir: &Path) -> Result<(), CoreError> {
    ensure_private_dir(dir)?;
    let name = format!(
        ".vowp-probe-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|source| io_error(&path, source))?;
    drop(file);
    fs::remove_file(&path).map_err(|source| io_error(&path, source))
}

/// Reads one secret file from an explicit directory.
fn read_value_at(dir: &Path, name: &SecretName) -> Result<Option<String>, CoreError> {
    let path = dir.join(name.as_str());
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(&path, source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(CoreError::Invalid(format!(
            "secret file {} must not be a symlink",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(CoreError::Invalid(format!(
            "secret path {} is not a file",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CoreError::Invalid(format!(
            "secret file {} must be mode 0600",
            path.display()
        )));
    }
    let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
    let value = String::from_utf8(bytes)
        .map_err(|_| CoreError::Invalid(format!("secret file {} is not valid UTF-8", path.display())))?;
    Ok(Some(value))
}

/// Writes the mode file atomically with mode 0600.
fn write_mode(deps: &Deps, mode: VaultMode) -> Result<(), CoreError> {
    let dir = deps.paths.root().join("secrets");
    ensure_private_dir(&dir)?;
    let text = match mode {
        VaultMode::VaultUser => "vault\n",
        VaultMode::Basic => "basic\n",
    };
    write_atomic(&dir.join("mode"), text.as_bytes(), 0o600)
}

/// Returns whether the installed helper runs as the vault user through passwordless sudo.
fn vault_user_ready(layout: &Layout) -> bool {
    layout.vault_home.is_dir() && layout.helper.is_file() && run_vault_probe(layout)
}

/// Returns whether the `sudo` program exists, whether or not it asks for a password.
///
/// The common upgrade path is a machine without passwordless sudo: the owner types a password
/// when running the printed command. Only a missing `sudo` makes the upgrade impossible here.
fn sudo_present(layout: &Layout) -> bool {
    let sudo = &layout.sudo;
    if sudo.as_os_str().is_empty() {
        return false;
    }
    if sudo.is_absolute() || sudo.components().count() > 1 {
        return is_executable(sudo);
    }
    std::env::var_os("PATH").is_some_and(|paths| executable_in_paths(sudo, &paths))
}

/// Returns whether `name` names an executable in one `PATH`-style list.
fn executable_in_paths(name: &Path, paths: &std::ffi::OsStr) -> bool {
    std::env::split_paths(paths).any(|dir| is_executable(&dir.join(name)))
}

/// Returns whether one path names an executable regular file.
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Runs the installed helper's harmless real operation as `vowp-vault`.
///
/// `--version` is handled by clap before `Ctx::build`, so it does not prove that the helper can
/// run as the vault user. `__vault-admin probe` exercises the same context and vault directory
/// as the real `set`/`rm` operations without touching a secret.
fn run_vault_probe(layout: &Layout) -> bool {
    let mut command = helper_command(layout, &["__vault-admin", "probe"]);
    run_status(&mut command)
}

/// Runs one status probe with all streams discarded.
fn run_status(command: &mut Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Shell body that moves every basic-mode secret into the vault directory.
///
/// The caller passes the basic and vault directories as `$1` and `$2`. Every step exits
/// non-zero on failure, so the surrounding `&&` chain stops before the sudoers line is written.
const MIGRATE_SCRIPT: &str = "for f in \"$1\"/[A-Z]*; do [ -e \"$f\" ] || continue; \
     n=${f##*/}; mv -f \"$f\" \"$2/$n\" || exit 1; \
     chown vowp-vault:vowp-vault \"$2/$n\" || exit 1; \
     chmod 0600 \"$2/$n\" || exit 1; \
     done";

/// Returns the one command the owner runs to create the vault user and move basic secrets.
fn upgrade_command(layout: &Layout) -> String {
    let basic = shell_quote(&layout.basic_dir);
    let vault = shell_quote(&layout.vault_dir());
    let helper = shell_quote(&PathBuf::from(DEFAULT_HELPER));
    let source = shell_quote(&layout.helper);
    let home = shell_quote(&layout.vault_home);
    let helper_home = layout.helper_home();
    let helper_home_arg = shell_quote(&helper_home);
    let helper_config = shell_quote(&helper_home.join("vowp.toml"));
    let helper_db = shell_quote(&helper_home.join("vowp.db"));
    let helper_config_text = HELPER_CONFIG
        .lines()
        .map(shell_quote_text)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "id -u vowp-vault >/dev/null 2>&1 || sudo useradd --system --home {home} --shell /usr/sbin/nologin vowp-vault \
         && sudo install -d -m 0700 -o vowp-vault -g vowp-vault {vault} \
         && sudo install -d -m 0700 -o vowp-vault -g vowp-vault {helper_home_arg} \
         && sudo install -d -m 0755 /usr/local/lib/vowp \
         && if [ {source} -ef {helper} ]; then true; else sudo install -m 0755 -o root -g root {source} {helper}; fi \
         && printf '%s\\n' {helper_config_text} | sudo tee {helper_config} >/dev/null \
         && sudo install -m 0600 -o vowp-vault -g vowp-vault /dev/null {helper_db} \
         && sudo chown vowp-vault:vowp-vault {helper_config} {helper_db} \
         && sudo chmod 0600 {helper_config} {helper_db} \
         && sudo sh -c '{MIGRATE_SCRIPT}' vowp-migrate {basic} {vault} \
         && echo 'vowp ALL=(vowp-vault) NOPASSWD: {helper}' | sudo tee /etc/sudoers.d/vowp-vault >/dev/null \
         && sudo chmod 0440 /etc/sudoers.d/vowp-vault"
    )
}

/// Quotes one path for a POSIX shell.
fn shell_quote(text: &Path) -> String {
    shell_quote_text(&text.display().to_string())
}

/// Quotes one text argument for a POSIX shell.
fn shell_quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Builds the sudo helper command with the helper's private home and project.
///
/// The flags must come before the hidden subcommand and before any `--` argument separator:
/// `VaultExecArgs.argv` is `last = true`, so anything after `--` belongs to the child command.
/// The helper gets its own minimal node home and project, so it never needs access to the
/// owner's home or checkout as `vowp-vault`.
fn helper_command(layout: &Layout, args: &[&str]) -> Command {
    let helper_home = layout.helper_home();
    let mut command = Command::new(&layout.sudo);
    command
        .arg("-n")
        .arg("-u")
        .arg("vowp-vault")
        .arg(&layout.helper)
        .arg("--home")
        .arg(&helper_home)
        .arg("--project")
        .arg(&helper_home);
    command.args(args);
    command
}

/// Runs a helper with a value on stdin and rejects a non-zero exit.
fn run_helper_with_value(mut command: Command, value: &[u8], action: &str) -> Result<(), CoreError> {
    let mut child = command.spawn().map_err(|source| CoreError::Io {
        path: "vowp-vault helper".to_owned(),
        source,
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(value);
    }
    let output = child.wait_with_output().map_err(|source| CoreError::Io {
        path: "vowp-vault helper".to_owned(),
        source,
    })?;
    if !output.status.success() {
        return Err(helper_failure(action, output.status.code(), &output.stderr));
    }
    Ok(())
}

/// Runs a helper and returns its stdout, rejecting a non-zero exit.
fn run_helper_capture(mut command: Command, stdin: Option<&[u8]>, action: &str) -> Result<Vec<u8>, CoreError> {
    command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|source| CoreError::Io {
        path: "vowp-vault helper".to_owned(),
        source,
    })?;
    if let Some(value) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(value);
    }
    let output = child.wait_with_output().map_err(|source| CoreError::Io {
        path: "vowp-vault helper".to_owned(),
        source,
    })?;
    if !output.status.success() {
        return Err(helper_failure(action, output.status.code(), &output.stderr));
    }
    Ok(output.stdout)
}

/// Builds a value-free helper error from the exit status and a short stderr tail.
fn helper_failure(action: &str, code: Option<i32>, stderr: &[u8]) -> CoreError {
    let detail = String::from_utf8_lossy(stderr);
    let detail: String = detail.trim().chars().take(300).collect();
    CoreError::Command(format!(
        "vowp-vault helper {action} failed (exit {code:?}){}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    ))
}

/// Ensures `dir` exists with mode 0700.
fn ensure_private_dir(dir: &Path) -> Result<(), CoreError> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| io_error(dir, source))?;
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| io_error(dir, source))
}

/// Writes `bytes` through a temporary file in the same directory, fsyncs and renames.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), CoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| CoreError::Invalid(format!("{} has no parent directory", path.display())))?;
    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "secret".to_owned());
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{file_name}.tmp.{}.{counter}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(mode);
    let mut file = options.open(&temp).map_err(|source| io_error(&temp, source))?;
    let result = (|| -> Result<(), CoreError> {
        file.write_all(bytes).map_err(|source| io_error(&temp, source))?;
        file.sync_all().map_err(|source| io_error(&temp, source))?;
        drop(file);
        fs::rename(&temp, path).map_err(|source| io_error(path, source))?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Removes a file when it exists and returns whether it did.
fn remove_file_if_present(path: &Path) -> Result<bool, CoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(path, source)),
    }
}

/// Replaces every occurrence of `needle` in `haystack`, including overlapping-free repeats.
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut result = Vec::with_capacity(haystack.len());
    let mut index = 0;
    while index < haystack.len() {
        if haystack[index..].starts_with(needle) {
            result.extend_from_slice(replacement);
            index += needle.len();
        } else {
            result.push(haystack[index]);
            index += 1;
        }
    }
    result
}

/// Redacts one artifact file when it exists; permission bits are preserved.
fn redact_file(path: &Path, redactor: &Redactor) -> Result<(), CoreError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
    };
    if !metadata.is_file() {
        return Ok(());
    }
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    let redacted = redactor.redact_bytes(&bytes);
    if redacted == bytes {
        return Ok(());
    }
    write_atomic(path, &redacted, metadata.permissions().mode() & 0o777)
}

/// Returns the last four characters of a value.
fn last4(value: &str) -> String {
    let chars: Vec<char> = value.chars().rev().take(4).collect();
    chars.into_iter().rev().collect()
}

/// Returns the harness config used for `executor`, matching the session service's lookup.
fn harness_config<'a>(deps: &'a Deps, harness: HarnessKind, model: &str) -> Option<&'a vowp_protocol::HarnessConfig> {
    let key = match harness {
        HarnessKind::ClaudeCode => "claude-code",
        HarnessKind::Opencode => "opencode",
        HarnessKind::Cli => model,
        HarnessKind::Vowp => return None,
    };
    deps.config
        .harnesses
        .get(key)
        .or_else(|| deps.config.harnesses.get(&format!("cli:{key}")))
}

/// Opens the global store for index rows, migrating any legacy `kv` metadata first.
fn index_store(deps: &Deps) -> Result<Store, CoreError> {
    let store = Store::open_global(&deps.paths)?.with_clock(deps.clock.clone());
    store.migrate_secret_kv_to_index()?;
    Ok(store)
}

/// Lists index rows, ordered by name.
fn list_index(deps: &Deps) -> Result<Vec<SecretInfo>, CoreError> {
    Ok(index_store(deps)?.list_secret_index()?)
}

/// Inserts or replaces one index row.
fn put_index(deps: &Deps, info: &SecretInfo) -> Result<(), CoreError> {
    index_store(deps)?.upsert_secret_index(&info.name, &info.last4, info.updated_at)?;
    Ok(())
}

/// Removes one index row and returns whether it existed.
fn remove_index(deps: &Deps, name: &SecretName) -> Result<bool, CoreError> {
    Ok(index_store(deps)?.remove_secret_index(name)?)
}

/// Maps one filesystem error onto the core error with its path.
fn io_error(path: &Path, source: std::io::Error) -> CoreError {
    CoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use tempfile::tempdir;
    use vowp_protocol::{FakeClock, MemoryLog, NodeName, VowpConfig};
    use vowp_store::NodePaths;
    use vowp_tmux::fake::FakeWindows;

    use super::*;
    use crate::deps::{AdapterRegistry, FixedRng, SystemRunner};

    fn now() -> Timestamp {
        Timestamp::parse("2026-11-10T12:00:00Z").expect("timestamp")
    }

    fn deps(root: &Path) -> Deps {
        let paths = NodePaths::new(root);
        paths.ensure().expect("node paths");
        Store::open(&paths).expect("store");
        Deps {
            paths,
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(FakeClock::at(now())),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(crate::SystemGit::default()),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        }
    }

    fn layout_with(root: &Path) -> Layout {
        Layout {
            basic_dir: root.join("secrets"),
            vault_home: root.join("vault-home"),
            sudo: root.join("fake-sudo"),
            helper: root.join("vowp"),
        }
    }

    fn fake_sudo(layout: &Layout) {
        let vault = layout.vault_home.join("secrets");
        fs::create_dir_all(&vault).expect("vault dir");
        let helper = "#!/bin/sh\nexit 0\n";
        fs::write(&layout.helper, helper).expect("helper file");
        fs::set_permissions(&layout.helper, fs::Permissions::from_mode(0o755)).expect("helper mode");
        let body = format!(
            "#!/bin/sh\n\
             umask 077\n\
             log=\"{}/argv.log\"\n\
             printf '%s\\n' \"$*\" >> \"$log\"\n\
             shift 3 || exit 0\n\
             helper=\"$1\"; shift || exit 0\n\
             while [ $# -gt 0 ]; do\n\
             case \"$1\" in\n\
             --home|--project) shift 2 ;;\n\
             *) break ;;\n\
             esac\n\
             done\n\
             cmd=\"$1\"; shift || true\n\
             case \"$cmd\" in\n\
             --version|true) exit 0 ;;\n\
             __vault-admin)\n\
             action=\"$1\"\n\
             name=\"$2\"\n\
             if [ \"$action\" = probe ]; then exit 0; elif [ \"$action\" = set ]; then cat > \"{}/$name\"; echo \"saved $name\"; else rm -f \"{}/$name\"; echo \"removed $name\"; fi\n\
             ;;\n\
             __vault-exec)\n\
             shift\n\
             name=\"$1\"\n\
             shift\n\
             while [ \"$1\" != \"--\" ]; do shift; done\n\
             shift\n\
             value=$(cat \"{}/$name\")\n\
             exec env \"$name=$value\" \"$@\"\n\
             ;;\n\
             esac\n",
            layout.vault_home.display(),
            vault.display(),
            vault.display(),
            vault.display(),
        );
        fs::write(&layout.sudo, body).expect("fake sudo");
        fs::set_permissions(&layout.sudo, fs::Permissions::from_mode(0o755)).expect("fake sudo mode");
    }

    #[test]
    fn helper_command_puts_global_flags_before_the_subcommand() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        let command = helper_command(&layout, &["__vault-exec", "--env", "NAME"]);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let home = args.iter().position(|arg| arg == "--home").expect("--home");
        let project = args.iter().position(|arg| arg == "--project").expect("--project");
        let subcommand = args.iter().position(|arg| arg == "__vault-exec").expect("subcommand");
        assert!(
            home < subcommand,
            "global flags must precede the hidden subcommand: {args:?}"
        );
        assert!(
            project < subcommand,
            "the project flag must precede the subcommand: {args:?}"
        );
        let helper_home = layout.helper_home().display().to_string();
        assert_eq!(args[home + 1], helper_home);
        assert_eq!(args[project + 1], helper_home);
        assert!(
            !args.contains(&deps.paths.root().display().to_string()),
            "the helper must not receive the owner's home: {args:?}"
        );
    }

    #[test]
    fn sudo_present_finds_a_bare_name_on_path() {
        let tmp = tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).expect("fake bin");
        fake_tool(&bin, "sudo", "#!/bin/sh\nexit 0\n");
        let paths = std::env::join_paths([&bin]).expect("path list");

        assert!(executable_in_paths(Path::new("sudo"), &paths));
        assert!(!executable_in_paths(Path::new("other-sudo"), &paths));
    }

    #[test]
    fn helper_config_is_a_valid_project_config() {
        let config = VowpConfig::from_toml(HELPER_CONFIG).expect("helper config parses");
        assert_eq!(config.project.name.as_str(), "vowp-vault-helper");
        assert!(config.profiles.len() >= 4);
    }

    #[test]
    fn vault_user_ready_probes_a_real_helper_operation() {
        let tmp = tempdir().expect("tempdir");
        let layout = layout_with(tmp.path());
        fake_sudo(&layout);

        assert!(vault_user_ready(&layout));
        let argv = fs::read_to_string(layout.vault_home.join("argv.log")).expect("argv log");
        assert!(
            argv.contains("__vault-admin probe"),
            "the probe must run a real helper operation: {argv}"
        );
        assert!(
            argv.contains(&layout.helper_home().display().to_string()),
            "the probe must use the helper's private context: {argv}"
        );
    }

    #[test]
    fn vault_user_ready_rejects_a_helper_that_only_answers_version() {
        let tmp = tempdir().expect("tempdir");
        let layout = layout_with(tmp.path());
        fake_sudo(&layout);
        // The old probe ran `--version`, which clap handles before Ctx::build.
        fake_tool(
            tmp.path(),
            "fake-sudo",
            "#!/bin/sh\ncase \"$*\" in *--version*) exit 0 ;; *) exit 1 ;; esac\n",
        );

        assert!(
            !vault_user_ready(&layout),
            "a version-only helper must not count as ready"
        );
    }

    #[test]
    fn already_vault_requires_a_ready_helper() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        fake_sudo(&layout);
        write_mode(&deps, VaultMode::VaultUser).expect("mode");

        assert_eq!(upgrade_impl(&deps, &layout).expect("upgrade"), Upgrade::AlreadyVault);
    }

    #[test]
    fn vault_mode_with_an_unready_helper_prints_the_command() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        fake_tool(tmp.path(), "fake-sudo", "#!/bin/sh\nexit 1\n");
        write_mode(&deps, VaultMode::VaultUser).expect("mode");

        let outcome = upgrade_impl(&deps, &layout).expect("upgrade");

        assert!(
            matches!(outcome, Upgrade::NeedsCommand(_)),
            "a vault mode with no usable helper must not report ready: {outcome:?}"
        );
        assert_eq!(mode(&deps).expect("mode"), VaultMode::VaultUser);
    }

    #[test]
    fn upgrade_moves_basic_secrets_into_the_vault() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        fake_sudo(&layout);
        let name = SecretName::parse("UPGRADE_KEY").expect("name");
        let value = "basic-upgrade-value-1357";
        write_secret_impl(&deps, &layout, &name, value).expect("basic write");
        assert!(layout.basic_dir.join("UPGRADE_KEY").is_file());

        let outcome = upgrade_impl(&deps, &layout).expect("upgrade");

        assert_eq!(outcome, Upgrade::Switched);
        assert!(
            !layout.basic_dir.join("UPGRADE_KEY").exists(),
            "the basic copy must be moved"
        );
        assert_eq!(
            fs::read_to_string(layout.vault_dir().join("UPGRADE_KEY")).expect("vault file"),
            value
        );
        assert_eq!(mode(&deps).expect("mode"), VaultMode::VaultUser);
        assert_eq!(
            read_via_helper(&layout, &name).expect("helper read"),
            Some(value.to_owned())
        );
    }

    /// Writes one fake executable used to run the printed upgrade command hermetically.
    fn fake_tool(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).expect("fake tool");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fake tool mode");
    }

    /// Installs hermetic fakes for every privileged command in the upgrade hand-off.
    fn fake_upgrade_tools(dir: &Path) {
        let sudo = "#!/bin/sh\n\
                    while [ \"$#\" -gt 0 ]; do\n\
                    case \"$1\" in\n\
                    -n) shift ;;\n\
                    -u) shift 2 ;;\n\
                    *) break ;;\n\
                    esac\n\
                    done\n\
                    exec \"$@\"\n";
        fake_tool(dir, "sudo", sudo);
        fake_tool(dir, "useradd", "#!/bin/sh\nexit 0\n");
        let install = "#!/bin/sh\n\
                       make_dir=false\n\
                       last=\n\
                       for arg in \"$@\"; do\n\
                       [ \"$arg\" = \"-d\" ] && make_dir=true\n\
                       last=$arg\n\
                       done\n\
                       if [ \"$make_dir\" = true ]; then\n\
                       if [ \"$VOWP_TEST_CREATE_VAULT\" = 1 ]; then\n\
                       case \"$last\" in\n\
                       \"$VOWP_TEST_ROOT\"/*) mkdir -p \"$last\" ;;\n\
                       esac\n\
                       fi\n\
                       else\n\
                       case \"$last\" in\n\
                       \"$VOWP_TEST_ROOT\"/*) : > \"$last\" ;;\n\
                       esac\n\
                       fi\n\
                       exit 0\n";
        fake_tool(dir, "install", install);
        let tee = "#!/bin/sh\n\
                   last=\n\
                   for arg in \"$@\"; do last=$arg; done\n\
                   case \"$last\" in\n\
                   \"$VOWP_TEST_ROOT\"/*) cat > \"$last\" ;;\n\
                   *) cat >/dev/null ;;\n\
                   esac\n\
                   exit 0\n";
        fake_tool(dir, "tee", tee);
        fake_tool(dir, "chown", "#!/bin/sh\nexit 0\n");
        let chmod = "#!/bin/sh\n\
                     case \"$2\" in\n\
                     \"$VOWP_TEST_ROOT\"/*) exec /bin/chmod \"$@\" ;;\n\
                     esac\n\
                     exit 0\n";
        fake_tool(dir, "chmod", chmod);
    }

    /// Returns true when the migrate glob `[A-Z]*` matches only uppercase names here.
    ///
    /// Bracket ranges are platform-dependent: macOS's default case-insensitive filesystem and
    /// UTF-8 locale also expand `[A-Z]` to lowercase names. The upgrade test can only assert
    /// that the lowercase `mode` marker stays behind where the glob is actually case-sensitive.
    fn migrate_glob_is_case_sensitive(dir: &Path) -> bool {
        let probe = dir.join("vowp-glob-probe");
        fs::write(&probe, b"").expect("glob probe");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg("for f in \"$1\"/[A-Z]*; do [ -e \"$f\" ] || continue; printf '%s\\n' \"${f##*/}\"; done")
            .arg("sh")
            .arg(dir)
            .output()
            .expect("run glob probe");
        fs::remove_file(&probe).expect("remove glob probe");
        assert!(output.status.success(), "the glob probe failed");
        !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|name| name == "vowp-glob-probe")
    }

    /// Runs one shell command with the fake tools and returns its output.
    fn run_with_fakes(command: &str, bin: &Path, root: &Path, create_vault: bool) -> std::process::Output {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("PATH", path)
            .env("VOWP_TEST_ROOT", root)
            .env("VOWP_TEST_CREATE_VAULT", if create_vault { "1" } else { "0" })
            .stdin(Stdio::null())
            .output()
            .expect("run command")
    }

    #[test]
    fn upgrade_command_moves_basic_secrets_into_the_vault_directory() {
        let tmp = tempdir().expect("tempdir");
        let layout = layout_with(tmp.path());
        fs::create_dir_all(&layout.basic_dir).expect("basic dir");
        fs::write(layout.basic_dir.join("UPGRADE_KEY"), "basic-upgrade-value-1357").expect("basic value");
        fs::write(layout.basic_dir.join("mode"), "basic\n").expect("mode file");
        let glob_is_case_sensitive = migrate_glob_is_case_sensitive(&layout.basic_dir);
        let command = upgrade_command(&layout);
        let basic = shell_quote(&layout.basic_dir);
        let vault = shell_quote(&layout.vault_dir());
        assert!(
            command.contains(&format!("vowp-migrate {basic} {vault}")),
            "the move must target the vault directory itself: {command}"
        );
        assert!(
            !command.contains(&format!("{}/secrets", layout.vault_dir().display())),
            "the vault directory already ends in secrets: {command}"
        );
        assert!(
            !command.contains("|| true"),
            "a failed move must not be masked: {command}"
        );

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).expect("fake bin");
        fake_upgrade_tools(&bin);

        let output = run_with_fakes(&command, &bin, tmp.path(), true);
        assert!(
            output.status.success(),
            "the printed command failed: {}\n{command}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !layout.basic_dir.join("UPGRADE_KEY").exists(),
            "the basic copy must be moved, not copied"
        );
        if glob_is_case_sensitive {
            assert!(
                layout.basic_dir.join("mode").is_file(),
                "the lowercase mode file must stay behind"
            );
        }
        let moved = layout.vault_dir().join("UPGRADE_KEY");
        assert_eq!(
            fs::read_to_string(&moved).expect("vault value"),
            "basic-upgrade-value-1357"
        );
        assert_eq!(
            fs::metadata(&moved).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        let helper_config = layout.helper_home().join("vowp.toml");
        let helper_db = layout.helper_home().join("vowp.db");
        assert_eq!(
            fs::read_to_string(&helper_config).expect("helper config"),
            HELPER_CONFIG
        );
        assert!(helper_db.is_file(), "the helper node database placeholder exists");
        VowpConfig::from_toml(&fs::read_to_string(&helper_config).expect("config")).expect("valid helper config");
    }

    #[test]
    fn upgrade_command_fails_loudly_when_the_move_fails() {
        let tmp = tempdir().expect("tempdir");
        let layout = layout_with(tmp.path());
        fs::create_dir_all(&layout.basic_dir).expect("basic dir");
        fs::write(layout.basic_dir.join("UPGRADE_KEY"), "basic-upgrade-value-1357").expect("basic value");
        // Only the vault directory is withheld; the helper context is created normally.
        fs::create_dir_all(layout.helper_home()).expect("helper home");
        let command = upgrade_command(&layout);

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).expect("fake bin");
        fake_upgrade_tools(&bin);

        let output = run_with_fakes(&command, &bin, tmp.path(), false);
        assert!(
            !output.status.success(),
            "a failed move must make the printed command exit non-zero"
        );
        assert!(
            layout.basic_dir.join("UPGRADE_KEY").is_file(),
            "the failed move leaves the basic copy in place"
        );
    }

    #[test]
    fn upgrade_prints_the_command_when_sudo_needs_a_password() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        // A normal sudo exists but its non-interactive probes fail until the owner types a password.
        fake_tool(tmp.path(), "fake-sudo", "#!/bin/sh\nexit 1\n");

        let outcome = upgrade_impl(&deps, &layout).expect("upgrade");

        match outcome {
            Upgrade::NeedsCommand(command) => {
                assert!(
                    command.contains(&format!(
                        "vowp-migrate {} {}",
                        shell_quote(&layout.basic_dir),
                        shell_quote(&layout.vault_dir())
                    )),
                    "{command}"
                );
                assert!(command.contains("sudo useradd"), "{command}");
            }
            other => panic!("expected NeedsCommand, got {other:?}"),
        }
    }

    #[test]
    fn upgrade_reports_missing_sudo_without_printing_a_command() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());

        let outcome = upgrade_impl(&deps, &layout).expect("upgrade");

        match outcome {
            Upgrade::NoSudo(reason) => assert!(reason.contains("sudo is not installed"), "{reason}"),
            other => panic!("expected NoSudo, got {other:?}"),
        }
    }

    #[test]
    fn upgrade_command_skips_useradd_when_the_vault_user_exists() {
        let tmp = tempdir().expect("tempdir");
        let layout = layout_with(tmp.path());
        fs::create_dir_all(&layout.basic_dir).expect("basic dir");
        fs::write(layout.basic_dir.join("UPGRADE_KEY"), "basic-upgrade-value-1357").expect("basic value");
        let command = upgrade_command(&layout);

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).expect("fake bin");
        fake_upgrade_tools(&bin);
        fake_tool(&bin, "id", "#!/bin/sh\nexit 0\n");
        fake_tool(&bin, "useradd", "#!/bin/sh\nexit 1\n");

        let output = run_with_fakes(&command, &bin, tmp.path(), true);
        assert!(
            output.status.success(),
            "an existing vault user must not fail the retry: {}\n{command}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(layout.vault_dir().join("UPGRADE_KEY").is_file());
    }

    #[test]
    fn vault_mode_writes_value_on_stdin_and_reads_it_back() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let layout = layout_with(tmp.path());
        fake_sudo(&layout);
        write_mode(&deps, VaultMode::VaultUser).expect("mode");
        let name = SecretName::parse("VAULT_KEY").expect("name");
        let value = "sk-vault-secret-4f2a";

        let info = write_secret_impl(&deps, &layout, &name, value).expect("write");

        assert_eq!(info.last4, "4f2a");
        let stored = fs::read_to_string(layout.vault_dir().join("VAULT_KEY")).expect("stored");
        assert_eq!(stored, value);
        let argv = fs::read_to_string(layout.vault_home.join("argv.log")).expect("argv log");
        assert!(!argv.contains(value), "the value must never be on the helper argv");
        assert_eq!(
            read_value_impl(&deps, &layout, &name).expect("read"),
            Some(value.to_owned())
        );
        assert_eq!(
            read_via_helper(&layout, &name).expect("helper read"),
            Some(value.to_owned())
        );
        assert!(remove_secret_impl(&deps, &layout, &name).expect("remove"));
        assert!(!layout.vault_dir().join("VAULT_KEY").exists());
    }

    #[test]
    fn admin_helpers_write_and_remove_private_files() {
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("vault");
        let name = SecretName::parse("ADMIN_KEY").expect("name");
        admin_set_at(&dir, &name, "admin-value-9c1d", now()).expect("set");
        let path = dir.join("ADMIN_KEY");
        assert_eq!(fs::read_to_string(&path).expect("value"), "admin-value-9c1d");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).expect("metadata").permissions().mode() & 0o777,
            0o700
        );
        assert!(admin_remove_at(&dir, &name).expect("remove"));
        assert!(!admin_remove_at(&dir, &name).expect("second remove"));
    }

    #[test]
    fn admin_probe_checks_the_vault_directory_without_leaving_a_file() {
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("vault");

        admin_probe_at(&dir).expect("probe");

        assert!(dir.is_dir());
        assert_eq!(
            fs::read_dir(&dir).expect("vault dir").count(),
            0,
            "the probe leaves no file behind"
        );
    }

    #[test]
    fn index_metadata_never_contains_the_value() {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(tmp.path());
        let name = SecretName::parse("INDEX_KEY").expect("name");
        write_secret(&deps, &name, "index-value-7b3c").expect("write");
        let store = index_store(&deps).expect("store");
        let rows = store.list_secret_index().expect("rows");
        assert_eq!(rows.len(), 1);
        let text = serde_json::to_string(&rows).expect("metadata json");
        assert!(!text.contains("index-value-7b3c"));
        assert!(text.contains("7b3c"));
        assert_eq!(rows[0].name, name);
        assert_eq!(rows[0].last4, "7b3c");
        assert!(
            store.kv_prefix("secret:").expect("legacy rows").is_empty(),
            "new writes must use secret_index, not kv"
        );
    }
}
