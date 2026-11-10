//! Acceptance tests for the Claude Code adapter.
//! Spec: `docs/design/crates/vowp-adapters.md` §7 and `docs/design/adapters.md` §2.
//!
//! Every test is hermetic: fixtures under `tests/fixtures`, the fake harness script under
//! `tests/fakes`, and temp directories only. No real `claude`, network or home directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_adapters::claude_code::{ClaudeCodeAdapter, ClaudeConfig, ClaudeParser, build_argv};
use vowp_adapters::stream::StreamParser;
use vowp_protocol::{
    Adapter, CostSource, EventBody, Executor, HarnessEvent, LimitEvent, LimitKind, RunOutcome, RunSummary, SessionId,
    StartInput, StopReason, Timestamp, Tokens, ToolCall, ToolResult, TurnEnded,
};

fn executor() -> Executor {
    Executor::parse("claude-code:example-model").expect("test executor is valid")
}

fn now() -> Timestamp {
    Timestamp::parse("2026-03-10T18:10:00Z").expect("test timestamp is valid")
}

fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("test timestamp is valid")
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn fake_claude() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes/fake-claude.sh")
}

fn fixture_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture_path(name))
        .expect("fixture is readable")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Feeds fixture lines through a fresh parser and finishes it with the given process state.
fn run_lines(
    lines: &[String],
    now: Timestamp,
    tz_offset_minutes: i32,
    exit: Option<i32>,
    killed: bool,
    stderr: &str,
) -> (Vec<HarnessEvent>, RunSummary) {
    let mut parser: Box<dyn StreamParser> = Box::new(ClaudeParser::new(now, tz_offset_minutes, executor()));
    let mut events = Vec::new();
    for line in lines {
        events.extend(parser.feed_line(line));
    }
    let (terminal, summary) = parser.finish(exit, killed, stderr);
    events.extend(terminal);
    (events, summary)
}

fn run_fixture(
    name: &str,
    now: Timestamp,
    tz_offset_minutes: i32,
    exit: Option<i32>,
    stderr: &str,
) -> (Vec<HarnessEvent>, RunSummary) {
    run_lines(&fixture_lines(name), now, tz_offset_minutes, exit, false, stderr)
}

fn limits(events: &[HarnessEvent]) -> Vec<&LimitEvent> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Limit(limit) => Some(limit),
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

