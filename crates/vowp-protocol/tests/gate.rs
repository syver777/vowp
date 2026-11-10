//! Gate, evidence and default-set tests. Spec: `docs/design/types.md` §8 and
//! `docs/design/crates/vowp-protocol.md` §6.

use pretty_assertions::assert_eq;
use serde_json::json;

use vowp_protocol::gate::{
    CmdGate, DeployGate, Evidence, Gate, GateKind, GateResult, PushedGate, ReportGate, ReviewGate, SecretHit,
    default_gates,
};
use vowp_protocol::{JobId, JobKind, ProtocolError, Role, Timestamp};

#[test]
fn gate_short_form_table() {
    assert_eq!(
        Gate::from_short("report").expect("known keyword"),
        Gate::Report(ReportGate::default())
    );
    assert_eq!(
        Gate::from_short("owns_respected").expect("known keyword"),
        Gate::OwnsRespected(true)
    );
    assert_eq!(
        Gate::from_short("review").expect("known keyword"),
        Gate::Review(ReviewGate::default())
    );
    assert_eq!(
        Gate::from_short("pushed").expect("known keyword"),
        Gate::Pushed(PushedGate::default())
    );
    assert_eq!(
        Gate::from_short("secret_scan").expect("known keyword"),
        Gate::SecretScan(true)
    );

    let command = Gate::from_short("cargo test -p x").expect("unknown short forms are commands");
    assert_eq!(
        command,
        Gate::Cmd(CmdGate {
            run: "cargo test -p x".to_owned(),
            expect_exit: 0,
            expect_pattern: None,
            timeout_secs: 1800,
            cwd: None,
        })
    );
    assert_eq!(command.short(), "cargo test -p x");
    assert_eq!(command.kind(), GateKind::Cmd);

    let short_forms = [
        Gate::Report(ReportGate::default()),
        Gate::OwnsRespected(true),
        Gate::Review(ReviewGate::default()),
        Gate::Pushed(PushedGate::default()),
        Gate::SecretScan(true),
        Gate::Cmd(CmdGate {
            run: "cargo test -p x".to_owned(),
            ..CmdGate::default()
        }),
    ];
    for gate in short_forms {
        assert_eq!(
            Gate::from_short(&gate.short()).expect("short form parses"),
            gate.clone(),
            "{}",
            gate.short()
        );
    }

    assert_eq!(
        Gate::from_short("  report  ").expect("trimmed keyword"),
        Gate::Report(ReportGate::default())
    );
    assert!(matches!(Gate::from_short(""), Err(ProtocolError::InvalidGate(_))));
    assert!(matches!(Gate::from_short("   "), Err(ProtocolError::InvalidGate(_))));
}

#[test]
fn gate_serde_matches_plan_yaml() {
    let examples = [
        json!({"cmd": {"run": "x", "expect_exit": 0}}),
        json!({"owns_respected": true}),
        json!({"review": {"by": "verifier"}}),
        json!({"report": {"fields": ["status", "branch", "commits", "tests"]}}),
        json!({"pushed": {"remote": "origin"}}),
        json!({"secret_scan": true}),
        json!({"script": {"run": "scripts/check.sh"}}),
    ];
    for raw in examples {
        let gate: Gate = serde_json::from_value(raw.clone()).expect("valid gate JSON");
        let encoded = serde_json::to_value(&gate).expect("gate is serialisable");
        let decoded: Gate = serde_json::from_value(encoded).expect("roundtrip");
        assert_eq!(decoded, gate);
    }

    let cmd: Gate = serde_json::from_value(json!({"cmd": {"run": "x", "expect_exit": 0}})).unwrap();
    assert_eq!(cmd.kind(), GateKind::Cmd);
    let review: Gate = serde_json::from_value(json!({"review": {"by": "verifier"}})).unwrap();
    assert_eq!(review.kind(), GateKind::Review);

    let unknown = serde_json::from_value::<Gate>(json!({"nonsense": true}));
    assert!(unknown.is_err(), "unknown gate key must fail");
}

