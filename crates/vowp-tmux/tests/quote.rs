//! Quoting and naming tests; no tmux server is used.

use std::process::Command;

use vowp_tmux::TmuxError;
use vowp_tmux::quote::{shell_quote, validate_name};
use vowp_tmux::{session_name_for_project, window_name_for_job};

#[test]
fn shell_quote_plain_and_empty() {
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("plain"), "'plain'");
}

#[test]
fn shell_quote_embedded_single_quote() {
    assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    let inputs = [
        "it's",
        "$(rm -rf /)",
        ";",
        "`echo boom`",
        "line one\nline two",
        "a b'c",
        "trailing\\",
    ];
    for input in inputs {
        let script = format!("printf %s {}", shell_quote(input));
        let output = Command::new("sh").arg("-c").arg(script).output().expect("run sh");
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), input);
    }
}

#[test]
fn validate_name_rules() {
    assert!(validate_name("window", "job-n04.graph_1").is_ok());
    assert!(validate_name("window", "a").is_ok());
    for bad in ["", "-x", "a b", "a/b", "a:b", "a;b"] {
        assert!(
            matches!(validate_name("window", bad), Err(TmuxError::InvalidName { .. })),
            "{bad:?} should be rejected"
        );
    }
    let long = "a".repeat(65);
    assert!(matches!(
        validate_name("window", &long),
        Err(TmuxError::InvalidName { .. })
    ));
}

#[test]
fn window_name_for_job_prefix() {
    assert_eq!(window_name_for_job("n04-graph"), "job-n04-graph");
}

#[test]
fn session_name_for_project_prefix() {
    assert_eq!(session_name_for_project("demo"), "vowp-demo");
}
