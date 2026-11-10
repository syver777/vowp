//! Session side files: capsule JSON files and the append-only `adapters.json`.
//! Spec: `docs/design/crates/vowp-store.md` §3.7 and §4.2 rule 7, and
//! `docs/design/sessions.md` §1 and §3.
//!
//! Capsule files are the authoritative copy of every capsule; the `capsules` table is a
//! rebuildable cache. Both files are written through a temporary file in the target
//! directory, fsynced and renamed, so a reader never sees a partial document.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vowp_protocol::{Capsule, HarnessKind, NodeName, SessionId, Timestamp};

use crate::error::StoreError;
use crate::paths::NodePaths;

/// The mirror of `adapters.json`: harness-side session ids, append-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptersMap {
    /// Mappings in the order they were added.
    pub natives: Vec<AdapterMapping>,
}

/// One harness-side session id recorded for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterMapping {
    /// Harness that owns the native id.
    pub harness: HarnessKind,
    /// Harness-side session id.
    pub native_session_id: String,
    /// Node the harness ran on.
    pub node: NodeName,
    /// First session sequence number recorded for this native session.
    pub first_seq: u64,
    /// When the mapping was first recorded.
    pub created_at: Timestamp,
}

/// Writes a capsule file and returns its path.
///
/// The file is named `<at_seq:010>-<capsule ulid>.json`, written as pretty JSON to a
/// temporary file, fsynced and renamed into the session's `capsules/` directory.
pub fn write_capsule_file(paths: &NodePaths, c: &Capsule) -> Result<PathBuf, StoreError> {
    let dir = paths.capsules_dir(&c.session_id);
    std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
    let capsule_ulid = c
        .capsule_id
        .as_str()
        .strip_prefix("cap_")
        .unwrap_or(c.capsule_id.as_str());
    let path = dir.join(format!("{:010}-{capsule_ulid}.json", c.at_seq));
    let bytes = serde_json::to_vec_pretty(c)?;
    write_atomic(&path, &bytes)?;
    Ok(path)
}

/// Reads and validates one capsule file.
///
/// Unparsable JSON becomes [`StoreError::Corrupt`]; a structurally invalid capsule returns
/// its [`vowp_protocol::ProtocolError`].
pub fn read_capsule_file(path: &Path) -> Result<Capsule, StoreError> {
    let bytes = std::fs::read(path).map_err(|err| StoreError::io(path.display(), err))?;
    let capsule: Capsule = serde_json::from_slice(&bytes).map_err(|err| StoreError::Corrupt {
        path: path.display().to_string(),
        line: None,
        reason: format!("invalid capsule JSON: {err}"),
    })?;
    capsule.validate()?;
    Ok(capsule)
}

/// Lists a session's capsule files in name order, which is `at_seq` order.
///
/// A session without a `capsules/` directory has no capsules.
pub fn list_capsule_files(paths: &NodePaths, id: &SessionId) -> Result<Vec<PathBuf>, StoreError> {
    let dir = paths.capsules_dir(id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(StoreError::io(dir.display(), err)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| StoreError::io(dir.display(), err))?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|extension| extension == "json") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Reads `adapters.json`; a missing file is an empty map.
pub fn read_adapters_json(paths: &NodePaths, id: &SessionId) -> Result<AdaptersMap, StoreError> {
    let path = paths.adapters_json_path(id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(AdaptersMap::default()),
        Err(err) => return Err(StoreError::io(path.display(), err)),
    };
    serde_json::from_slice(&bytes).map_err(|err| StoreError::Corrupt {
        path: path.display().to_string(),
        line: None,
        reason: format!("invalid adapters.json: {err}"),
    })
}

/// Adds a mapping to `adapters.json`, preserving existing entries.
///
/// Adding the same `(harness, native_session_id)` pair again is a no-op. The new file is
/// written atomically with the same temporary-file recipe as capsule files.
pub fn add_adapter_mapping(paths: &NodePaths, id: &SessionId, m: AdapterMapping) -> Result<(), StoreError> {
    let mut map = read_adapters_json(paths, id)?;
    let duplicate = map
        .natives
        .iter()
        .any(|existing| existing.harness == m.harness && existing.native_session_id == m.native_session_id);
    if duplicate {
        return Ok(());
    }
    map.natives.push(m);
    let dir = paths.session_dir(id);
    std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
    let bytes = serde_json::to_vec_pretty(&map)?;
    write_atomic(&paths.adapters_json_path(id), &bytes)
}

/// Writes `bytes` to `path` through a fsynced temporary file and an atomic rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let tmp = parent.join(format!(".{name}.tmp-{}", ulid::Ulid::generate()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|err| StoreError::io(tmp.display(), err))?;
        file.write_all(bytes)
            .map_err(|err| StoreError::io(tmp.display(), err))?;
        file.sync_all().map_err(|err| StoreError::io(tmp.display(), err))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|err| StoreError::io(path.display(), err))?;
        let dir = File::open(parent).map_err(|err| StoreError::io(parent.display(), err))?;
        dir.sync_all().map_err(|err| StoreError::io(parent.display(), err))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
