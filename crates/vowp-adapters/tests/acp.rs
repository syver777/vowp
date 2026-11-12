//! ACP adapter tests: the fake JSON-RPC server, event order, permission refusal and failures.
//! Spec: `docs/design/adapters.md` §10 and `docs/design/v0.2/acp-adapter.md`.
//!
//! Every test is hermetic: the server is `tests/fakes/fake_acp.py` in a temporary directory,
//! no test reaches a real ACP server, model, network or HOME.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;
use vowp_adapters::acp::{ACP_PROTOCOL_VERSION, AcpAdapter, AcpConfig};
use vowp_adapters::proc::process_alive;
use vowp_protocol::{
    Adapter, AdapterRun, CostSource, ErrorKind, EventBody, EventType, Executor, FileOp, HarnessEvent, HarnessKind,
    LimitKind, PlanStatus, RunOutcome, SessionId, StartInput, StopReason, Timestamp,
};

fn fake_acp() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes/fake_acp.py");
    let mut permissions = fs::metadata(&path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
    path
}

fn adapter() -> AcpAdapter {
    AcpAdapter::new(AcpConfig {
        binary: Some(fake_acp().to_string_lossy().into_owned()),
        ..AcpConfig::default()
    })
}

fn executor() -> Executor {
    Executor {
        harness: HarnessKind::Acp,
        model: "claude-code-acp".to_owned(),
        effort: None,
        variant: None,
    }
}

fn input(cwd: &Path, prompt: &str, mode: &str, log: Option<&Path>) -> StartInput {
    let mut env = vec![("FAKE_ACP_MODE".to_owned(), mode.to_owned())];
    if let Some(log) = log {
        env.push(("FAKE_ACP_LOG".to_owned(), log.to_string_lossy().into_owned()));
    }
    StartInput {
        session_id: SessionId::new(),
        executor: executor(),
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_owned(),
        resume: None,
        new_native_id: None,
        env,
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    }
}

/// Collects events until the channel closes, with a hard test timeout.
fn collect(run: &mut dyn AdapterRun) -> Vec<HarnessEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();
    loop {
        match run.events().recv_timeout(Duration::from_millis(50)) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                panic!("timed out waiting for ACP events");
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    events
}

fn event_types(events: &[HarnessEvent]) -> Vec<&'static str> {
    events.iter().map(|event| event.body.event_type().as_str()).collect()
}

fn tool_calls(events: &[HarnessEvent]) -> Vec<&vowp_protocol::ToolCall> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[HarnessEvent]) -> Vec<&vowp_protocol::ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn errors(events: &[HarnessEvent]) -> Vec<&vowp_protocol::ErrorEvent> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Error(error) => Some(error),
            _ => None,
        })
        .collect()
}

fn turn_endings(events: &[HarnessEvent]) -> Vec<&vowp_protocol::TurnEnded> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::TurnEnded(ended) => Some(ended),
            _ => None,
        })
        .collect()
}

/// Reads a JSONL log the fake server wrote.
fn read_log(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("log line is JSON: {error}\n{line}")))
        .collect()
}

#[test]
fn acp_probe_reports_the_binary_without_a_version_call() {
    let adapter = adapter();
    let probe = adapter.probe();
    assert!(probe.ok, "probe should find the fake server: {}", probe.detail);
    assert_eq!(probe.version, None);
    assert_eq!(probe.path, Some(fake_acp().to_string_lossy().into_owned()));
    assert!(probe.detail.contains("found"));

    let missing = AcpAdapter::new(AcpConfig {
        binary: Some("/definitely/not/a/vowp-acp-server".to_owned()),
        ..AcpConfig::default()
    })
    .probe();
    assert!(!missing.ok);
    assert_eq!(missing.version, None);
    assert_eq!(missing.path, None);
    assert!(missing.detail.contains("vowp-acp-server"), "{}", missing.detail);
}

#[test]
fn acp_capabilities_are_honest() {
    let capabilities = adapter().capabilities();
    assert!(!capabilities.can_resume_native);
    assert!(!capabilities.can_inject_assistant_history);
    assert!(!capabilities.can_switch_model_in_session);
    assert!(!capabilities.live_input);
    assert!(!capabilities.emits_reasoning);
    assert!(!capabilities.emits_cost_receipts);
    assert!(!capabilities.reports_resets_at);
}

