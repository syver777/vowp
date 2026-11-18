//! The long-lived ed25519 node key.
//!
//! `node.key` holds the 32-byte secret as lowercase hex and is mode 0600. `node.pub` holds the
//! 32-byte public key as lowercase hex. The private key is never logged; [`NodeKey`]'s `Debug`
//! output names only the public key.

use std::fmt;
use std::fs;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::config::{write_private, write_public};
use crate::error::AgentError;

/// Private key file name.
pub const NODE_KEY_FILE: &str = "node.key";
/// Public key file name.
pub const NODE_PUB_FILE: &str = "node.pub";

/// A loaded ed25519 node key.
#[derive(Clone)]
pub struct NodeKey {
    signing: SigningKey,
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeKey")
            .field("public_key", &self.public_key_hex())
            .finish_non_exhaustive()
    }
}

impl NodeKey {
    /// Generates a fresh key pair and writes `node.key` (0600) and `node.pub`.
    pub fn generate(state_dir: &Path) -> Result<Self, AgentError> {
        let signing = SigningKey::generate(&mut OsRng);
        let key = Self { signing };
        write_private(
            &state_dir.join(NODE_KEY_FILE),
            format!("{}\n", hex::encode(key.signing.to_bytes())).as_bytes(),
        )?;
        write_public(
            &state_dir.join(NODE_PUB_FILE),
            format!("{}\n", key.public_key_hex()).as_bytes(),
        )?;
        Ok(key)
    }

    /// Loads `node.key` from `state_dir`.
    pub fn load(state_dir: &Path) -> Result<Self, AgentError> {
        use std::os::unix::fs::PermissionsExt;

        let path = state_dir.join(NODE_KEY_FILE);
        let mode = fs::metadata(&path)
            .map_err(|error| AgentError::Key(format!("{}: {error}", path.display())))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(AgentError::Key(format!(
                "{} is mode {mode:03o}; the node key must be 0600",
                path.display()
            )));
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(AgentError::Key(format!("{} is missing", path.display())));
            }
            Err(error) => return Err(AgentError::Io(error)),
        };
        let bytes = hex::decode(text.trim())
            .map_err(|error| AgentError::Key(format!("{} is not hex: {error}", path.display())))?;
        let secret: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| AgentError::Key(format!("{} must hold 32 bytes", path.display())))?;
        Ok(Self {
            signing: SigningKey::from_bytes(&secret),
        })
    }

    /// Loads `node.key`, generating one when it does not exist yet.
    pub fn load_or_generate(state_dir: &Path) -> Result<Self, AgentError> {
        if state_dir.join(NODE_KEY_FILE).is_file() {
            Self::load(state_dir)
        } else {
            Self::generate(state_dir)
        }
    }

    /// Returns the public key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// Returns the 32-byte public key as lowercase hex.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.verifying_key().as_bytes())
    }

    /// Returns the short, stable fingerprint shown by `vowp agent status`.
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.verifying_key())
    }

    /// Signs one handshake challenge.
    pub fn sign_handshake(&self, nonce: &str, home_id: &str, connection_id: &str) -> String {
        hex::encode(
            self.signing
                .sign(&handshake_payload(nonce, home_id, connection_id))
                .to_bytes(),
        )
    }
}

/// Reads `node.pub` and returns the public key.
pub fn load_public(state_dir: &Path) -> Result<VerifyingKey, AgentError> {
    let path = state_dir.join(NODE_PUB_FILE);
    let text = fs::read_to_string(&path).map_err(|_| AgentError::Key(format!("{} is missing", path.display())))?;
    let bytes =
        hex::decode(text.trim()).map_err(|error| AgentError::Key(format!("{} is not hex: {error}", path.display())))?;
    let public: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| AgentError::Key(format!("{} must hold 32 bytes", path.display())))?;
    VerifyingKey::from_bytes(&public)
        .map_err(|error| AgentError::Key(format!("{} is not an ed25519 key: {error}", path.display())))
}

/// Returns the exact bytes signed by `AgentAuth`: `nonce + "\n" + home_id + "\n" + connection_id`.
pub fn handshake_payload(nonce: &str, home_id: &str, connection_id: &str) -> Vec<u8> {
    format!("{nonce}\n{home_id}\n{connection_id}").into_bytes()
}

/// Returns the short fingerprint of a public key: `sha256:` plus the first eight digest bytes.
pub fn fingerprint_of(key: &VerifyingKey) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("sha256:{}", hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn agent_key_is_created_0600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = NodeKey::load_or_generate(dir.path()).expect("generate key");

        let key_path = dir.path().join(NODE_KEY_FILE);
        let pub_path = dir.path().join(NODE_PUB_FILE);
        let key_mode = fs::metadata(&key_path).expect("key metadata").permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "node.key must be private");

        let public = fs::read_to_string(&pub_path).expect("public key");
        assert_eq!(public.trim(), key.public_key_hex());
        assert_eq!(public.trim().len(), 64, "public key is 32 bytes in lowercase hex");

        let loaded = NodeKey::load(dir.path()).expect("load key");
        assert_eq!(loaded.public_key_hex(), key.public_key_hex());
        assert_eq!(loaded.fingerprint(), key.fingerprint());
        assert!(key.fingerprint().starts_with("sha256:"));
    }
}
