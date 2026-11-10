//! Content-addressed blob storage for spilled tool output, diffs and attachments.
//! Spec: `docs/design/crates/vowp-store.md` §3.5 and §4.2 rule 7.
//!
//! A [`BlobStore`] is a directory of files named by the lowercase hex SHA-256 of their
//! content. Writes go to a temporary file in the same directory and are renamed into place,
//! so a reader never sees a partial blob; identical content is written once.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use vowp_protocol::{BLOB_PREVIEW_CHARS, BlobRef};

use crate::error::StoreError;

/// Length of a lowercase hex SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// A content-addressed directory of blobs.
///
/// One store exists per session (`sessions/<id>/blobs`) and callers may create others for
/// gate output or other large artefacts. The directory is created on the first [`put`].
///
/// [`put`]: BlobStore::put
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// Builds a store rooted at `dir` without touching the filesystem.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Stores `bytes` and returns the reference to store in an event.
    ///
    /// The content is written to a temporary file, fsynced, renamed to its digest and the
    /// directory entry is fsynced. If a file with the same digest already exists the write is
    /// skipped ([`BlobStore::put`] is idempotent).
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef, StoreError> {
        let sha256 = hex::encode(Sha256::digest(bytes));
        let target = self.dir.join(&sha256);
        let blob_ref = BlobRef {
            sha256: sha256.clone(),
            bytes: bytes.len() as u64,
            preview: preview(bytes),
        };

        std::fs::create_dir_all(&self.dir).map_err(|err| StoreError::io(self.dir.display(), err))?;
        if target.is_file() {
            return Ok(blob_ref);
        }

        let tmp = self.dir.join(format!(".tmp-{}", ulid::Ulid::generate()));
        let result: Result<(), StoreError> = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|err| StoreError::io(tmp.display(), err))?;
            file.write_all(bytes)
                .map_err(|err| StoreError::io(tmp.display(), err))?;
            file.sync_all().map_err(|err| StoreError::io(tmp.display(), err))?;
            drop(file);
            std::fs::rename(&tmp, &target).map_err(|err| StoreError::io(target.display(), err))?;
            sync_dir(&self.dir)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        Ok(blob_ref)
    }

    /// Reads a blob and verifies that its content still hashes to `sha256`.
    ///
    /// Returns [`StoreError::NotFound`] when the file is absent and [`StoreError::Corrupt`]
    /// when the digest does not match the requested name.
    pub fn get(&self, sha256: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.path_of(sha256)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::not_found("blob", sha256));
            }
            Err(err) => return Err(StoreError::io(path.display(), err)),
        };
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != sha256.to_ascii_lowercase() {
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("blob digest mismatch: expected {sha256}, content hashes to {actual}"),
            });
        }
        Ok(bytes)
    }

    /// Returns whether a blob with this digest is present; invalid names are simply absent.
    pub fn exists(&self, sha256: &str) -> bool {
        self.path_of(sha256).is_ok_and(|path| path.is_file())
    }

    /// Returns the path of a blob after validating the digest name.
    ///
    /// Only a 64-character hexadecimal SHA-256 is accepted, which keeps the joined path
    /// inside the store directory.
    pub fn path_of(&self, sha256: &str) -> Result<PathBuf, StoreError> {
        let valid = sha256.len() == SHA256_HEX_LEN && sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !valid {
            return Err(StoreError::InvalidInput(format!(
                "blob name must be 64 hexadecimal characters, got {sha256:?}"
            )));
        }
        Ok(self.dir.join(sha256.to_ascii_lowercase()))
    }
}

/// Returns the first [`BLOB_PREVIEW_CHARS`] characters of the content, lossily decoded.
fn preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(BLOB_PREVIEW_CHARS)
        .collect()
}

/// Fsyncs a directory so a rename inside it survives a crash.
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    let file = File::open(dir).map_err(|err| StoreError::io(dir.display(), err))?;
    file.sync_all().map_err(|err| StoreError::io(dir.display(), err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_newlines_and_counts_characters() {
        let bytes = format!("line one\nline two\n{}", "é".repeat(BLOB_PREVIEW_CHARS)).into_bytes();
        let text = preview(&bytes);
        assert_eq!(text.chars().count(), BLOB_PREVIEW_CHARS);
        assert!(text.starts_with("line one\nline two\n"));
    }

    #[test]
    fn path_rejects_traversal() {
        let store = BlobStore::new("/tmp/blobs");
        assert!(matches!(store.path_of("../x"), Err(StoreError::InvalidInput(_))));
        assert!(matches!(store.path_of("abc"), Err(StoreError::InvalidInput(_))));
        assert!(store.path_of(&"a".repeat(64)).is_ok());
    }
}