fn assistant_texts(events: &[HarnessEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::AssistantMessage(message) => Some(message.text.clone()),
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

fn tool_results(events: &[HarnessEvent]) -> Vec<&ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn last_turn_ended(events: &[HarnessEvent]) -> &TurnEnded {
    match &events.last().expect("run emits a terminal event").body {
        EventBody::TurnEnded(ended) => ended,
        other => panic!("last event is not turn.ended: {other:?}"),
    }
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write fake script");
    let mut permissions = std::fs::metadata(path).expect("stat fake script").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake script");
}

fn start_input(cwd: &Path, prompt: &str) -> StartInput {
    StartInput {
        session_id: SessionId::new(),
        executor: executor(),
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_owned(),
        resume: None,
        new_native_id: None,
        env: Vec::new(),
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    }
}

fn adapter_for(path: &Path) -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(ClaudeConfig {
        binary: Some(path.to_string_lossy().into_owned()),
        ..ClaudeConfig::default()
    })
}

fn read_argv(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("fake wrote argv")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn claude_probe_parses_version() {
    let tmp = TempDir::new().expect("temp dir");
    let script = tmp.path().join("fake-claude-version.sh");
    write_script(&script, "#!/bin/sh\nprintf '%s\\n' '2.1.319 (Claude Code)'\n");

    let probe = adapter_for(&script).probe();
    assert!(probe.ok);
    assert_eq!(probe.version.as_deref(), Some("2.1.319"));
    assert_eq!(probe.path.as_deref(), Some(script.to_string_lossy().as_ref()));

    let missing = ClaudeCodeAdapter::new(ClaudeConfig {
        binary: Some(tmp.path().join("missing-claude").to_string_lossy().into_owned()),
        ..ClaudeConfig::default()
    })
    .probe();
    assert!(!missing.ok);
    assert_eq!(missing.version, None);
    assert_eq!(missing.path, None);
}

#[test]
fn claude_argv_new_session() {
    let tmp = TempDir::new().expect("temp dir");
    let argv_out = tmp.path().join("argv.txt");
    let mut input = start_input(tmp.path(), "the prompt stays on stdin");
    input.new_native_id = Some("11111111-1111-4111-8111-111111111111".to_owned());
    input.env = vec![("FAKE_ARGV_OUT".to_owned(), argv_out.to_string_lossy().into_owned())];

    let mut run = adapter_for(&fake_claude()).start(input).expect("fake adapter starts");
    run.wait().expect("fake adapter finishes");

    let expected = vec![
        "-p".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--model".to_owned(),
        "example-model".to_owned(),
        "--permission-mode".to_owned(),
        "auto".to_owned(),
        "--session-id".to_owned(),
        "11111111-1111-4111-8111-111111111111".to_owned(),
    ];
    assert_eq!(read_argv(&argv_out), expected);
    assert!(!read_argv(&argv_out).iter().any(|arg| arg == "--resume"));
    assert!(!read_argv(&argv_out).iter().any(|arg| arg.contains("the prompt stays")));
}

#[test]
fn claude_argv_resume_effort_system_note() {
    let config = ClaudeConfig {
        extra_args: vec!["--extra-one".to_owned(), "value".to_owned()],
        add_dirs: vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")],
        ..ClaudeConfig::default()
    };
    let mut input = start_input(Path::new("/home/user/project"), "unused prompt");
    input.resume = Some("native-session-123".to_owned());
    input.system_note = Some("system note".to_owned());
    input.executor = Executor::parse("claude-code:example-model@medium").expect("executor");

    let argv = build_argv(&config, &input, "claude");

    assert_eq!(
        argv,
        vec![
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "example-model",
            "--effort",
            "medium",
            "--permission-mode",
            "auto",
            "--append-system-prompt",
            "system note",
            "--add-dir",
            "/tmp/one",
            "--add-dir",
            "/tmp/two",
            "--resume",
            "native-session-123",
            "--extra-one",
            "value",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    );
}

#[test]
fn claude_prompt_goes_to_stdin() {
    let tmp = TempDir::new().expect("temp dir");
    let stdin_out = tmp.path().join("stdin.txt");
    let prompt = format!("begin é 漢字\n{}\nend", "p".repeat(210_000));
    let mut input = start_input(tmp.path(), &prompt);
    input.env = vec![("FAKE_STDIN_OUT".to_owned(), stdin_out.to_string_lossy().into_owned())];

    let mut run = adapter_for(&fake_claude()).start(input).expect("fake adapter starts");
    run.wait().expect("fake adapter finishes");

    let received = std::fs::read_to_string(&stdin_out).expect("fake copied stdin");
    assert_eq!(received, prompt);
    assert!(received.len() > 200 * 1024);
}

#[test]
fn claude_prompt_via_arg_fallback() {
    let tmp = TempDir::new().expect("temp dir");
    let argv_out = tmp.path().join("argv.txt");
    let prompt = "prompt on argv é";
    let mut input = start_input(tmp.path(), prompt);
    input.env = vec![("FAKE_ARGV_OUT".to_owned(), argv_out.to_string_lossy().into_owned())];
    let adapter = ClaudeCodeAdapter::new(ClaudeConfig {
        binary: Some(fake_claude().to_string_lossy().into_owned()),
        prompt_via_arg: true,
        ..ClaudeConfig::default()
    });

    let mut run = adapter.start(input).expect("fake adapter starts");
    run.wait().expect("fake adapter finishes");

    assert_eq!(read_argv(&argv_out).last().map(String::as_str), Some(prompt));
}

#[test]
fn claude_ok_fixture_events() {
    let (events, summary) = run_fixture("claude_ok.jsonl", now(), 0, Some(0), "");

    assert_eq!(assistant_texts(&events), vec!["ok".to_owned()]);
    assert_eq!(events.len(), 3, "only assistant, cost and turn.ended: {events:?}");
    let cost = match &events[1].body {
        EventBody::Cost(cost) => &cost.cost,
        other => panic!("second event is not cost: {other:?}"),
    };
    assert_eq!(cost.usd, Some(0.02));
    assert_eq!(
        cost.tokens,
        Tokens {
            input: 2,
            output: 4,
            reasoning: 0,
            cache_read: 6,
            cache_write: 4,
        }
    );
    assert_eq!(cost.source, CostSource::Receipt);
    assert!(limits(&events).is_empty(), "allowed rate event emits no limit");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.body, EventBody::SessionExecutor(_) | EventBody::TurnStarted(_)))
    );
    assert_eq!(last_turn_ended(&events).stop_reason, StopReason::EndTurn);
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(
        summary.native_session_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
}

#[test]
fn claude_tool_use_fixture_events() {
    let (events, summary) = run_fixture("claude_tool_use.jsonl", now(), 0, Some(0), "");

    let calls = tool_calls(&events);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id, "toolu_01");
    assert_eq!(calls[0].name, "Read");
    assert_eq!(calls[0].input["file_path"], "/home/user/project/src/lib.rs");
    assert_eq!(calls[1].call_id, "toolu_02");
    assert_eq!(calls[1].name, "Bash");

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].call_id, "toolu_01");
    assert!(results[0].ok);
    assert_eq!(results[0].output.as_deref(), Some("line one\nline two"));
    assert_eq!(results[1].call_id, "toolu_02");
    assert!(!results[1].ok);
    assert_eq!(results[1].output.as_deref(), Some("first block\nsecond block"));

    assert!(
        !events
            .iter()
            .any(|event| matches!(event.body, EventBody::AssistantReasoning(_))),
        "empty thinking produces no reasoning event"
    );
    assert_eq!(assistant_texts(&events), vec!["done".to_owned()]);
    assert_eq!(summary.outcome, RunOutcome::Ok);

    for event in &events {
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("sig-opaque-never-store"));
        assert!(!rendered.contains("signature"));
    }
    assert!(
        events
            .iter()
            .filter(|event| matches!(event.body, EventBody::ToolCall(_) | EventBody::ToolResult(_)))
            .all(|event| event.raw.is_some())
    );
}

