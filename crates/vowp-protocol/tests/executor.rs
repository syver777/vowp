//! Executor grammar and profile-ladder tests. Spec: `docs/design/crates/vowp-protocol.md` §6.

use std::collections::HashMap;
use std::time::Duration;

use pretty_assertions::assert_eq;
use vowp_protocol::prelude::*;

fn profile(rungs: &[&str]) -> Profile {
    Profile {
        ladder: rungs
            .iter()
            .map(|text| Executor::parse(text).expect("test ladder rung must parse"))
            .collect(),
        on_quota: OnQuota::NextThenProbeBack,
        node: None,
    }
}

fn assert_invalid_executor(result: Result<Executor, ProtocolError>) {
    let err = result.expect_err("expected an invalid executor error");
    assert!(
        matches!(err, ProtocolError::InvalidExecutor(..)),
        "unexpected error: {err:?}"
    );
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn executor_parse_table() {
    let valid = [
        "claude-code:example-model@medium",
        "opencode:example/model#max",
        "cli:codex",
        "opencode:anthropic/claude-x#high",
        "vowp:system",
    ];
    for text in valid {
        let executor = Executor::parse(text).unwrap();
        assert_eq!(executor.to_string(), text);
        assert_eq!(text.parse::<Executor>().unwrap(), executor);
        assert!(executor.validate().is_ok());
    }

    let invalid = [
        "nocolon",
        "opencode:m@high",
        "claude-code:m#v",
        "claude-code:",
        "unknown:m",
        "claude-code:m@Hi",
        "opencode:m#Hi",
        "claude-code:m@",
        "opencode:m#",
        "claude-code:m@med@high",
        "opencode:m#a#b",
        "cli:m@x",
        "cli:m#x",
        "opencode:m@x#y",
        "claude-code:m#v@e",
        "opencode:m model",
        "claude-code:m@high!",
    ];
    for text in invalid {
        assert_invalid_executor(Executor::parse(text));
    }

    let parsed = Executor::parse("claude-code:example-model@medium").unwrap();
    assert_eq!(parsed.harness, HarnessKind::ClaudeCode);
    assert_eq!(parsed.model, "example-model");
    assert_eq!(parsed.effort.as_deref(), Some("medium"));
    assert_eq!(parsed.variant, None);

    let parsed = Executor::parse("opencode:example/model#max").unwrap();
    assert_eq!(parsed.harness, HarnessKind::Opencode);
    assert_eq!(parsed.model, "example/model");
    assert_eq!(parsed.effort, None);
    assert_eq!(parsed.variant.as_deref(), Some("max"));

    // Struct literals can be invalid; validate catches them.
    let bad = Executor {
        harness: HarnessKind::Opencode,
        model: String::new(),
        effort: None,
        variant: None,
    };
    assert!(bad.validate().is_err());
    let bad = Executor {
        harness: HarnessKind::Opencode,
        model: "example/model".to_owned(),
        effort: Some("max".to_owned()),
        variant: None,
    };
    assert!(bad.validate().is_err());
}

#[test]
fn executor_provider_and_model_arg() {
    let claude = Executor::parse("claude-code:example-model@medium").unwrap();
    assert_eq!(claude.provider(), "claude");
    assert_eq!(claude.model_arg(), "example-model");

    let opencode = Executor::parse("opencode:example/model#max").unwrap();
    assert_eq!(opencode.provider(), "example");
    assert_eq!(opencode.model_arg(), "example/model#max");

    let codex = Executor::parse("cli:codex").unwrap();
    assert_eq!(codex.provider(), "codex");
    assert_eq!(codex.model_arg(), "codex");

    let bare_opencode = Executor::parse("opencode:claude-x").unwrap();
    assert_eq!(bare_opencode.provider(), "claude-x");
    assert_eq!(bare_opencode.model_arg(), "claude-x");

    assert_eq!(HarnessKind::ClaudeCode.as_str(), "claude-code");
    assert_eq!(HarnessKind::Opencode.as_str(), "opencode");
    assert_eq!(HarnessKind::Cli.as_str(), "cli");
    assert_eq!(HarnessKind::Vowp.as_str(), "vowp");
    assert_eq!(HarnessKind::parse("claude-code").unwrap(), HarnessKind::ClaudeCode);
    assert!(HarnessKind::parse("nope").is_err());
}

#[test]
fn executor_serde_is_string() {
    let executor = Executor::parse("opencode:example/model#max").unwrap();
    let json = serde_json::to_string(&executor).unwrap();
    assert_eq!(json, "\"opencode:example/model#max\"");
    assert_eq!(serde_json::from_str::<Executor>(&json).unwrap(), executor);
    assert!(serde_json::from_str::<Executor>("\"not-an-executor\"").is_err());

    let reference = ExecutorRef {
        executor: executor.clone(),
        node: Some(NodeName::parse("vps").unwrap()),
    };
    let value = serde_json::to_value(&reference).unwrap();
    assert_eq!(value["executor"], "opencode:example/model#max");
    assert_eq!(value["node"], "vps");
    assert_eq!(serde_json::from_value::<ExecutorRef>(value).unwrap(), reference);

    // Effort and variant are serialised in canonical order.
    let claude = Executor::parse("claude-code:example-model@medium").unwrap();
    assert_eq!(
        serde_json::to_value(&claude).unwrap(),
        serde_json::Value::String("claude-code:example-model@medium".to_owned())
    );
}

#[test]
fn profile_validate_rejects_empty_and_duplicates() {
    assert!(profile(&[]).validate().is_err());
    assert!(profile(&["cli:codex", "cli:codex"]).validate().is_err());
    assert!(
        profile(&["claude-code:example-model@medium", "opencode:example/model#max"])
            .validate()
            .is_ok()
    );

    let mut with_node = profile(&["cli:codex"]);
    with_node.node = Some(NodeName::parse("vps").unwrap());
    assert!(with_node.validate().is_ok());

    // Bad executor fields behind a struct literal are caught too.
    let mut bad = profile(&["cli:codex"]);
    bad.ladder[0].harness = HarnessKind::Opencode;
    bad.ladder[0].effort = Some("high".to_owned());
    assert!(bad.validate().is_err());
}

#[test]
fn profile_pick_skips_limited_rungs() {
    let profile = profile(&["claude-code:example-model@medium", "opencode:example/model#max"]);
    let now = Timestamp::parse("2026-03-10T19:00:00.000Z").unwrap();
    let future = now.plus(Duration::from_secs(3600));
    let past = now.minus(Duration::from_secs(3600));

    let mut limited = HashMap::new();
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(0));

    limited.insert("claude".to_owned(), Some(future));
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(1));

    limited.insert("claude".to_owned(), Some(past));
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(0));

    limited.insert("claude".to_owned(), Some(now));
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(0));

    limited.insert("claude".to_owned(), None);
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(1));

    limited.insert("example".to_owned(), None);
    assert_eq!(profile.pick(&limited, now), None);

    limited.insert("example".to_owned(), Some(future));
    assert_eq!(profile.pick(&limited, now), None);

    limited.insert("example".to_owned(), Some(past));
    assert_eq!(profile.pick(&limited, now).map(|(i, _)| i), Some(1));

    let (index, executor) = profile.pick(&limited, now).unwrap();
    assert_eq!(index, 1);
    assert_eq!(executor, &profile.ladder[1]);
}