#[test]
fn acp_clean_turn_emits_events_in_order() {
    let temp = TempDir::new().expect("temp dir");
    let log = temp.path().join("requests.jsonl");
    let mut run = adapter()
        .start(input(temp.path(), "do the work", "ok", Some(&log)))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    assert_eq!(
        event_types(&events),
        vec![
            "tool.call",
            "tool.result",
            "tool.call",
            "tool.result",
            "file.change",
            "plan",
            "assistant.message",
            "cost",
            "turn.ended",
        ],
        "{events:#?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event.body,
            EventBody::UserMessage(_) | EventBody::TurnStarted(_) | EventBody::SessionExecutor(_)
        )),
        "adapters never emit framing events"
    );

    let calls = tool_calls(&events);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id, "tool-read-1");
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].input["path"], "README.md");
    assert_eq!(calls[1].call_id, "tool-edit-2");
    assert_eq!(calls[1].name, "edit");

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert!(results[0].ok);
    assert_eq!(results[0].output.as_deref(), Some("readme contents"));
    assert!(results[1].ok);
    assert_eq!(results[1].output.as_deref(), Some("edited"));

    let EventBody::FileChange(file) = &events[4].body else {
        panic!("the edit completes with a file change");
    };
    assert_eq!(file.path, "src/lib.rs");
    assert_eq!(file.op, FileOp::Modify);

    let EventBody::Plan(plan) = &events[5].body else {
        panic!("the plan update is mapped");
    };
    assert_eq!(plan.items.len(), 2);
    assert_eq!(plan.items[0].text, "read the file");
    assert_eq!(plan.items[0].status, PlanStatus::Done);
    assert_eq!(plan.items[1].status, PlanStatus::InProgress);

    let EventBody::AssistantMessage(message) = &events[6].body else {
        panic!("chunks accumulate into one message");
    };
    assert_eq!(message.text, "Hello from ACP.");

    let EventBody::Cost(cost) = &events[7].body else {
        panic!("token usage is a cost event");
    };
    assert_eq!(cost.cost.source, CostSource::Unknown);
    assert_eq!(cost.cost.usd, None);
    assert_eq!(cost.cost.tokens.input, 120);
    assert_eq!(cost.cost.tokens.output, 30);
    assert_eq!(cost.cost.tokens.cache_read, 4);
    assert_eq!(cost.cost.tokens.total(), 154);

    let endings = turn_endings(&events);
    assert_eq!(endings.len(), 1);
    assert_eq!(endings[0].stop_reason, StopReason::EndTurn);

    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(summary.native_session_id.as_deref(), Some("sess_fake_acp_1"));
    assert_eq!(summary.cost.tokens.input, 120);
    assert_eq!(summary.result_text.as_deref(), Some("Hello from ACP."));

    let messages = read_log(&log);
    let requests: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("method").is_some())
        .collect();
    assert_eq!(requests.len(), 3, "one handshake, one session, one prompt");
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[0]["params"]["protocolVersion"], ACP_PROTOCOL_VERSION);
    assert_eq!(requests[0]["params"]["clientInfo"]["name"], "vowp");
    assert_eq!(requests[1]["method"], "session/new");
    assert_eq!(
        requests[1]["params"]["cwd"].as_str(),
        Some(temp.path().to_string_lossy().as_ref())
    );
    assert_eq!(requests[2]["method"], "session/prompt");
    assert_eq!(requests[2]["params"]["sessionId"], "sess_fake_acp_1");
    assert_eq!(requests[2]["params"]["prompt"][0]["type"], "text");
    assert_eq!(requests[2]["params"]["prompt"][0]["text"], "do the work");
}

#[test]
fn acp_permission_request_is_never_approved() {
    let temp = TempDir::new().expect("temp dir");
    let log = temp.path().join("requests.jsonl");
    let mut run = adapter()
        .start(input(temp.path(), "edit a file", "permission", Some(&log)))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    assert_eq!(
        event_types(&events),
        vec!["note", "error", "assistant.message", "turn.ended"],
        "{events:#?}"
    );
    let refusals = errors(&events);
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].kind, ErrorKind::Refused);
    assert!(!refusals[0].retryable);
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Error);

    let messages = read_log(&log);
    let response = messages
        .iter()
        .find_map(|message| message.get("response"))
        .expect("the client answered the permission request");
    assert_eq!(response["result"]["outcome"]["outcome"], "cancelled");
    let response_text = response.to_string();
    assert!(!response_text.contains("allow-once"), "{response_text}");
    assert!(!response_text.contains("optionId"), "{response_text}");
}

