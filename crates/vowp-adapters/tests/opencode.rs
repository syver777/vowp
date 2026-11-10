//! opencode adapter tests: argv, recorded JSONL fixtures, cost, limits and process controls.
//! Spec: `docs/design/crates/vowp-adapters.md` §7 and `docs/design/adapters.md` §3.
//!
//! Every test is hermetic: fixtures are sanitised recordings, the fake harness is
//! `tests/fakes/fake-opencode.sh`, and no test reaches a real `opencode` binary or HOME.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use vowp_adapters::opencode::{OpencodeAdapter, OpencodeConfig, OpencodeParser, build_argv};
use vowp_adapters::stream::{ParsedRun, StreamParser};
use vowp_protocol::{
    Actor, Adapter, Cost, CostEvent, CostSource, EventBody, Executor, HarnessEvent, HarnessKind, LimitEvent, LimitKind,
    RunOutcome, SessionId, StartInput, StopReason, Timestamp, ToolCall, ToolResult, TurnEnded,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn fake_opencode() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes/fake-opencode.sh")
}

fn now() -> Timestamp {
    Timestamp::parse("2026-03-10T18:10:00Z").expect("test timestamp is valid")
}

fn ts(value: &str) -> Timestamp {
    Timestamp::parse(value).expect("test timestamp is valid")
}

fn executor(variant: Option<&str>) -> Executor {
    Executor {
        harness: HarnessKind::Opencode,
        model: "example/model".to_owned(),
        effort: None,
        variant: variant.map(str::to_owned),
    }
}

fn parser_at(now: Timestamp) -> OpencodeParser {
    OpencodeParser::new(now, 0, executor(Some("max")))
}

fn input(cwd: &Path, prompt: &str, resume: Option<&str>) -> StartInput {
    StartInput {
        session_id: SessionId::new(),
        executor: executor(Some("max")),
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_owned(),
        resume: resume.map(str::to_owned),
        new_native_id: None,
        env: Vec::new(),
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    }
}

/// Parses a recorded fixture and its `<name>.meta.json` sidecar.
fn parse_fixture(name: &str, now: Timestamp) -> ParsedRun {
    let path = fixture(name);
    let meta_path = path.with_extension("meta.json");
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&meta_path).unwrap_or_else(|error| panic!("read {}: {error}", meta_path.display())),
    )
    .expect("fixture metadata is valid JSON");
    let exit = meta
        .get("exit")
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    let stderr = meta
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut parser = parser_at(now);
    let mut events = Vec::new();
    for line in text.lines() {
        events.extend(parser.feed_line(line));
    }
    let (terminal, summary) = Box::new(parser).finish(exit, false, stderr);
    events.extend(terminal);
    ParsedRun { events, summary }
}

fn tool_results(events: &[HarnessEvent]) -> Vec<&ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn tool_calls(events: &[HarnessEvent]) -> Vec<&ToolCall> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn assistant_text(events: &[HarnessEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::AssistantMessage(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect()
}

fn cost_events(events: &[HarnessEvent]) -> Vec<&CostEvent> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Cost(cost) => Some(cost),
            _ => None,
        })
        .collect()
}

fn limit_events(events: &[HarnessEvent]) -> Vec<&LimitEvent> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Limit(limit) => Some(limit),
            _ => None,
        })
        .collect()
}

fn turn_endings(events: &[HarnessEvent]) -> Vec<&TurnEnded> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::TurnEnded(ended) => Some(ended),
            _ => None,
        })
        .collect()
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write fake script");
    let mut permissions = fs::metadata(path).expect("stat fake script").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod fake script");
}

