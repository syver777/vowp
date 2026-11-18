//! Node-key challenge and response.
//!
//! The home issues a random nonce and a connection id; the agent signs
//! `nonce + "\n" + home_id + "\n" + connection_id` with its ed25519 private key. The home
//! verifies the 64-byte signature against the node's active public key. Both the public key
//! and the signature cross the wire as lowercase hex; no private key ever leaves the node.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use vowp_protocol::new_lease_token;

/// Returns a fresh handshake nonce.
///
/// The nonce is a 52-character random token; it is never reused within a gateway process,
/// which makes an observed signature useless on the next connection.
pub fn issue_nonce() -> String {
    new_lease_token()
}

/// Returns the exact bytes an agent signs: `nonce + "\n" + home_id + "\n" + connection_id`.
pub fn handshake_payload(nonce: &str, home_id: &str, connection_id: &str) -> Vec<u8> {
    format!("{nonce}\n{home_id}\n{connection_id}").into_bytes()
}

/// Returns whether `value` is a 32-byte lowercase hex ed25519 public key.
pub fn is_public_key_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Returns whether `value` is a 64-byte lowercase hex ed25519 signature.
pub fn is_signature_hex(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Verifies one agent signature over the handshake payload.
///
/// Returns `Ok(false)` for a malformed key, signature or payload that does not verify. The
/// caller decides whether to send `Revoked` or a typed `Error`.
pub fn verify(public_key_hex: &str, signature_hex: &str, nonce: &str, home_id: &str, connection_id: &str) -> bool {
    let Some(public_key) = decode_hex::<32>(public_key_hex) else {
        return false;
    };
    let Some(signature) = decode_hex::<64>(signature_hex) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&public_key) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    verifying_key
        .verify(&handshake_payload(nonce, home_id, connection_id), &signature)
        .is_ok()
}

/// Decodes exactly `N` bytes of lowercase hex.
fn decode_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let mut bytes = [0_u8; N];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(bytes)
}

/// Maps one lowercase hex digit to its value.
fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn public_hex(key: &SigningKey) -> String {
        key.verifying_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn signature_hex(key: &SigningKey, nonce: &str, home: &str, connection: &str) -> String {
        key.sign(&handshake_payload(nonce, home, connection))
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn a_valid_signature_verifies() {
        let key = signing_key();
        let signature = signature_hex(&key, "nonce", "home-1", "connection-1");
        assert!(verify(&public_hex(&key), &signature, "nonce", "home-1", "connection-1"));
    }

    #[test]
    fn a_signature_for_another_connection_does_not_verify() {
        let key = signing_key();
        let signature = signature_hex(&key, "nonce", "home-1", "connection-1");
        assert!(!verify(
            &public_hex(&key),
            &signature,
            "nonce",
            "home-1",
            "connection-2"
        ));
        assert!(!verify(
            &public_hex(&key),
            &signature,
            "other-nonce",
            "home-1",
            "connection-1"
        ));
    }

    #[test]
    fn malformed_keys_signatures_and_uppercase_hex_are_refused() {
        let key = signing_key();
        let signature = signature_hex(&key, "nonce", "home-1", "connection-1");
        assert!(!verify("", &signature, "nonce", "home-1", "connection-1"));
        assert!(!verify(
            &public_hex(&key).to_uppercase(),
            &signature,
            "nonce",
            "home-1",
            "connection-1"
        ));
        assert!(!verify(
            &public_hex(&key),
            &signature.to_uppercase(),
            "nonce",
            "home-1",
            "connection-1"
        ));
        assert!(!verify(&public_hex(&key), "00", "nonce", "home-1", "connection-1"));
    }
}
