//! `cmd` gate tests: exit codes, expected patterns, timeouts and evidence tails.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_core::ledger::Ledger;
use vowp_core::vault;
use vowp_protocol::{CmdGate, Evidence, Gate, JobKind, LedgerEntry, LedgerKind, SecretName, VowpConfig};
use vowp_store::LedgerFilter;
use vowp_store::blobs::BlobStore;

/// Builds one evaluable job and its dependencies.
fn setup(
    tmp: &Path,
) -> (
    vowp_core::Deps,
    vowp_protocol::Job,
    vowp_store::Store,
    std::path::PathBuf,
) {
    let node = tmp.join("node");
    let wt = worktree(tmp);
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "cmd-test", JobKind::Build, &["src/**"], Vec::new());
    let job = set_workspace(&store, &job, "job/cmd-test", Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store, wt)
}

/// Evaluates one command gate.
fn evaluate(
    deps: &vowp_core::Deps,
    job: &vowp_protocol::Job,
    store: &vowp_store::Store,
    wt: &Path,
    gate: &CmdGate,
) -> vowp_protocol::GateResult {
    let runner = GateRunner::new(deps.clone());
    let context = ctx(deps, job, store, wt, wt);
    runner.evaluate(&context, 0, &Gate::Cmd(gate.clone())).unwrap()
}