#[test]
fn opencode_probe_parses_version() {
    let temp = TempDir::new().expect("temp dir");
    let binary = temp.path().join("opencode-version");
    write_script(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.0.28'; fi\n",
    );

    let adapter = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(binary.to_string_lossy().into_owned()),
        ..OpencodeConfig::default()
    });
    let probe = adapter.probe();
    assert!(probe.ok, "probe should succeed: {}", probe.detail);
    assert_eq!(probe.version.as_deref(), Some("2.0.28"));
    assert_eq!(probe.path, Some(binary.to_string_lossy().into_owned()));

    let missing = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(temp.path().join("missing-opencode").to_string_lossy().into_owned()),
        ..OpencodeConfig::default()
    });
    let probe = missing.probe();
    assert!(!probe.ok);
    assert_eq!(probe.version, None);
    assert_eq!(probe.path, None);
    assert_eq!(probe.detail, "binary not found on PATH");
}

#[test]
fn opencode_argv_new_and_resume() {
    let temp = TempDir::new().expect("temp dir");
    let cfg = OpencodeConfig {
        title: Some("vowp worker example".to_owned()),
        extra_args: vec!["--print-logs".to_owned()],
        ..OpencodeConfig::default()
    };
    let binary = "/usr/local/bin/opencode";
    let prompt = "do the job";

    let new_input = input(temp.path(), prompt, None);
    assert_eq!(
        build_argv(&cfg, &new_input, binary, prompt),
        vec![
            binary,
            "run",
            "--format",
            "json",
            "--auto",
            "--model",
            "example/model#max",
            "--title",
            "vowp worker example",
            "--print-logs",
            prompt,
        ]
    );

    let resume_input = input(temp.path(), prompt, Some("ses_example000000000000000"));
    assert_eq!(
        build_argv(&cfg, &resume_input, binary, prompt),
        vec![
            binary,
            "run",
            "--format",
            "json",
            "--auto",
            "--model",
            "example/model#max",
            "--session",
            "ses_example000000000000000",
            "--title",
            "vowp worker example",
            "--print-logs",
            prompt,
        ]
    );
}

#[test]
fn opencode_tool_exit_code_from_nested_metadata() {
    let parsed = parse_fixture("opencode_tool_exit.jsonl", now());
    let exit_codes: Vec<Option<i32>> = tool_results(&parsed.events)
        .into_iter()
        .map(|result| result.exit_code)
        .collect();
    assert_eq!(exit_codes, vec![Some(0), Some(2)]);
    assert_eq!(parsed.summary.outcome, RunOutcome::Ok);
}

#[test]
fn opencode_variant_sigil_only_when_present() {
    let temp = TempDir::new().expect("temp dir");
    let cfg = OpencodeConfig::default();

    let plain = StartInput {
        executor: executor(None),
        ..input(temp.path(), "prompt", None)
    };
    let argv = build_argv(&cfg, &plain, "opencode", "prompt");
    let model = argv
        .iter()
        .position(|argument| argument == "--model")
        .and_then(|index| argv.get(index + 1))
        .expect("model argument present");
    assert_eq!(model, "example/model");
    assert!(!argv.iter().any(|argument| argument.contains('#')));

    let variant = input(temp.path(), "prompt", None);
    let argv = build_argv(&cfg, &variant, "opencode", "prompt");
    let model = argv
        .iter()
        .position(|argument| argument == "--model")
        .and_then(|index| argv.get(index + 1))
        .expect("model argument present");
    assert_eq!(model, "example/model#max");
}