#[test]
fn acp_server_exiting_without_a_result_fails_cleanly() {
    let temp = TempDir::new().expect("temp dir");
    let mut run = adapter()
        .start(input(temp.path(), "do the work", "no-result", None))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    assert_eq!(event_types(&events), vec!["assistant.message", "error", "turn.ended"]);
    let failures = errors(&events);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].kind, ErrorKind::AdapterFailed);
    assert!(
        failures[0].message.contains("without a prompt result"),
        "{}",
        failures[0].message
    );
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Crash);
    assert_eq!(summary.exit_code, Some(0));
}

#[test]
fn acp_malformed_json_fails_cleanly() {
    let temp = TempDir::new().expect("temp dir");
    let mut run = adapter()
        .start(input(temp.path(), "do the work", "malformed", None))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    let failures = errors(&events);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].kind, ErrorKind::AdapterFailed);
    assert!(
        failures[0].message.contains("malformed JSON"),
        "{}",
        failures[0].message
    );
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Crash);
    assert_eq!(turn_endings(&events).len(), 1);
}

#[test]
fn acp_unknown_method_fails_cleanly_and_answers_the_request() {
    let temp = TempDir::new().expect("temp dir");
    let log = temp.path().join("requests.jsonl");
    let mut run = adapter()
        .start(input(temp.path(), "do the work", "unknown-method", Some(&log)))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    let failures = errors(&events);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].kind, ErrorKind::AdapterFailed);
    assert!(
        failures[0].message.contains("unknown method"),
        "{}",
        failures[0].message
    );
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Crash);

    let messages = read_log(&log);
    let response = messages
        .iter()
        .find_map(|message| message.get("response"))
        .expect("the client answered the unknown method");
    assert_eq!(response["error"]["code"], -32601);
}

#[test]
fn acp_rpc_error_classifies_a_quota_limit() {
    let temp = TempDir::new().expect("temp dir");
    let mut run = adapter()
        .start(input(temp.path(), "do the work", "quota", None))
        .expect("start fake ACP server");
    let events = collect(run.as_mut());
    let summary = run.wait().expect("fake server exits");

    assert_eq!(
        event_types(&events),
        vec!["error", "limit", "turn.ended"],
        "{events:#?}"
    );
    let limits: Vec<&vowp_protocol::LimitEvent> = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Limit(limit) => Some(limit),
            _ => None,
        })
        .collect();
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Quota);
    assert_eq!(limits[0].resets_at, Some(Timestamp::from_unix_secs(1_794_164_400)));
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(summary.exit_code, Some(1));
    assert!(summary.limit.is_some());
}

#[test]
fn acp_stop_kills_the_process_group() {
    let temp = TempDir::new().expect("temp dir");
    let child_pid_path = temp.path().join("child.pid");
    let mut start = input(temp.path(), "hang", "hang", None);
    start.env.push((
        "FAKE_ACP_CHILD_PID".to_owned(),
        child_pid_path.to_string_lossy().into_owned(),
    ));
    let mut run = adapter().start(start).expect("start fake ACP server");

    let first = run
        .events()
        .recv_timeout(Duration::from_secs(10))
        .expect("the fake server announces a tool call");
    assert!(matches!(first.body, EventBody::ToolCall(_)));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !child_pid_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let child_pid: u32 = fs::read_to_string(&child_pid_path)
        .expect("fake server records its child pid")
        .trim()
        .parse()
        .expect("child pid is a number");

    let stopped = Instant::now();
    run.stop().expect("stop the ACP run");
    assert!(stopped.elapsed() < Duration::from_secs(6), "stop must not hang");

    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(child_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_alive(child_pid), "the process group must be killed");

    let events = collect(run.as_mut());
    let summary = run.wait().expect("run summary after stop");
    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);
    assert_eq!(turn_endings(&events).len(), 1);
    assert!(events.iter().all(|event| event.body.event_type() != EventType::Cost));
}

#[test]
fn acp_extra_args_follow_the_binary() {
    let temp = TempDir::new().expect("temp dir");
    let log = temp.path().join("requests.jsonl");
    let adapter = AcpAdapter::new(AcpConfig {
        binary: Some(fake_acp().to_string_lossy().into_owned()),
        extra_args: vec!["--flag".to_owned()],
        ..AcpConfig::default()
    });
    let mut run = adapter
        .start(input(temp.path(), "do the work", "ok", Some(&log)))
        .expect("start fake ACP server");
    let _ = collect(run.as_mut());
    let _ = run.wait().expect("fake server exits");

    let messages = read_log(&log);
    let argv = messages
        .iter()
        .find_map(|message| message.get("argv"))
        .expect("the fake server logs argv");
    assert_eq!(argv[0], fake_acp().to_string_lossy().as_ref());
    assert_eq!(argv[1], "--flag");
}