#[test]
fn claude_rate_limit_rejected_fixture() {
    let (events, summary) = run_fixture("claude_rate_limit_rejected.jsonl", now(), 0, Some(0), "");

    let limit = *limits(&events).first().expect("rejected rate event emits a limit");
    assert_eq!(limit.kind, LimitKind::Quota);
    assert_eq!(limit.provider, "claude");
    assert_eq!(limit.resets_at, Some(ts("2026-03-10T19:00:00Z")));
    assert!(limit.message.contains("rate_limit_event rejected"));

    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(
        summary.limit.as_ref().map(|event| event.resets_at),
        Some(Some(ts("2026-03-10T19:00:00Z")))
    );
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(summary.exit_code, Some(0));
}

#[test]
fn claude_usage_limit_text_fixture() {
    let (events, summary) = run_fixture("claude_usage_limit_text.jsonl", now(), 0, Some(1), "");

    let limit = *limits(&events).first().expect("usage text emits a limit");
    assert_eq!(limit.kind, LimitKind::Quota);
    assert_eq!(limit.resets_at, Some(ts("2026-03-10T19:00:00Z")));
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(
        summary.limit.as_ref().map(|event| event.resets_at),
        Some(Some(ts("2026-03-10T19:00:00Z")))
    );
}

#[test]
fn claude_weekly_limit_fixture() {
    let (events, summary) = run_fixture("claude_weekly_limit.jsonl", now(), 60, Some(1), "");

    let limit = *limits(&events).first().expect("weekly text emits a limit");
    assert_eq!(limit.kind, LimitKind::Quota);
    assert_eq!(limit.resets_at, Some(ts("2026-03-15T08:00:00Z")));
    assert_eq!(summary.outcome, RunOutcome::Limit);
}

#[test]
fn claude_429_fixture() {
    let (events, summary) = run_fixture("claude_429.jsonl", now(), 0, Some(1), "");
    let expected = now().plus(Duration::from_secs(600));

    let limit = *limits(&events).first().expect("429 emits a rate limit");
    assert_eq!(limit.kind, LimitKind::Rate);
    assert_eq!(limit.resets_at, Some(expected));
    assert_eq!(summary.outcome, RunOutcome::Rate);
    assert_eq!(
        summary.limit.as_ref().map(|event| event.resets_at),
        Some(Some(expected))
    );
    assert_eq!(summary.stop_reason, StopReason::Limit);
}

#[test]
fn claude_auth_fixture() {
    let (events, summary) = run_fixture("claude_auth.jsonl", now(), 0, Some(1), "");

    let limit = *limits(&events).first().expect("401 emits an auth limit");
    assert_eq!(limit.kind, LimitKind::Auth);
    assert_eq!(limit.resets_at, None);
    assert_eq!(summary.outcome, RunOutcome::Auth);
    assert_eq!(summary.limit.as_ref().map(|event| event.kind), Some(LimitKind::Auth));
    assert_eq!(
        errors(&events).first().map(|error| error.kind),
        Some(vowp_protocol::ErrorKind::Auth)
    );
}