#[test]
fn cmd_gate_exit_zero_passes() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "printf 'all good\\n'".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(result.passed);
    match &result.evidence {
        Evidence::Cmd {
            exit_code,
            timed_out,
            tail,
            ..
        } => {
            assert_eq!(*exit_code, Some(0));
            assert!(!timed_out);
            assert!(tail.contains("all good"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_wrong_exit_fails_with_tail() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "printf 'boom detail\\n'; exit 3".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd { exit_code, tail, .. } => {
            assert_eq!(*exit_code, Some(3));
            assert!(tail.contains("boom detail"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_expect_pattern() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());

    let matching = CmdGate {
        run: "printf 'test result: ok. 31 passed\\n'".to_owned(),
        expect_pattern: Some("31 passed".to_owned()),
        ..CmdGate::default()
    };
    assert!(evaluate(&deps, &job, &store, &wt, &matching).passed);

    let not_matching = CmdGate {
        run: "printf 'test result: FAILED\\n'".to_owned(),
        expect_pattern: Some("31 passed".to_owned()),
        ..CmdGate::default()
    };
    let result = evaluate(&deps, &job, &store, &wt, &not_matching);
    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd { exit_code, tail, .. } => {
            assert_eq!(*exit_code, Some(0), "the process itself exited fine");
            assert!(tail.contains("FAILED"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_timeout_kills_group_and_fails() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let marker = wt.join("survived");
    let gate = CmdGate {
        run: format!("(sleep 2; printf survived > '{}') & sleep 30", marker.display()),
        timeout_secs: 1,
        ..CmdGate::default()
    };

    let started = std::time::Instant::now();
    let result = evaluate(&deps, &job, &store, &wt, &gate);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "gate should return promptly after the timeout"
    );
    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd {
            timed_out, exit_code, ..
        } => {
            assert!(timed_out, "timeout must be recorded");
            assert_eq!(*exit_code, None, "a killed process has no exit code");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }

    std::thread::sleep(Duration::from_millis(2300));
    assert!(
        fs::metadata(&marker).is_err(),
        "the background child survived; the gate did not kill the process group"
    );
}

#[test]
fn cmd_gate_tail_is_last_200_lines() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "for i in $(seq 1 250); do echo line$i; done".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(result.passed);
    match &result.evidence {
        Evidence::Cmd { tail, .. } => {
            let lines: Vec<&str> = tail.lines().collect();
            assert_eq!(lines.len(), 200, "tail must hold exactly the last 200 lines");
            assert_eq!(lines.first(), Some(&"line51"));
            assert_eq!(lines.last(), Some(&"line250"));
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

/// Returns the path of a secret written into the temporary node home.
fn secret_file(deps: &vowp_core::Deps, name: &SecretName) -> std::path::PathBuf {
    deps.paths.root().join("secrets").join(name.as_str())
}

/// Evaluates and persists one command gate, returning the stored row.
fn evaluate_stored(
    deps: &vowp_core::Deps,
    job: &vowp_protocol::Job,
    store: &vowp_store::Store,
    wt: &Path,
    gate: &CmdGate,
) -> vowp_protocol::GateResult {
    let result = evaluate(deps, job, store, wt, gate);
    store.insert_gate_result(&result).unwrap();
    store.latest_gate_results(&job.id).unwrap().pop().unwrap()
}

#[test]
fn cmd_gate_redacts_injected_secret_from_stored_evidence() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let name = SecretName::parse("GATE_KEY").unwrap();
    let secret = "gate-secret-value-1234";
    vault::write_secret(&deps, &name, secret).unwrap();
    let path = secret_file(&deps, &name);
    let gate = CmdGate {
        run: format!("printf 'plain-line\\n'; sed -n '1p' '{}'", path.display()),
        // The pattern still sees the command's real output, not the redacted tail.
        expect_pattern: Some(secret.to_owned()),
        ..CmdGate::default()
    };

    let stored = evaluate_stored(&deps, &job, &store, &wt, &gate);

    assert!(stored.passed);
    match &stored.evidence {
        Evidence::Cmd { tail, .. } => {
            assert!(tail.contains("[REDACTED:GATE_KEY]"), "tail: {tail:?}");
            assert!(!tail.contains(secret), "the injected value was stored: {tail:?}");
            assert!(tail.contains("plain-line"), "uninjected output is unchanged: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_redacts_injected_secret_from_spilled_blob() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let name = SecretName::parse("SPILL_KEY").unwrap();
    let secret = "spilled-secret-value-3456";
    vault::write_secret(&deps, &name, secret).unwrap();
    let path = secret_file(&deps, &name);
    let gate = CmdGate {
        run: format!(
            "i=0; while [ \"$i\" -lt 900 ]; do printf 'filler-%04d\\n' \"$i\"; i=$((i+1)); done; sed -n '1p' '{}'",
            path.display()
        ),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);
    let blob = result.output_blob.as_deref().expect("large output is spilled");
    let blobs = BlobStore::new(deps.paths.root().join("gates").join("blobs"));
    let text = String::from_utf8(blobs.get(blob).unwrap()).unwrap();
    assert!(text.contains("[REDACTED:SPILL_KEY]"), "spilled blob: {text}");
    assert!(!text.contains(secret), "the injected value was spilled: {text}");
}

#[test]
fn ledger_record_gate_redacts_secret_from_detail() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let name = SecretName::parse("LEDGER_KEY").unwrap();
    let secret = "ledger-secret-value-5678";
    vault::write_secret(&deps, &name, secret).unwrap();
    let path = secret_file(&deps, &name);
    let gate = CmdGate {
        run: format!("sed -n '1p' '{}'", path.display()),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);
    Ledger::new(deps.clone()).record_gate(&store, &result).unwrap();

    let entries = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Gate),
            job_id: Some(job.id.clone()),
            since: None,
            limit: None,
        })
        .unwrap();
    assert_eq!(entries.len(), 1, "one gate row was written");
    let entry = &entries[0];
    assert_eq!(entry.kind, LedgerKind::Gate);
    assert_eq!(entry.job_id.as_ref(), Some(&job.id));
    let detail = entry.detail.to_string();
    assert!(detail.contains("[REDACTED:LEDGER_KEY]"), "detail: {detail}");
    assert!(!detail.contains(secret), "the injected value was stored: {detail}");
}

#[test]
fn ledger_record_redacts_summary_and_detail_strings_without_touching_keys() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, _wt) = setup(tmp.path());
    let name = SecretName::parse("NOTE_KEY").unwrap();
    let secret = "ledger-note-value-9012";
    vault::write_secret(&deps, &name, secret).unwrap();

    let entry = LedgerEntry {
        id: None,
        ts: store.clock().now(),
        kind: LedgerKind::Note,
        project: None,
        job_id: Some(job.id.clone()),
        run_id: None,
        session_id: None,
        role: None,
        provider: Some("deepseek".to_owned()),
        harness: None,
        model: None,
        cost: None,
        summary: format!("note {secret}"),
        detail: serde_json::json!({
            "note": format!("detail {secret}"),
            "kept_key": "plain-value",
        }),
    };
    Ledger::new(deps.clone()).record(&store, entry).unwrap();

    let entries = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            job_id: Some(job.id.clone()),
            since: None,
            limit: None,
        })
        .unwrap();
    assert_eq!(entries.len(), 1, "one note row was written");
    let entry = &entries[0];
    assert_eq!(entry.summary, "note [REDACTED:NOTE_KEY]");
    assert_eq!(entry.detail["note"], "detail [REDACTED:NOTE_KEY]");
    assert_eq!(
        entry.detail["kept_key"], "plain-value",
        "object keys and plain values are kept"
    );
    assert_eq!(
        entry.provider.as_deref(),
        Some("deepseek"),
        "other columns are unchanged"
    );
}

#[test]
fn cmd_gate_without_secret_output_is_unchanged() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let name = SecretName::parse("UNUSED_KEY").unwrap();
    vault::write_secret(&deps, &name, "never-printed-value").unwrap();
    let gate = CmdGate {
        run: "printf 'plain output\\n'".to_owned(),
        ..CmdGate::default()
    };

    let stored = evaluate_stored(&deps, &job, &store, &wt, &gate);

    match &stored.evidence {
        Evidence::Cmd { tail, .. } => assert_eq!(tail, "plain output"),
        other => panic!("expected cmd evidence, got {other:?}"),
    }

    Ledger::new(deps.clone()).record_gate(&store, &stored).unwrap();
    let entries = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Gate),
            job_id: Some(job.id.clone()),
            since: None,
            limit: None,
        })
        .unwrap();
    let detail = entries[0].detail.to_string();
    assert!(detail.contains("plain output"), "detail: {detail}");
    assert!(!detail.contains("[REDACTED:"), "no marker is introduced: {detail}");
}
