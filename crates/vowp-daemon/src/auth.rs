//! The bearer token that gates the daemon's write endpoints.
//!
//! The value comes from `VOWP_API_TOKEN` at startup. Missing or empty disables both writes;
//! the token is never logged, echoed, or accepted from a URL. The type has a redacted
//! [`Debug`] and compares candidate values without an early exit.

use std::fmt;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

/// The daemon's write token, read from `VOWP_API_TOKEN`.
///
/// Reads stay token-free. Only `POST /api/v1/inbox/{id}/answer` and
/// `POST /api/v1/sessions/{id}/switch` check this value, and a daemon started without it
/// answers `503`.
#[derive(Clone)]
pub struct ApiToken(String);

impl ApiToken {
    /// Reads `VOWP_API_TOKEN`, treating a missing or empty value as writes-disabled.
    pub fn from_env() -> Option<Self> {
        std::env::var("VOWP_API_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Self)
    }

    /// Wraps one token value.
    ///
    /// The binary uses [`ApiToken::from_env`]; tests use this constructor so no process
    /// environment is touched.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns true when `candidate` equals the stored token in constant time.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken([REDACTED])")
    }
}

/// Returns the token from an `Authorization: Bearer <token>` header.
///
/// The scheme is compared case-insensitively. A missing header, another scheme or an empty
/// token is `None`.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Compares two byte strings without stopping at the first difference.
///
/// The lengths are mixed into the result as well, so equal content with different lengths
/// is not accepted. This is a timing backstop, not a claim that memory access is constant.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_debug_is_redacted_and_compare_is_exact() {
        let token = ApiToken::new("correct-horse");
        assert_eq!(format!("{token:?}"), "ApiToken([REDACTED])");
        assert!(!format!("{token:?}").contains("correct-horse"));
        assert!(token.matches("correct-horse"));
        assert!(!token.matches("correct-hors"));
        assert!(!token.matches("correct-horse "));
        assert!(!token.matches(""));
    }

    #[test]
    fn bearer_token_accepts_only_a_non_empty_bearer_value() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(bearer_token(&headers), Some("abc"));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer abc"));
        assert_eq!(bearer_token(&headers), Some("abc"));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_token(&headers), None);

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_token(&headers), None);

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn constant_time_eq_handles_lengths_and_content() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"samf"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