#[test]
fn opencode_long_prompt_goes_through_file() {
    let temp = TempDir::new().expect("temp dir");
    let argv_out = temp.path().join("argv.txt");
    let stdin_out = temp.path().join("stdin.txt");
    let fixture_path = fixture("opencode_no_receipt.jsonl");
    let prompt = "é".repeat(150);

    let adapter = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(fake_opencode().to_string_lossy().into_owned()),
        max_arg_prompt_bytes: 100,
        ..OpencodeConfig::default()
    });
    let mut start = input(temp.path(), &prompt, None);
    start.env = vec![
        ("FAKE_ARGV_OUT".to_owned(), argv_out.to_string_lossy().into_owned()),
        ("FAKE_STDIN_OUT".to_owned(), stdin_out.to_string_lossy().into_owned()),
        ("FAKE_FIXTURE".to_owned(), fixture_path.to_string_lossy().into_owned()),
    ];
    let mut run = adapter.start(start).expect("start fake opencode");
    let summary = run.wait().expect("fake opencode exits");
    assert_eq!(summary.outcome, RunOutcome::Ok);

    let argv = fs::read_to_string(&argv_out).expect("fake recorded argv");
    let argv: Vec<&str> = argv.lines().collect();
    let pointer = argv.last().expect("prompt argument present");
    assert!(
        pointer.starts_with("Read the instructions in .vowp/prompts/"),
        "{pointer}"
    );
    assert!(pointer.ends_with(" and follow them exactly."), "{pointer}");
    assert!(!argv.contains(&prompt.as_str()));

    let file_name = pointer
        .strip_prefix("Read the instructions in .vowp/prompts/")
        .and_then(|rest| rest.strip_suffix(" and follow them exactly."))
        .expect("pointer names the prompt file");
    let prompt_path = temp.path().join(".vowp/prompts").join(file_name);
    assert_eq!(fs::read_to_string(prompt_path).expect("prompt file exists"), prompt);
    assert_eq!(fs::read_to_string(&stdin_out).expect("stdin was closed"), "");
}

#[test]
fn opencode_ok_fixture_events() {
    let parsed = parse_fixture("opencode_ok.jsonl", now());

    assert!(
        !parsed
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::TurnStarted(_) | EventBody::SessionExecutor(_))),
        "adapters never emit framing events"
    );
    assert_eq!(tool_calls(&parsed.events).len(), 1);
    let results = tool_results(&parsed.events);
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].output.as_deref(), Some("hello\n"));
    assert_eq!(assistant_text(&parsed.events).len(), 1);
    assert_eq!(cost_events(&parsed.events).len(), 2);
    assert!(limit_events(&parsed.events).is_empty());
    assert!(
        !parsed
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Error(_)))
    );

    let endings = turn_endings(&parsed.events);
    assert_eq!(endings.len(), 1);
    assert_eq!(endings[0].stop_reason, StopReason::EndTurn);

    assert_eq!(
        parsed.summary.native_session_id.as_deref(),
        Some("ses_example000000000000000")
    );
    assert_eq!(parsed.summary.outcome, RunOutcome::Ok);
    assert_eq!(parsed.summary.stop_reason, StopReason::EndTurn);
    assert_eq!(parsed.summary.exit_code, Some(0));
}

#[test]
fn opencode_tool_use_line_becomes_two_events_in_order() {
    let line = fs::read_to_string(fixture("opencode_tool_exit.jsonl"))
        .expect("read fixture")
        .lines()
        .nth(1)
        .expect("second fixture line")
        .to_owned();
    let mut parser = parser_at(now());
    let events = parser.feed_line(&line);

    assert_eq!(events.len(), 2, "one tool_use line is a call plus a result");
    let EventBody::ToolCall(call) = &events[0].body else {
        panic!("first event is a tool call");
    };
    let EventBody::ToolResult(result) = &events[1].body else {
        panic!("second event is a tool result");
    };
    assert_eq!(call.call_id, "call_00_ok");
    assert_eq!(result.call_id, call.call_id);
    assert_eq!(call.name, "shell");
    assert_eq!(events[0].ts, Some(Timestamp::from_unix_millis(1773171900900)));
    assert_eq!(events[1].ts, Some(Timestamp::from_unix_millis(1773171901000)));
    assert_eq!(events[0].actor, Actor::Assistant);
    assert_eq!(events[1].actor, Actor::Tool);
}

#[test]
fn opencode_tool_error_is_not_run_error() {
    let parsed = parse_fixture("opencode_tools.jsonl", now());
    let results = tool_results(&parsed.events);
    assert_eq!(results.len(), 2);

    let failed = results.iter().find(|result| !result.ok).expect("one tool failed");
    assert_eq!(failed.call_id, "call_01_search");
    assert_eq!(failed.output.as_deref(), Some("Web search cancelled"));
    assert_eq!(failed.exit_code, None);

    assert!(
        !parsed
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Error(_))),
        "a failed tool is not a run error"
    );
    assert_eq!(parsed.summary.outcome, RunOutcome::Ok);
    assert_eq!(parsed.summary.stop_reason, StopReason::EndTurn);
}