#[test]
fn gate_validate_rejects_bad_regex_and_empty_run() {
    let bad_regex = Gate::Cmd(CmdGate {
        run: "cargo test".to_owned(),
        expect_pattern: Some("(".to_owned()),
        ..CmdGate::default()
    });
    assert!(matches!(bad_regex.validate(), Err(ProtocolError::InvalidGate(_))));

    let empty_run = Gate::Cmd(CmdGate {
        run: "   ".to_owned(),
        ..CmdGate::default()
    });
    assert!(matches!(empty_run.validate(), Err(ProtocolError::InvalidGate(_))));

    let zero_timeout = Gate::Cmd(CmdGate {
        run: "true".to_owned(),
        timeout_secs: 0,
        ..CmdGate::default()
    });
    assert!(matches!(zero_timeout.validate(), Err(ProtocolError::InvalidGate(_))));

    let good = Gate::Cmd(CmdGate {
        run: "cargo test -p vowp-protocol".to_owned(),
        expect_pattern: Some("test result: ok".to_owned()),
        ..CmdGate::default()
    });
    assert!(good.validate().is_ok());

    let deploy = Gate::Deploy(DeployGate {
        run: "deploy.sh".to_owned(),
        probe: CmdGate {
            run: "curl -fsS localhost/health".to_owned(),
            ..CmdGate::default()
        },
        env: Some("prod".to_owned()),
    });
    assert!(deploy.validate().is_ok());
    assert_eq!(deploy.kind(), GateKind::Deploy);
}

#[test]
fn gate_validate_rejects_file_exists_only_cmd() {
    for run in [
        "test -f Cargo.toml",
        "test -e Cargo.toml",
        "test -d src",
        "[ -f Cargo.toml ]",
        "[ -e Cargo.toml ]",
        "ls -la",
        "stat Cargo.toml",
        "cat Cargo.toml",
        "true",
        ":",
    ] {
        let gate = Gate::Cmd(CmdGate {
            run: run.to_owned(),
            ..CmdGate::default()
        });
        match gate.validate() {
            Err(ProtocolError::InvalidGate(message)) => {
                assert!(message.contains("anti-pattern"), "{run:?}: {message}");
                assert!(message.contains("execute and check results"), "{run:?}: {message}");
            }
            other => panic!("{run:?} only checks existence; expected rejection, got {other:?}"),
        }
    }

    let script = Gate::Script(CmdGate {
        run: "test -f Cargo.toml".to_owned(),
        ..CmdGate::default()
    });
    assert!(matches!(script.validate(), Err(ProtocolError::InvalidGate(_))));

    let deploy_run = Gate::Deploy(DeployGate {
        run: "test -f target/release/vowp".to_owned(),
        probe: CmdGate {
            run: "curl -fsS localhost/health".to_owned(),
            ..CmdGate::default()
        },
        env: None,
    });
    assert!(matches!(deploy_run.validate(), Err(ProtocolError::InvalidGate(_))));

    let deploy_probe = Gate::Deploy(DeployGate {
        run: "cargo run -- deploy".to_owned(),
        probe: CmdGate {
            run: "test -e target/release/vowp".to_owned(),
            ..CmdGate::default()
        },
        env: None,
    });
    assert!(matches!(deploy_probe.validate(), Err(ProtocolError::InvalidGate(_))));
}

#[test]
fn gate_validate_rejects_nonzero_exit_without_pattern() {
    let no_pattern = Gate::Cmd(CmdGate {
        run: "grep -q 'needle' haystack.txt".to_owned(),
        expect_exit: 1,
        ..CmdGate::default()
    });
    match no_pattern.validate() {
        Err(ProtocolError::InvalidGate(message)) => {
            assert!(message.contains("anti-pattern"), "{message}");
            assert!(message.contains("expect_pattern"), "{message}");
        }
        other => panic!("a nonzero exit without a pattern must be rejected, got {other:?}"),
    }

    let with_pattern = Gate::Cmd(CmdGate {
        run: "grep -q 'needle' haystack.txt".to_owned(),
        expect_exit: 1,
        expect_pattern: Some("needle".to_owned()),
        ..CmdGate::default()
    });
    assert!(
        with_pattern.validate().is_ok(),
        "a negative gate that asserts its pattern is behavioural"
    );
}

