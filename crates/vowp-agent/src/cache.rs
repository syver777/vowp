//! The bounded read cache served while the home is unreachable.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.4, §9.2 and §9.5, and
//! `docs/design/crates/vowp-agent.md`. One entry is one file below `<state_dir>/cache/`.
//! The key is the surface id, the tool name and a SHA-256 over the canonicalised
//! `arguments` JSON, so no argument value, token, secret or absolute path is ever written
//! to a file name or to the cache itself. Only the six MCP read tools are accepted; a write
//! or unknown tool is a typed refusal, never a silent miss.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vowp_protocol::{SurfaceId, Timestamp};

use crate::client::validate_surface_id;
use crate::config::ensure_private_dir;
use crate::error::AgentError;

/// Directory name below the agent state directory.
pub const CACHE_DIR: &str = "cache";

/// Largest number of entries the cache keeps.
pub const CACHE_MAX_ENTRIES: usize = 256;

/// Largest total size of the cache files on disk, in bytes.
pub const CACHE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How long a stored read may be used for an offline answer.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The MCP tools whose successful reads may be cached.
///
/// Every other tool writes, opens a tunnel or requests a secret and is never cached.
pub const READ_TOOLS: [&str; 6] = [
    "fleet_status",
    "fleet_queue",
    "inbox_list",
    "capacity_report",
    "session_tail",
    "session_resume",
];

/// Returns whether `tool` is one of the six cacheable MCP read tools.
pub fn is_cacheable_tool(tool: &str) -> bool {
    READ_TOOLS.contains(&tool)
}

/// One cached read: the home's exact successful result and when it was stored.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedRead {
    /// The exact MCP result payload the home returned for the successful call.
    pub result: Value,
    /// When the entry was written.
    pub stored_at: Timestamp,
}

/// The size and age summary shown by `vowp agent status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of entry files on disk.
    pub entries: u64,
    /// Total size of the entry files on disk, in bytes.
    pub bytes: u64,
    /// When the oldest readable entry was stored.
    pub oldest_stored_at: Option<Timestamp>,
}