#[test]
fn opencode_cost_sums_over_steps() {
    let parsed = parse_fixture("opencode_ok.jsonl", now());
    let costs = cost_events(&parsed.events);
    assert_eq!(costs.len(), 2);
    assert!(costs.iter().all(|cost| cost.cost.source == CostSource::Receipt));

    let expected_usd = 0.001 + 0.002;
    let summary = &parsed.summary.cost;
    assert_eq!(summary.source, CostSource::Receipt);
    assert!((summary.usd.expect("receipt has a USD amount") - expected_usd).abs() < 1e-12);
    assert_eq!(summary.tokens.input, 10 + 20);
    assert_eq!(summary.tokens.output, 5 + 8);
    assert_eq!(summary.tokens.reasoning, 2);
    assert_eq!(summary.tokens.cache_read, 100 + 200);
    assert_eq!(summary.tokens.cache_write, 0);

    let event_sum: f64 = costs.iter().filter_map(|cost| cost.cost.usd).sum();
    assert!((event_sum - expected_usd).abs() < 1e-12);
}

#[test]
fn opencode_no_receipt_is_unknown_cost() {
    let parsed = parse_fixture("opencode_no_receipt.jsonl", now());
    assert!(cost_events(&parsed.events).is_empty());
    assert_eq!(parsed.summary.cost, Cost::unknown());
    assert_eq!(parsed.summary.cost.usd, None);
    assert_eq!(parsed.summary.cost.source, CostSource::Unknown);
    assert_eq!(parsed.summary.outcome, RunOutcome::Ok);
}

#[test]
fn opencode_402_fixture_is_credits() {
    let parsed = parse_fixture("opencode_402.jsonl", now());
    assert_eq!(parsed.summary.outcome, RunOutcome::Credits);
    assert_eq!(parsed.summary.exit_code, Some(1));
    assert_eq!(parsed.summary.stop_reason, StopReason::Limit);

    let limits = limit_events(&parsed.events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Credits);
    assert_eq!(limits[0].provider, "example");
    assert_eq!(limits[0].http_status, Some(402));
    assert_eq!(parsed.summary.limit.as_ref(), Some(limits[0]));
}

#[test]
fn opencode_429_fixture_is_rate() {
    let parsed = parse_fixture("opencode_429.jsonl", now());
    assert_eq!(parsed.summary.outcome, RunOutcome::Rate);
    assert_eq!(parsed.summary.stop_reason, StopReason::Limit);

    let limits = limit_events(&parsed.events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Rate);
    assert_eq!(limits[0].provider, "example");
    assert_eq!(limits[0].resets_at, Some(ts("2026-03-10T18:20:00Z")));
    assert_eq!(parsed.summary.limit.as_ref(), Some(limits[0]));
}

#[test]
fn opencode_auth_fixture() {
    let parsed = parse_fixture("opencode_auth.jsonl", now());
    assert_eq!(parsed.summary.outcome, RunOutcome::Auth);
    assert_eq!(parsed.summary.stop_reason, StopReason::Limit);

    let limits = limit_events(&parsed.events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Auth);
    assert_eq!(limits[0].resets_at, None);
    assert_eq!(limits[0].http_status, Some(401));
}

#[test]
fn opencode_context_fixture() {
    let parsed = parse_fixture("opencode_context.jsonl", now());
    assert_eq!(parsed.summary.outcome, RunOutcome::Context);
    assert_eq!(parsed.summary.stop_reason, StopReason::Limit);

    let limits = limit_events(&parsed.events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Context);
    assert!(limits[0].message.contains("maximum context length"));

    let errors: Vec<&vowp_protocol::ErrorEvent> = parsed
        .events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Error(error) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind, vowp_protocol::ErrorKind::Limit);
}