#[test]
fn gate_validate_rejects_empty_pattern() {
    for pattern in ["", "   "] {
        let gate = Gate::Cmd(CmdGate {
            run: "grep -q 'needle' haystack.txt".to_owned(),
            expect_exit: 1,
            expect_pattern: Some(pattern.to_owned()),
            ..CmdGate::default()
        });
        match gate.validate() {
            Err(ProtocolError::InvalidGate(message)) => {
                assert!(message.contains("non-empty expect_pattern"), "{pattern:?}: {message}");
            }
            other => panic!("an empty pattern counts as no pattern; expected rejection, got {other:?}"),
        }
    }

    let noop_on_success = Gate::Cmd(CmdGate {
        run: "printf 'ok\\n'".to_owned(),
        expect_exit: 0,
        expect_pattern: Some(String::new()),
        ..CmdGate::default()
    });
    assert!(
        noop_on_success.validate().is_ok(),
        "an empty pattern is harmless when a zero exit is expected"
    );
}

#[test]
fn gate_validate_accepts_behavioural_cmd() {
    let gate = Gate::Cmd(CmdGate {
        run: "cargo test -p vowp-protocol".to_owned(),
        expect_pattern: Some("test result: ok".to_owned()),
        ..CmdGate::default()
    });
    assert!(gate.validate().is_ok());

    let negative = Gate::Cmd(CmdGate {
        run: "grep -q 'needle' haystack.txt".to_owned(),
        expect_exit: 1,
        expect_pattern: Some("needle".to_owned()),
        ..CmdGate::default()
    });
    assert!(negative.validate().is_ok());

    let script = Gate::Script(CmdGate {
        run: "scripts/check.sh".to_owned(),
        expect_pattern: Some("all checks passed".to_owned()),
        ..CmdGate::default()
    });
    assert!(script.validate().is_ok());

    let deploy = Gate::Deploy(DeployGate {
        run: "scripts/deploy.sh".to_owned(),
        probe: CmdGate {
            run: "curl -fsS localhost/health".to_owned(),
            ..CmdGate::default()
        },
        env: Some("prod".to_owned()),
    });
    assert!(deploy.validate().is_ok());
}

#[test]
fn default_gates_per_kind() {
    let kinds = |kind| default_gates(kind).iter().map(Gate::kind).collect::<Vec<_>>();

    let build = kinds(JobKind::Build);
    assert!(build.contains(&GateKind::Report));
    assert!(build.contains(&GateKind::OwnsRespected));
    assert!(build.contains(&GateKind::SecretScan));
    assert!(build.contains(&GateKind::Review));

    assert_eq!(kinds(JobKind::Verify), vec![GateKind::Report]);
    assert_eq!(kinds(JobKind::Decision), Vec::<GateKind>::new());
    assert_eq!(kinds(JobKind::Release), Vec::<GateKind>::new());
    assert_eq!(kinds(JobKind::Promote), Vec::<GateKind>::new());
    assert_eq!(kinds(JobKind::Merge), vec![GateKind::Report]);

    for kind in [JobKind::Fix, JobKind::Test] {
        let gates = kinds(kind);
        assert!(gates.contains(&GateKind::Report));
        assert!(gates.contains(&GateKind::OwnsRespected));
        assert!(gates.contains(&GateKind::SecretScan));
        assert!(!gates.contains(&GateKind::Review));
    }
    for kind in [JobKind::Docs, JobKind::Spec] {
        assert_eq!(
            kinds(kind),
            vec![
                GateKind::Report,
                GateKind::OwnsRespected,
                GateKind::SecretScan,
                GateKind::Review
            ]
        );
    }
}