#[test]
fn claude_limit_words_in_assistant_text_are_not_a_limit() {
    let lines = vec![
        serde_json::to_string(&json!({
            "type": "system",
            "subtype": "init",
            "session_id": "77777777-7777-4777-8777-777777777777",
            "model": "example-model"
        }))
        .expect("json line"),
        serde_json::to_string(&json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "you may hit your usage limit"}]},
            "session_id": "77777777-7777-4777-8777-777777777777"
        }))
        .expect("json line"),
        serde_json::to_string(&json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "all done",
            "total_cost_usd": 0.01,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .expect("json line"),
    ];

    let (events, summary) = run_lines(&lines, now(), 0, Some(0), false, "");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert!(limits(&events).is_empty());
    assert!(errors(&events).is_empty());
}

#[test]
fn claude_no_result_line_is_crash_or_error() {
    let lines = fixture_lines("claude_ok.jsonl");
    let truncated = &lines[..lines.len() - 1];

    let (events, summary) = run_lines(truncated, now(), 0, Some(137), false, "");
    assert_eq!(summary.outcome, RunOutcome::Crash);
    assert_eq!(summary.stop_reason, StopReason::Crash);
    assert_eq!(last_turn_ended(&events).stop_reason, StopReason::Crash);

    let (events, summary) = run_lines(truncated, now(), 0, Some(0), false, "");
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Crash);
    assert_eq!(summary.result_text.as_deref(), Some("no result line"));
    assert!(
        errors(&events)
            .first()
            .is_some_and(|error| error.message.contains("no result line"))
    );
}

#[test]
fn claude_subtype_error_max_turns() {
    let lines = vec![
        serde_json::to_string(&json!({
            "type": "system",
            "subtype": "init",
            "session_id": "88888888-8888-4888-8888-888888888888",
            "model": "example-model"
        }))
        .expect("json line"),
        serde_json::to_string(&json!({
            "type": "result",
            "subtype": "error_max_turns",
            "is_error": true,
            "result": "maximum turns reached",
            "duration_ms": 2500,
            "total_cost_usd": 0.05,
            "usage": {"input_tokens": 5, "output_tokens": 6}
        }))
        .expect("json line"),
    ];

    let (events, summary) = run_lines(&lines, now(), 0, Some(1), false, "");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::MaxTurns);
    let ended = last_turn_ended(&events);
    assert_eq!(ended.stop_reason, StopReason::MaxTurns);
    assert_eq!(ended.duration_ms, Some(2500));
    assert!(limits(&events).is_empty());
    assert!(errors(&events).is_empty());
}

#[test]
fn claude_unknown_message_types_ignored_but_teed() {
    let tmp = TempDir::new().expect("temp dir");
    let tee = tmp.path().join("events.jsonl");
    let fixture = fixture_path("claude_unknown_type.jsonl");
    let mut input = start_input(tmp.path(), "prompt");
    input.event_log_hint = Some(tee.clone());
    input.env = vec![
        ("FAKE_FIXTURE".to_owned(), fixture.to_string_lossy().into_owned()),
        ("FAKE_EXIT".to_owned(), "0".to_owned()),
    ];

    let mut run = adapter_for(&fake_claude()).start(input).expect("fake adapter starts");
    let summary = run.wait().expect("fake adapter finishes");
    let events: Vec<HarnessEvent> = run.events().try_iter().collect();

    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert!(
        !events.iter().any(|event| matches!(event.body, EventBody::Note(_))),
        "unknown type must not produce an event: {events:?}"
    );
    assert!(!events.iter().any(|event| match &event.body {
        EventBody::Note(note) => note.text.contains("mystery_event"),
        _ => false,
    }));
    let teed = std::fs::read_to_string(&tee).expect("tee file exists");
    assert_eq!(teed, std::fs::read_to_string(&fixture).expect("fixture readable"));
    assert!(teed.contains("\"type\":\"mystery_event\""));
}