#[test]
fn opencode_error_text_in_assistant_message_is_not_a_limit() {
    let mut parser = parser_at(now());
    let mut events = Vec::new();
    events.extend(
        parser.feed_line(
            &serde_json::json!({
                "type": "text",
                "timestamp": 1_773_171_800_000_i64,
                "sessionID": "ses_textlimit0000000000000",
                "part": {
                    "id": "prt_text",
                    "type": "text",
                    "text": "You may hit your usage limit; resets 3pm. Handle it carefully."
                }
            })
            .to_string(),
        ),
    );
    events.extend(
        parser.feed_line(
            &serde_json::json!({
                "type": "step_finish",
                "timestamp": 1_773_171_801_000_i64,
                "sessionID": "ses_textlimit0000000000000",
                "part": {
                    "id": "prt_finish",
                    "type": "step-finish",
                    "reason": "stop",
                    "cost": 0.01,
                    "tokens": {"input": 10, "output": 5, "reasoning": 0, "cache": {"read": 0, "write": 0}}
                }
            })
            .to_string(),
        ),
    );
    let (terminal, summary) = Box::new(parser).finish(Some(0), false, "");
    events.extend(terminal);

    assert!(
        limit_events(&events).is_empty(),
        "assistant text is not an error source"
    );
    assert!(!events.iter().any(|event| matches!(event.body, EventBody::Error(_))));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(assistant_text(&events).len(), 1);
}

#[test]
fn opencode_silent_run_then_stop() {
    let temp = TempDir::new().expect("temp dir");
    let adapter = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(fake_opencode().to_string_lossy().into_owned()),
        ..OpencodeConfig::default()
    });
    let mut start = input(temp.path(), "hang silently", None);
    start.env = vec![("FAKE_HANG".to_owned(), "1".to_owned())];
    let mut run = adapter.start(start).expect("start hanging fake");

    // The adapter has no timeout of its own: nothing arrives and the run is still alive.
    let deadline = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < deadline {
        match run.events().try_recv() {
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
            other => panic!("silent run produced an event: {other:?}"),
        }
    }
    assert!(run.pid().is_some());

    run.stop().expect("stop the silent run");
    let summary = run.wait().expect("summary after stop");
    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);

    let events: Vec<HarnessEvent> = run.events().try_iter().collect();
    let endings = turn_endings(&events);
    assert_eq!(endings.len(), 1);
    assert_eq!(endings[0].stop_reason, StopReason::Interrupted);
}

#[test]
fn opencode_end_to_end_with_fake_script() {
    let temp = TempDir::new().expect("temp dir");
    let stdin_out = temp.path().join("stdin.txt");
    let fixture_path = fixture("opencode_ok.jsonl");
    let adapter = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(fake_opencode().to_string_lossy().into_owned()),
        title: Some("vowp worker example".to_owned()),
        ..OpencodeConfig::default()
    });
    let mut start = input(temp.path(), "run the recorded turn", None);
    start.env = vec![
        ("FAKE_FIXTURE".to_owned(), fixture_path.to_string_lossy().into_owned()),
        ("FAKE_STDIN_OUT".to_owned(), stdin_out.to_string_lossy().into_owned()),
        ("FAKE_EXIT".to_owned(), "0".to_owned()),
    ];
    let mut run = adapter.start(start).expect("start fake opencode");
    let summary = run.wait().expect("fake opencode exits");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.native_session_id.as_deref(), Some("ses_example000000000000000"));

    let events: Vec<HarnessEvent> = run.events().try_iter().collect();
    assert_eq!(tool_calls(&events).len(), 1);
    assert_eq!(cost_events(&events).len(), 2);
    assert_eq!(turn_endings(&events).len(), 1);
    assert_eq!(turn_endings(&events)[0].stop_reason, StopReason::EndTurn);
    assert_eq!(fs::read_to_string(stdin_out).expect("stdin file"), "");

    let messages = assistant_text(&events);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0], "Example turn complete.");
}
