//! Strongly typed ids: ULIDs for sessions, events, runs and capsules; slugs for jobs, nodes
//! and projects. Spec: `docs/design/types.md` §1 and `docs/design/crates/vowp-protocol.md` §4.
//!
//! Parsing never panics and always returns [`ProtocolError::InvalidId`] for bad input. ULID
//! parsing accepts lowercase and normalises to the canonical uppercase form; ids created with
//! `new()` are monotonic within one process.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Process-wide ULID generator so ids created in the same millisecond keep creation order.
static ULID_GENERATOR: OnceLock<Mutex<ulid::Generator>> = OnceLock::new();

/// Returns the next ULID as its canonical uppercase string, waiting 1 ms on same-ms overflow.
fn next_ulid() -> String {
    let generator = ULID_GENERATOR.get_or_init(|| Mutex::new(ulid::Generator::new()));
    let mut generator = generator.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match generator.generate() {
            Ok(ulid) => return ulid.to_string(),
            // The 80 random bits for this millisecond are exhausted; the generator's clock
            // has not advanced yet, so wait for the next millisecond and retry.
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
}

/// Returns true for a character in the Crockford base32 ULID alphabet, either case.
fn is_ulid_char(c: char) -> bool {
    c.is_ascii_digit()
        || matches!(
            c,
            'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z'
                | 'a'..='h' | 'j' | 'k' | 'm' | 'n' | 'p'..='t' | 'v'..='z'
        )
}

/// Validates a 26-character ULID and returns its canonical uppercase form.
fn validate_ulid(s: &str, kind: &'static str, original: &str) -> Result<String, ProtocolError> {
    if s.len() != 26 || !s.chars().all(is_ulid_char) {
        return Err(ProtocolError::InvalidId {
            kind,
            value: original.to_owned(),
        });
    }
    let upper = s.to_ascii_uppercase();
    // A ULID is 128 bits in 26 Crockford base32 characters, so the first character carries
    // only two significant bits and must be 0-7. `ulid::Ulid::from_string` would otherwise
    // silently truncate a larger value and return a different id.
    if upper.as_bytes().first().is_some_and(|first| *first > b'7') {
        return Err(ProtocolError::InvalidId {
            kind,
            value: original.to_owned(),
        });
    }
    let ulid = ulid::Ulid::from_string(&upper).map_err(|_| ProtocolError::InvalidId {
        kind,
        value: original.to_owned(),
    })?;
    Ok(ulid.to_string())
}

/// Returns true for a valid slug: `^[a-z0-9][a-z0-9-]{0,63}$` with no trailing `-`.
fn is_slug(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return false;
    }
    !matches!(bytes.last(), Some(&b'-'))
}

/// Implements the traits shared by every id newtype: Display, FromStr, AsRef<str> and serde.
macro_rules! id_traits {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = ProtocolError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProtocolError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

/// Declares a ULID-backed id type with an optional prefix (for example `cap_`).
macro_rules! ulid_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal, $kind:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(String);

        #[allow(clippy::new_without_default)] // a default ULID would be a freshly created id
        impl $name {
            /// Creates an id with a fresh ULID, monotonic within this process.
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, next_ulid()))
            }

            /// Parses an id; lowercase ULIDs are accepted and normalised to uppercase.
            pub fn parse(s: &str) -> Result<Self, ProtocolError> {
                let ulid = s.strip_prefix($prefix).ok_or_else(|| ProtocolError::InvalidId {
                    kind: $kind,
                    value: s.to_owned(),
                })?;
                let ulid = validate_ulid(ulid, $kind, s)?;
                Ok(Self(format!("{}{}", $prefix, ulid)))
            }

            /// Borrows the id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        id_traits!($name);
    };
}

/// Declares a slug-backed id type.
macro_rules! slug_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(String);

        impl $name {
            /// Parses a slug: lowercase letters and digits, `-` inside, at most 64 characters.
            pub fn parse(s: &str) -> Result<Self, ProtocolError> {
                if is_slug(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(ProtocolError::InvalidId {
                        kind: "slug",
                        value: s.to_owned(),
                    })
                }
            }

            /// Borrows the id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        id_traits!($name);
    };
}

ulid_id!(
    /// Session id: 26-character ULID.
    SessionId,
    "",
    "ulid"
);
ulid_id!(
    /// Event id: 26-character ULID.
    EventId,
    "",
    "ulid"
);
ulid_id!(
    /// Run id: 26-character ULID.
    RunId,
    "",
    "ulid"
);
ulid_id!(
    /// Switch id: 26-character ULID.
    SwitchId,
    "",
    "ulid"
);
ulid_id!(
    /// Inbox item id: 26-character ULID.
    InboxId,
    "",
    "ulid"
);
ulid_id!(
    /// Release id: 26-character ULID.
    ReleaseId,
    "",
    "ulid"
);
ulid_id!(
    /// Schedule id: 26-character ULID.
    ScheduleId,
    "",
    "ulid"
);
ulid_id!(
    /// Capsule id: `cap_` followed by a 26-character ULID.
    CapsuleId,
    "cap_",
    "capsule"
);

impl CapsuleId {
    /// Returns the 26-character ULID part, without the `cap_` prefix.
    pub fn ulid(&self) -> &str {
        self.0.strip_prefix("cap_").unwrap_or(&self.0)
    }
}

slug_id!(
    /// Job id slug, for example `example-job`.
    JobId
);
slug_id!(
    /// Node name slug, for example `vps`.
    NodeName
);
slug_id!(
    /// Project name slug, for example `vowp`.
    ProjectName
);