#[test]
fn claude_run_end_to_end_with_fake_script() {
    let tmp = TempDir::new().expect("temp dir");
    let fixture = fixture_path("claude_ok.jsonl");
    let mut input = start_input(tmp.path(), "prompt through stdin");
    input.env = vec![
        ("FAKE_FIXTURE".to_owned(), fixture.to_string_lossy().into_owned()),
        ("FAKE_EXIT".to_owned(), "0".to_owned()),
    ];

    let mut run = adapter_for(&fake_claude()).start(input).expect("fake adapter starts");
    let pid = run.pid();
    assert!(pid.is_some());
    let summary = run.wait().expect("fake adapter finishes");
    let events: Vec<HarnessEvent> = run.events().try_iter().collect();

    assert_eq!(assistant_texts(&events), vec!["ok".to_owned()]);
    assert_eq!(last_turn_ended(&events).stop_reason, StopReason::EndTurn);
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(
        summary.native_session_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
}

#[test]
fn claude_stop_kills_hung_run() {
    let tmp = TempDir::new().expect("temp dir");
    let mut input = start_input(tmp.path(), "prompt");
    input.env = vec![("FAKE_HANG".to_owned(), "1".to_owned())];

    let mut run = adapter_for(&fake_claude()).start(input).expect("fake adapter starts");
    run.stop().expect("stop is ok");
    let summary = run.wait().expect("stop leaves a summary");
    let events: Vec<HarnessEvent> = run.events().try_iter().collect();

    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);
    let ended = last_turn_ended(&events);
    assert_eq!(ended.stop_reason, StopReason::Interrupted);
    assert!(errors(&events).is_empty());
}

#[test]
fn claude_cost_thinking_tokens_split() {
    let line = serde_json::to_string(&json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "done",
        "total_cost_usd": 0.5,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 100,
            "output_tokens_details": {"thinking_tokens": 40},
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 7
        }
    }))
    .expect("json line");

    let (events, summary) = run_lines(&[line], now(), 0, Some(0), false, "");
    let cost = match &events[0].body {
        EventBody::Cost(event) => &event.cost,
        other => panic!("first event is not cost: {other:?}"),
    };

    assert_eq!(cost.tokens.output, 60);
    assert_eq!(cost.tokens.reasoning, 40);
    assert_eq!(cost.tokens.total(), 10 + 100 + 5 + 7);
    assert_eq!(summary.cost.tokens, cost.tokens);
    assert_eq!(summary.cost.usd, Some(0.5));
}

#[test]
fn claude_quota_without_reset_uses_hour_fallback() {
    let line = serde_json::to_string(&json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "result": "You've hit your limit",
        "total_cost_usd": 0.0,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .expect("json line");

    let (_, summary) = run_lines(&[line], now(), 0, Some(1), false, "");
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(
        summary.limit.as_ref().map(|limit| limit.resets_at),
        Some(Some(now().plus(Duration::from_secs(3600))))
    );
}

#[test]
fn claude_noise_line_is_classified() {
    let lines = vec![
        "Claude AI usage limit reached|1773169200".to_owned(),
        serde_json::to_string(&json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "result": "",
            "total_cost_usd": 0.0,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .expect("json line"),
    ];

    let (events, summary) = run_lines(&lines, now(), 0, Some(1), false, "");
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(
        summary.limit.as_ref().map(|limit| limit.resets_at),
        Some(Some(ts("2026-03-10T19:00:00Z")))
    );
    assert_eq!(limits(&events).first().map(|limit| limit.kind), Some(LimitKind::Quota));
}

#[test]
fn claude_raw_cap_and_redaction() {
    let secret = format!("sk-{}", "a".repeat(30));
    let content = format!("{secret}\n{}", "x".repeat(40_000));
    let line = serde_json::to_string(&json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_secret",
                "content": content,
                "is_error": false
            }]
        },
        "session_id": "99999999-9999-4999-8999-999999999999",
        "timestamp": "2026-03-10T20:00:00.000Z"
    }))
    .expect("json line");

    let mut parser: Box<dyn StreamParser> = Box::new(ClaudeParser::new(now(), 0, executor()));
    let events = parser.feed_line(&line);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(matches!(event.body, EventBody::ToolResult(_)));
    assert_eq!(event.ts, Some(ts("2026-03-10T20:00:00Z")));

    let raw = event.raw.as_ref().expect("tool result keeps raw");
    assert_eq!(raw["truncated"], Value::Bool(true));
    assert!(raw["bytes"].as_u64().expect("byte count") > 40_000);
    let head = raw["head"].as_str().expect("head text");
    assert!(head.len() <= 2 * 1024);
    assert!(head.contains("<redacted>"));
    let serialised = serde_json::to_string(raw).expect("raw serialises");
    assert!(!serialised.contains("sk-"));
    assert!(!serialised.contains(&secret));
}
