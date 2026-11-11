//! Secret names, metadata and the one-time request document.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9 and §13.1. Values are never part of
//! these types; storage, injection and redaction live in `vowp-core`.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::time::Timestamp;

/// A validated secret name: `^[A-Z][A-Z0-9_]{0,63}$`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SecretName(pub String);

impl SecretName {
    /// Parses and validates a secret name.
    pub fn parse(name: &str) -> Result<Self, ProtocolError> {
        let mut chars = name.chars();
        let first = chars.next();
        let valid_first = first.is_some_and(|c| c.is_ascii_uppercase());
        let valid_rest = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if name.len() <= 64 && valid_first && valid_rest {
            Ok(Self(name.to_owned()))
        } else {
            Err(ProtocolError::InvalidId {
                kind: "secret name",
                value: name.to_owned(),
            })
        }
    }

    /// Borrows the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata for one stored secret; never the value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecretInfo {
    /// Secret name.
    pub name: SecretName,
    /// Last four characters of the value.
    pub last4: String,
    /// When the secret was first stored.
    pub created_at: Timestamp,
    /// When the secret was last replaced.
    pub updated_at: Timestamp,
}

/// How secrets are stored on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VaultMode {
    /// Files are owned by the separate `vowp-vault` user.
    VaultUser,
    /// Files are owned by the current user.
    Basic,
}

/// The one-time paste-page document returned by `vowp secret request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecretRequest {
    /// Secret the page will set.
    pub name: SecretName,
    /// Loopback URL to show the owner.
    pub url: String,
    /// When the page stops accepting a value.
    pub expires_at: Timestamp,
    /// Whether the page accepts exactly one value.
    pub single_use: bool,
}