#[test]
fn evidence_serde_tagged() {
    let variants = vec![
        Evidence::Report {
            front_matter: Some(json!({"job": "sample-job", "status": "done"})),
            missing_fields: vec!["tests".to_owned()],
            problems: Vec::new(),
        },
        Evidence::Cmd {
            command: "cargo test".to_owned(),
            exit_code: Some(0),
            timed_out: false,
            tail: "test result: ok".to_owned(),
            duration_ms: 42,
        },
        Evidence::Script {
            command: "scripts/check.sh".to_owned(),
            exit_code: Some(1),
            timed_out: false,
            tail: "failed".to_owned(),
            duration_ms: 7,
        },
        Evidence::OwnsRespected {
            changed: vec!["crates/vowp-graph/src/lib.rs".to_owned()],
            violations: Vec::new(),
        },
        Evidence::Pushed {
            remote: "origin".to_owned(),
            branch: "job/sample-job".to_owned(),
            ls_remote: "abc\trefs/heads/job/sample-job".to_owned(),
            local_sha: Some("abc".to_owned()),
        },
        Evidence::Review {
            verify_job: Some(JobId::parse("sample-verify").unwrap()),
            verdict: Some(vowp_protocol::Verdict::Pass),
            findings: Vec::new(),
        },
        Evidence::Deploy {
            command: "deploy.sh".to_owned(),
            exit_code: Some(0),
            probe: Box::new(Evidence::Cmd {
                command: "curl -fsS localhost/health".to_owned(),
                exit_code: Some(0),
                timed_out: false,
                tail: "ok".to_owned(),
                duration_ms: 3,
            }),
            version_before: Some("v1".to_owned()),
            version_after: Some("v2".to_owned()),
            rollback_cmd: Some("rollback.sh".to_owned()),
        },
        Evidence::SecretScan {
            files_scanned: 12,
            hits: vec![SecretHit {
                path: "src/lib.rs".to_owned(),
                line: 9,
                rule: "aws-access-key".to_owned(),
            }],
        },
    ];

    for evidence in variants {
        let encoded = serde_json::to_value(&evidence).expect("evidence is serialisable");
        assert!(encoded.get("kind").is_some(), "evidence is tagged: {encoded}");
        let decoded: Evidence = serde_json::from_value(encoded).expect("roundtrip");
        assert_eq!(decoded, evidence);
    }

    let scan = serde_json::to_value(Evidence::SecretScan {
        files_scanned: 1,
        hits: vec![SecretHit {
            path: "src/lib.rs".to_owned(),
            line: 1,
            rule: "rule-name".to_owned(),
        }],
    })
    .unwrap();
    assert_eq!(scan["kind"], "secret_scan");
    let hit = &scan["hits"][0];
    assert!(hit.get("value").is_none(), "secret values are never stored");
    assert_eq!(hit["rule"], "rule-name");
}

#[test]
fn gate_result_roundtrips_fixture_shape() {
    let raw = json!({
        "id": 42,
        "job_id": "sample-job",
        "run_id": null,
        "attempt": 0,
        "gate_index": 1,
        "gate": {"cmd": {"run": "cargo test -p vowp-graph", "expect_exit": 0, "expect_pattern": null, "timeout_secs": 1800, "cwd": null}},
        "passed": true,
        "evidence": {"kind": "cmd", "command": "cargo test -p vowp-graph", "exit_code": 0, "timed_out": false,
                     "tail": "test result: ok. 31 passed", "duration_ms": 41234},
        "output_blob": null,
        "duration_ms": 41234,
        "ts": "2026-03-10T20:03:00.000Z"
    });
    let result: GateResult = serde_json::from_value(raw.clone()).expect("gate result decodes");
    assert_eq!(result.gate_index, 1);
    assert!(result.passed);
    assert_eq!(serde_json::to_value(&result).expect("serialisable"), raw);
    assert_eq!(Timestamp::parse("2026-03-10T20:03:00.000Z").unwrap(), result.ts);
    assert_eq!(Role::Verifier, ReviewGate::default().by);
}
