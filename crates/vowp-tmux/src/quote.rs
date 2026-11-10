//! Quoting and name validation for tmux command lines.
//!
//! tmux takes the command for a new window as one shell-command string, so
//! every argv element must be single-quoted. Names are passed as separate
//! process arguments and therefore only need validation.
//!
//! Spec: `docs/design/crates/vowp-tmux.md` §3 rule 5.

use crate::error::TmuxError;

/// POSIX single-quote escaping: wraps in `'...'` and rewrites each `'` as
/// `'\''`. The empty string becomes `''`.
pub fn shell_quote(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Validate a session, window or option-value name.
///
/// Allowed: `[A-Za-z0-9_.-]+`, 1..=64 characters, and the first character
/// must not be `-`. Anything else is [`TmuxError::InvalidName`].
pub fn validate_name(kind: &'static str, s: &str) -> Result<(), TmuxError> {
    let invalid = || TmuxError::InvalidName {
        kind,
        value: s.to_owned(),
    };
    let count = s.chars().count();
    if count == 0 || count > 64 || s.starts_with('-') {
        return Err(invalid());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Build the single shell-command string tmux runs for a new window.
///
/// `env` entries become leading `env 'K=V'` words; the key must match
/// `[A-Za-z_][A-Za-z0-9_]*`. `argv` must be non-empty.
pub(crate) fn window_command(argv: &[String], env: &[(String, String)]) -> Result<String, TmuxError> {
    if argv.is_empty() {
        return Err(TmuxError::EmptyCommand);
    }
    let mut words = Vec::with_capacity(argv.len() + env.len());
    for (key, value) in env {
        if !valid_env_key(key) {
            return Err(TmuxError::InvalidName {
                kind: "environment variable",
                value: key.clone(),
            });
        }
        words.push(format!("env {}", shell_quote(&format!("{key}={value}"))));
    }
    words.extend(argv.iter().map(|arg| shell_quote(arg)));
    Ok(words.join(" "))
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_string_builds_quoted_argv_with_env() {
        let argv = vec!["echo".to_owned(), "a b".to_owned(), "c'd".to_owned()];
        let env = vec![("K".to_owned(), "V".to_owned())];
        assert_eq!(
            window_command(&argv, &env).unwrap(),
            r#"env 'K=V' 'echo' 'a b' 'c'\''d'"#
        );
    }

    #[test]
    fn command_string_rejects_empty_argv_and_bad_env_key() {
        assert!(matches!(window_command(&[], &[]), Err(TmuxError::EmptyCommand)));
        let argv = vec!["sh".to_owned()];
        let env = vec![("BAD KEY".to_owned(), "V".to_owned())];
        assert!(matches!(
            window_command(&argv, &env),
            Err(TmuxError::InvalidName { .. })
        ));
    }
}
