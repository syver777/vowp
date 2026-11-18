//! One-time enrollment codes.
//!
//! A code is a 52-character [`vowp_protocol::new_lease_token`] value (two ULIDs, 160 random
//! bits). Only its SHA-256 hex digest is stored; the plaintext is returned once by
//! `vowp node enroll` and never logged or written to a file. Codes expire after ten minutes.

use std::time::Duration;

use sha2::{Digest, Sha256};
use vowp_protocol::Timestamp;

/// Lifetime of an enrollment code.
pub const ENROLLMENT_CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// A freshly generated code and the hash stored by the home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEnrollmentCode {
    /// Plaintext code returned to the owner exactly once.
    pub code: String,
    /// Lowercase hex SHA-256 of [`Self::code`].
    pub code_hash: String,
    /// Instant at which the code stops redeeming.
    pub expires_at: Timestamp,
}

/// Returns a fresh 52-character enrollment code and its hash, valid for ten minutes.
pub fn new_enrollment_code(now: Timestamp) -> NewEnrollmentCode {
    let code = vowp_protocol::new_lease_token();
    let code_hash = hash_code(&code);
    NewEnrollmentCode {
        code,
        code_hash,
        expires_at: now.plus(ENROLLMENT_CODE_TTL),
    }
}

/// Returns the lowercase hex SHA-256 digest of `code`.
pub fn hash_code(code: &str) -> String {
    hash_secret(code)
}

/// Returns the lowercase hex SHA-256 digest of any secret string.
///
/// Pairing codes and surface tokens use this helper so the home stores one digest shape
/// for every one-time or long-lived secret.
pub(crate) fn hash_secret(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Returns whether `code` has the documented 52-character alphanumeric shape.
pub fn has_code_shape(code: &str) -> bool {
    code.len() == 52 && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
    }

    #[test]
    fn a_code_is_52_characters_and_hashes_to_64_hex_digits() {
        let generated = new_enrollment_code(now());
        assert_eq!(generated.code.len(), 52);
        assert!(has_code_shape(&generated.code));
        assert_eq!(generated.code_hash.len(), 64);
        assert!(generated.code_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(generated.code_hash.bytes().all(|byte| !byte.is_ascii_uppercase()));
        assert_eq!(generated.expires_at, now().plus(ENROLLMENT_CODE_TTL));
    }

    #[test]
    fn hashing_is_stable_and_distinguishes_codes() {
        assert_eq!(hash_code("abc"), hash_code("abc"));
        assert_ne!(hash_code("abc"), hash_code("abd"));
        assert_eq!(hash_code("abc").len(), 64);
    }

    #[test]
    fn malformed_codes_are_rejected() {
        assert!(!has_code_shape(""));
        assert!(!has_code_shape(&"a".repeat(51)));
        assert!(!has_code_shape(&"a".repeat(53)));
        assert!(!has_code_shape(&format!("{}!", "a".repeat(51))));
    }
}