/// The agent-side read cache below `<state_dir>/cache/`.
///
/// The cache is blocking and holds no in-memory state beyond its root, so it is safe to
/// share between the frame loop and the local socket threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Creates a cache handle for `state_dir`; no directory is created until the first
    /// [`Cache::store`].
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join(CACHE_DIR),
        }
    }

    /// Returns the cache root (`<state_dir>/cache`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stores one successful read.
    ///
    /// A write or unknown tool is [`AgentError::Config`] and nothing is written. The file
    /// is written through a temporary file and renamed, so a crash never leaves a half
    /// entry. A store that would exceed [`CACHE_MAX_ENTRIES`] or [`CACHE_MAX_BYTES`]
    /// prunes expired entries and then evicts the oldest ones until both bounds hold.
    pub fn store(
        &self,
        tool: &str,
        surface: &SurfaceId,
        arguments: &Value,
        result: &Value,
        now: Timestamp,
    ) -> Result<(), AgentError> {
        check_key(tool, surface)?;
        let hash = arguments_hash(arguments)?;
        let entry = Entry {
            surface: surface.0.clone(),
            tool: tool.to_owned(),
            arguments_sha256: hash.clone(),
            stored_at: now,
            result: result.clone(),
        };
        let bytes = serde_json::to_vec(&entry)?;
        if bytes.len() as u64 > CACHE_MAX_BYTES {
            return Err(AgentError::Config(format!(
                "a cache entry for {tool} exceeds {CACHE_MAX_BYTES} bytes"
            )));
        }
        let path = self.entry_path(surface, tool, &hash);
        write_entry(&path, &bytes)?;
        let (entries, size) = self.measure();
        if entries > CACHE_MAX_ENTRIES || size > CACHE_MAX_BYTES {
            self.prune(now);
        }
        Ok(())
    }

    /// Returns a fresh cached read for exactly this surface, tool and argument set.
    ///
    /// A missing, corrupt, unreadable or expired entry is `Ok(None)`: a cache problem is
    /// never fatal to the tool call. An expired entry is removed when it is noticed.
    pub fn lookup(
        &self,
        tool: &str,
        surface: &SurfaceId,
        arguments: &Value,
        now: Timestamp,
    ) -> Result<Option<CachedRead>, AgentError> {
        check_key(tool, surface)?;
        let hash = arguments_hash(arguments)?;
        let path = self.entry_path(surface, tool, &hash);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        let entry: Entry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(_) => {
                let _ = fs::remove_file(&path);
                return Ok(None);
            }
        };
        if entry.surface != surface.0 || entry.tool != tool || entry.arguments_sha256 != hash {
            return Ok(None);
        }
        if now.since(entry.stored_at) > CACHE_TTL {
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
        Ok(Some(CachedRead {
            result: entry.result,
            stored_at: entry.stored_at,
        }))
    }

    /// Removes expired and corrupt entries, then evicts the oldest until both bounds hold.
    ///
    /// Returns the stats left after the prune. Filesystem failures are skipped: a cache
    /// problem is never fatal.
    pub fn prune(&self, now: Timestamp) -> CacheStats {
        let mut remaining: VecDeque<EntryFile> = VecDeque::new();
        for file in self.scan() {
            let expired = match file.stored_at {
                Some(stored_at) => now.since(stored_at) > CACHE_TTL,
                // An unreadable or corrupt entry can never be served; it is skipped.
                None => true,
            };
            if expired {
                let _ = fs::remove_file(&file.path);
            } else {
                remaining.push_back(file);
            }
        }
        self.remove_stale_temps();
        let mut bytes: u64 = remaining.iter().map(|file| file.bytes).sum();
        while remaining.len() > CACHE_MAX_ENTRIES || bytes > CACHE_MAX_BYTES {
            let Some(file) = remaining.pop_front() else {
                break;
            };
            let _ = fs::remove_file(&file.path);
            bytes = bytes.saturating_sub(file.bytes);
        }
        stats_of(remaining)
    }

    /// Walks the cache directory and returns its current size and age.
    ///
    /// Unreadable and corrupt files are skipped for the age and never make the walk fail.
    pub fn stats(&self) -> CacheStats {
        let files: VecDeque<EntryFile> = self.scan().into();
        stats_of(files)
    }

    /// Removes every entry of one surface and returns how many files were removed.
    ///
    /// The client calls this when the home sends `SurfaceRevoked` and after a fresh
    /// pairing, so an old surface token can never read entries written before it.
    pub fn clear_surface(&self, surface: &SurfaceId) -> Result<usize, AgentError> {
        validate_surface_id(surface)?;
        let entry_prefix = format!("{}.", surface.0);
        // A crashed write leaves `.<surface>.<tool>.<hash>.json.<pid>.<n>.tmp` behind.
        let temp_prefix = format!(".{}.", surface.0);
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(AgentError::Io(error)),
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let entry_file = name.starts_with(&entry_prefix) && name.ends_with(".json");
            let temp_file = name.starts_with(&temp_prefix) && name.ends_with(".tmp");
            if !entry_file && !temp_file {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(AgentError::Io(error)),
            }
        }
        Ok(removed)
    }

    /// Removes hidden temporary files left by a crash or a failed write.
    fn remove_stale_temps(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn entry_path(&self, surface: &SurfaceId, tool: &str, hash: &str) -> PathBuf {
        self.root.join(format!("{}.{}.{hash}.json", surface.0, tool))
    }

    /// Counts entry files and their bytes without parsing them.
    fn measure(&self) -> (usize, u64) {
        let mut entries = 0usize;
        let mut bytes = 0u64;
        let Ok(dir) = fs::read_dir(&self.root) else {
            return (entries, bytes);
        };
        for entry in dir.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            entries = entries.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
        }
        (entries, bytes)
    }

    /// Returns every entry file, oldest first; a corrupt file sorts first because it has
    /// no readable `stored_at`.
    fn scan(&self) -> Vec<EntryFile> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return files;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            // A temporary file left by a crash is hidden and is never an entry.
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let stored_at = fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).ok())
                .map(|entry| entry.stored_at);
            files.push(EntryFile {
                path: entry.path(),
                bytes: metadata.len(),
                stored_at,
            });
        }
        files.sort_by(|left, right| {
            left.stored_at
                .cmp(&right.stored_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        files
    }
}

/// Writes a cache diagnostic to stderr.
///
/// The agent has no logger and a cache failure must never fail the tool call, so this is
/// the only visible trace. The cache's own errors name a tool or a filesystem operation,
/// never an argument value, result, token or path.
pub(crate) fn debug_failure(context: &str, error: &AgentError) {
    eprintln!("vowp-agent: {context}: {error}");
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    surface: String,
    tool: String,
    arguments_sha256: String,
    stored_at: Timestamp,
    result: Value,
}

struct EntryFile {
    path: PathBuf,
    bytes: u64,
    stored_at: Option<Timestamp>,
}

fn stats_of(mut files: VecDeque<EntryFile>) -> CacheStats {
    let mut stats = CacheStats::default();
    while let Some(file) = files.pop_front() {
        stats.entries = stats.entries.saturating_add(1);
        stats.bytes = stats.bytes.saturating_add(file.bytes);
        if let Some(stored_at) = file.stored_at {
            stats.oldest_stored_at = Some(match stats.oldest_stored_at {
                Some(current) => current.min(stored_at),
                None => stored_at,
            });
        }
    }
    stats
}

fn check_key(tool: &str, surface: &SurfaceId) -> Result<(), AgentError> {
    if !is_cacheable_tool(tool) {
        return Err(AgentError::Config(format!("tool {tool} is not a cacheable read")));
    }
    validate_surface_id(surface)
}

/// Returns the lowercase SHA-256 of the canonical `arguments` JSON.
///
/// Object keys are sorted recursively so two equivalent argument objects hash the same,
/// and no argument value is retained.
fn arguments_hash(arguments: &Value) -> Result<String, AgentError> {
    let mut bytes = Vec::new();
    write_canonical(arguments, &mut bytes)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), AgentError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&serde_json::to_vec(key)?);
                out.push(b':');
                if let Some(inner) = map.get(key.as_str()) {
                    write_canonical(inner, out)?;
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        other => out.extend_from_slice(&serde_json::to_vec(other)?),
    }
    Ok(())
}

/// Creates the 0700 cache directory and writes `bytes` to `path` through a 0600 temporary
/// file and a rename.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_entry(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    let parent = path
        .parent()
        .ok_or_else(|| AgentError::Config("the cache entry has no parent directory".to_owned()))?;
    ensure_private_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AgentError::Config("the cache entry has no file name".to_owned()))?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let temp = parent.join(format!(".{name}.{}.{stamp}.{counter}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.map_err(AgentError::Io)
}
