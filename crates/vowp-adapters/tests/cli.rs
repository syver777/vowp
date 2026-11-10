//! Tests for the generic CLI adapter and its mapping file.
//! Spec: `docs/design/crates/vowp-adapters.md` §7 and `docs/design/adapters.md` §4.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use vowp_adapters::cli::{CliAdapter, CliParser, Mapping, OutputFormat, pointer_get};
use vowp_adapters::stream::StreamParser;
use vowp_protocol::{
    Adapter, AdapterError, CostSource, ErrorEvent, ErrorKind, EventBody, Executor, HarnessEvent, LimitEvent, LimitKind,
    RunOutcome, RunSummary, SessionId, StartInput, StopReason, Timestamp,
};

fn now() -> Timestamp {
    Timestamp::parse("2026-03-10T18:10:00Z").expect("invariant: test timestamp literal is valid")
}

fn mapping(text: &str) -> Mapping {
    Mapping::from_toml(text).expect("mapping should be valid")
}

fn parser(mapping: Mapping) -> CliParser {
    CliParser::new(mapping, now()).expect("parser should compile the mapping")
}

fn finish_run(parser: CliParser, exit: Option<i32>, stderr: &str) -> (Vec<HarnessEvent>, RunSummary) {
    Box::new(parser).finish(exit, false, stderr)
}

fn feed_all(parser: &mut CliParser, text: &str) -> Vec<HarnessEvent> {
    let mut events = Vec::new();
    for line in text.lines() {
        events.extend(parser.feed_line(line));
    }
    events
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn fake_cli() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes/fake-cli.sh")
}

fn notes(events: &[HarnessEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Note(note) => Some(note.text.clone()),
            _ => None,
        })
        .collect()
}

fn limit_events(events: &[HarnessEvent]) -> Vec<LimitEvent> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Limit(limit) => Some(limit.clone()),
            _ => None,
        })
        .collect()
}

fn turn_ended(events: &[HarnessEvent]) -> &StopReason {
    match events.last().map(|event| &event.body) {
        Some(EventBody::TurnEnded(ended)) => &ended.stop_reason,
        other => panic!("last event is not turn.ended: {other:?}"),
    }
}

fn start_input(cwd: &Path, prompt: &str, resume: Option<&str>, env: &[(&str, String)]) -> StartInput {
    StartInput {
        session_id: SessionId::new(),
        executor: Executor::parse("cli:demo").expect("invariant: executor literal is valid"),
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_owned(),
        resume: resume.map(str::to_owned),
        new_native_id: None,
        env: env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    }
}

fn argv_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("fake harness writes its argv")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_rejected(toml: &str, needle: &str) {
    let error = Mapping::from_toml(toml).expect_err("mapping must be rejected");
    assert!(
        matches!(error, AdapterError::Mapping { .. }),
        "expected a mapping error, got {error:?}"
    );
    assert!(
        error.to_string().contains(needle),
        "error {error} does not name {needle:?}"
    );
}

#[test]
fn mapping_valid_examples_load() {
    let codex = mapping(include_str!("fixtures/cli_codex_like.toml"));
    assert_eq!(codex.name, "codex");
    assert_eq!(codex.provider.as_deref(), Some("openai"));
    assert_eq!(codex.command.argv, ["codex", "exec", "--json"]);
    assert_eq!(codex.command.prompt_via, vowp_adapters::cli::PromptVia::Arg);
    assert_eq!(
        codex.command.resume_argv,
        Some(vec![
            "codex".to_owned(),
            "exec".to_owned(),
            "resume".to_owned(),
            "{native_id}".to_owned(),
            "--json".to_owned()
        ])
    );
    assert_eq!(codex.output.format, OutputFormat::Jsonl);
    assert_eq!(codex.output.native_session_pointer.as_deref(), Some("/session_id"));
    assert_eq!(codex.rules.len(), 3);
    assert_eq!(codex.rules[0].emit, "assistant.message");
    assert_eq!(codex.limits.credits, ["insufficient_quota", "billing"]);

    let gemini = mapping(include_str!("fixtures/cli_gemini_like.toml"));
    assert_eq!(gemini.name, "gemini");
    assert_eq!(gemini.provider.as_deref(), Some("google"));
    assert_eq!(gemini.output.format, OutputFormat::Text);
    assert_eq!(gemini.output.text_chunk_lines, 40);
    assert!(gemini.rules.is_empty());
    assert_eq!(gemini.exit.ok, [0]);
}

#[test]
fn mapping_rejects_each_invalid_rule() {
    let cases = [
        (
            "rule 1 name",
            "name = \"Codex\"\n[command]\nargv = [\"demo\"]\n",
            "Codex",
        ),
        (
            "rule 1 empty argv",
            "name = \"demo\"\n[command]\nargv = []\n",
            "command.argv",
        ),
        (
            "rule 2 resume placeholder",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\nresume_argv = [\"demo\", \"resume\", \"--json\"]\n",
            "{native_id}",
        ),
        (
            "rule 2 invalid prompt_via",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\nprompt_via = \"flag:\"\n",
            "flag:",
        ),
        (
            "rule 3 unknown emit",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[[map]]\nmatch = { pointer = \"/type\", equals = \"x\" }\n\
             emit = \"bogus.event\"\nfields = { text = \"/text\" }\n",
            "bogus.event",
        ),
        (
            "rule 3 match without a condition",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[[map]]\nmatch = { pointer = \"/type\" }\n\
             emit = \"note\"\nfields = { text = \"/text\" }\n",
            "exactly one",
        ),
        (
            "rule 3 missing required field",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[[map]]\nmatch = { pointer = \"/type\", equals = \"x\" }\n\
             emit = \"assistant.message\"\nfields = {}\n",
            "text",
        ),
        (
            "rule 3 bad pointer",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[output]\nnative_session_pointer = \"session_id\"\n",
            "session_id",
        ),
        (
            "rule 4 bad regex",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nrate = [\"(\"]\n",
            "(",
        ),
        (
            "rule 4 too many patterns",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nquota = [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \
             \"g\", \"h\", \"i\", \"j\", \"k\", \"l\", \"m\", \"n\", \"o\", \"p\", \"q\", \"r\", \"s\", \"t\", \"u\", \
             \"v\", \"w\", \"x\", \"y\", \"z\", \"aa\", \"ab\", \"ac\", \"ad\", \"ae\", \"af\", \"ag\"]\n",
            "max 32",
        ),
        (
            "rule 5 bad exit code",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[exit]\nok = [300]\n",
            "300",
        ),
        (
            "rule 5 bad env name",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\nenv = [\"openai_api_key\"]\n",
            "openai_api_key",
        ),
        (
            "rule 6 unknown key",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\nshell = true\n",
            "shell",
        ),
    ];

    for (label, toml, needle) in cases {
        assert_rejected(toml, needle);
        println!("rejected {label} as expected");
    }
}

#[test]
fn mapping_rejects_unknown_keys_and_secret_looking_env_values() {
    assert_rejected("name = \"demo\"\nbogus = 1\n[command]\nargv = [\"demo\"]\n", "bogus");
    assert_rejected(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[output]\nbogus = true\n",
        "bogus",
    );
    // Build the synthetic token from fragments so this file contains no literal
    // secret-shaped value for the repository's secret scanner.
    let prefix = format!("{}{}", "s", "k-");
    let secret = format!("{prefix}{}", "abcdefghijklmnopqrstuvwx");
    assert_rejected(
        &format!("name = \"demo\"\n[command]\nargv = [\"demo\"]\nenv = [\"OPENAI_API_KEY\", \"{secret}\"]\n"),
        "secret",
    );
    let error = Mapping::from_toml(&format!(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\nenv = [\"OPENAI_API_KEY={secret}\"]\n"
    ))
    .expect_err("a secret-shaped env value must be rejected");
    assert!(
        matches!(error, AdapterError::Mapping { .. }),
        "expected a mapping error, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("command.env[0]"),
        "error should name the offending env entry: {message}"
    );
    assert!(
        !message.contains(&secret) && !message.contains(&prefix),
        "error must not echo the secret value: {message}"
    );

    let cleaned = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\nenv = [\"OPENAI_API_KEY\", \"_X1\", \"A_B2\"]\n\
         model_arg = \"\"\ncwd_flag = \"\"\n[output]\nnative_session_pointer = \"\"\n[cost]\nusd_pointer = \"\"\n",
    );
    assert_eq!(cleaned.command.env, ["OPENAI_API_KEY", "_X1", "A_B2"]);
    assert_eq!(cleaned.command.model_arg, None);
    assert_eq!(cleaned.command.cwd_flag, None);
    assert_eq!(cleaned.output.native_session_pointer, None);
    assert_eq!(cleaned.cost.usd_pointer, None);
}

#[test]
fn mapping_accepts_dotted_keys_and_hex_ints() {
    let map = mapping(
        "name = \"demo\"\n\
         command.argv = [\"demo\", \"--json\"]\n\
         command.prompt_via = \"stdin\"\n\
         output.format = \"jsonl\"\n\
         output.text_chunk_lines = 0x14\n\
         limits.tz_offset_minutes = 0x3c\n\
         [[map]]\n\
         match.pointer = \"/type\"\n\
         match.equals = \"note\"\n\
         emit = \"note\"\n\
         fields.text = \"/text\"\n\
         [exit]\n\
         ok = [0x0]\n",
    );

    assert_eq!(map.command.argv, ["demo", "--json"]);
    assert_eq!(map.command.prompt_via, vowp_adapters::cli::PromptVia::Stdin);
    assert_eq!(map.output.format, OutputFormat::Jsonl);
    assert_eq!(map.output.text_chunk_lines, 20);
    assert_eq!(map.limits.tz_offset_minutes, 60);
    assert_eq!(map.rules.len(), 1);
    assert_eq!(map.rules[0].match_.pointer, "/type");
    assert_eq!(map.rules[0].match_.equals.as_deref(), Some("note"));
    assert_eq!(map.rules[0].fields.get("text").map(String::as_str), Some("/text"));
    assert_eq!(map.exit.ok, [0]);
}

#[test]
fn mapping_rejects_unknown_fields() {
    let cases = [
        (
            "root",
            "name = \"demo\"\nbogus = 1\n[command]\nargv = [\"demo\"]\n",
            "bogus",
        ),
        (
            "command",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\nshell = true\n",
            "shell",
        ),
        (
            "limits",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nbogus = []\n",
            "bogus",
        ),
        (
            "cost",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[cost]\nbogus = \"/x\"\n",
            "bogus",
        ),
        (
            "exit",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[exit]\nbogus = [0]\n",
            "bogus",
        ),
        (
            "map rule",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[[map]]\n\
             match = { pointer = \"/type\", equals = \"x\" }\nemit = \"note\"\n\
             fields = { text = \"/text\" }\nbogus = 1\n",
            "bogus",
        ),
        (
            "match",
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[[map]]\n\
             match = { pointer = \"/type\", equals = \"x\", bogus = true }\nemit = \"note\"\n\
             fields = { text = \"/text\" }\n",
            "bogus",
        ),
    ];

    for (label, toml, needle) in cases {
        assert_rejected(toml, needle);
        println!("rejected unknown {label} field as expected");
    }
}

#[test]
fn cli_jsonl_codex_like_events() {
    let base = mapping(include_str!("fixtures/cli_codex_like.toml"));
    let text = std::fs::read_to_string(fixture("cli_codex_like.jsonl")).expect("read fixture");

    let mut without_usd = parser(base.clone());
    let mut events = feed_all(&mut without_usd, &text);
    let (terminal, summary) = finish_run(without_usd, Some(0), "");
    events.extend(terminal);
    assert_eq!(summary.cost.source, CostSource::Unknown);
    assert!(
        events.iter().all(|event| !matches!(event.body, EventBody::Cost(_))),
        "no usd pointer means no cost receipt event"
    );

    let mut with_usd = base;
    with_usd.cost.usd_pointer = Some("/usage/cost_usd".to_owned());
    let mut parser = parser(with_usd);
    let mut events = feed_all(&mut parser, &text);
    let (terminal, summary) = finish_run(parser, Some(0), "");
    events.extend(terminal);

    let messages = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::AssistantMessage(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, ["hello from codex"]);

    let calls = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].input, serde_json::json!("echo hi"));

    let results = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call_id, "call_1");
    assert!(results[0].ok);
    assert_eq!(results[0].output.as_deref(), Some("hi"));

    let costs = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Cost(cost) => Some(cost.cost.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].source, CostSource::Receipt);
    assert!((costs[0].usd.expect("receipt has usd") - 0.0012).abs() < 1e-12);
    assert_eq!(costs[0].tokens.input, 12);
    assert_eq!(costs[0].tokens.output, 5);

    assert_eq!(summary.native_session_id.as_deref(), Some("thr_abc123"));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(summary.cost.source, CostSource::Receipt);
    assert_eq!(turn_ended(&events), &StopReason::EndTurn);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.body, EventBody::SessionExecutor(_) | EventBody::TurnStarted(_)))
    );
}

#[test]
fn cli_missing_required_field_skips_rule_with_note() {
    let mapping = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"call\" }\nemit = \"tool.call\"\n\
         fields = { call_id = \"/missing\", name = \"/name\", input = \"/input\" }\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"call\" }\nemit = \"note\"\n\
         fields = { text = \"/text\" }\n",
    );
    let mut parser = parser(mapping);
    let events = parser.feed_line(r#"{"type":"call","name":"shell","input":{"cmd":"ls"},"text":"still fires"}"#);

    assert_eq!(
        notes(&events),
        ["mapping: rule 1 missing call_id".to_owned(), "still fires".to_owned()]
    );
    assert!(events.iter().all(|event| !matches!(event.body, EventBody::ToolCall(_))));
}

#[test]
fn cli_error_emit_defaults_kind_and_classifies() {
    let mapping = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nquota = [\"quota exceeded\"]\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"error\" }\nemit = \"error\"\n\
         fields = { message = \"/message\" }\n",
    );
    let mut parser = parser(mapping);
    let events = parser.feed_line(r#"{"type":"error","message":"quota exceeded"}"#);
    let errors = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Error(error) => Some(error.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind, ErrorKind::AdapterFailed);
    assert_eq!(errors[0].message, "quota exceeded");

    let (terminal, summary) = finish_run(parser, Some(0), "");
    let limits = limit_events(&terminal);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Quota);
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(turn_ended(&terminal), &StopReason::Limit);
}

#[test]
fn cli_mapped_events_carry_redacted_raw() {
    let prefix = format!("{}{}", "s", "k-");
    let secret = format!("{prefix}{}", "abcdefghijklmnopqrstuvwx");
    let mapping = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"call\" }\nemit = \"tool.call\"\n\
         fields = { call_id = \"/id\", name = \"/name\", input = \"/input\" }\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"error\" }\nemit = \"error\"\n\
         fields = { message = \"/message\" }\n",
    );
    let mut parser = parser(mapping);

    let call_line = format!(r#"{{"type":"call","id":"c1","name":"shell","input":{{"command":"{secret}"}}}}"#);
    let call_events = parser.feed_line(&call_line);
    let call = call_events
        .iter()
        .find(|event| matches!(event.body, EventBody::ToolCall(_)))
        .expect("the call rule emits a tool.call");
    let raw = call.raw.as_ref().expect("tool.call carries raw");
    assert_eq!(raw["type"], "call");
    assert_eq!(raw["input"]["command"], "<redacted>");
    assert!(!raw.to_string().contains(&secret));

    let error_events = parser.feed_line(r#"{"type":"error","message":"boom"}"#);
    let error = error_events
        .iter()
        .find(|event| matches!(event.body, EventBody::Error(_)))
        .expect("the error rule emits an error");
    assert_eq!(error.raw.as_ref().expect("error carries raw")["message"], "boom");

    let (terminal, summary) = finish_run(parser, Some(1), "boom");
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert!(
        terminal
            .iter()
            .find(|event| matches!(event.body, EventBody::Error(_)))
            .expect("the terminal error carries raw")
            .raw
            .is_some()
    );
}

#[test]
fn cli_context_limit_emits_error_before_turn_ended() {
    let mapping = mapping("name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\ncontext = [\"context length\"]\n");
    let (events, summary) = finish_run(parser(mapping), Some(1), "context length exceeded");
    assert_eq!(summary.outcome, RunOutcome::Context);

    let error_index = events
        .iter()
        .position(|event| matches!(event.body, EventBody::Error(_)))
        .expect("a context limit also emits an error event");
    let limit_index = events
        .iter()
        .position(|event| matches!(event.body, EventBody::Limit(_)))
        .expect("a limit event is emitted");
    assert!(error_index < limit_index, "the error event precedes the limit event");
    let EventBody::Error(error) = &events[error_index].body else {
        panic!("event at {error_index} is not an error");
    };
    assert_eq!(error.kind, ErrorKind::Limit);
    assert_eq!(turn_ended(&events), &StopReason::Limit);
}

#[test]
fn cli_mapping_file_and_probe() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("demo.toml");
    std::fs::write(
        &path,
        "name = \"demo\"\nversion_argv = [\"echo\", \"2.3.4\"]\n[command]\nargv = [\"echo\"]\n",
    )
    .expect("write mapping file");
    let adapter = CliAdapter::from_mapping_file(&path).expect("load mapping file");
    assert_eq!(adapter.mapping().name, "demo");
    let probe = adapter.probe();
    assert!(probe.ok);
    assert_eq!(probe.version.as_deref(), Some("2.3.4"));
    assert!(probe.path.is_some());

    let missing = CliAdapter::from_mapping(mapping(
        "name = \"demo\"\nversion_argv = [\"vowp-missing-binary-xyz\", \"--version\"]\n\
         [command]\nargv = [\"demo\"]\n",
    ))
    .expect("adapter");
    let probe = missing.probe();
    assert!(!probe.ok);
    assert_eq!(probe.version, None);
    assert_eq!(probe.path, None);
    assert!(probe.detail.contains("not found"), "detail was {:?}", probe.detail);
}

#[test]
fn cli_map_regex_is_case_insensitive() {
    let mapping = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n\
         [[map]]\nmatch = { pointer = \"/type\", regex = \"^TOOL_\" }\nemit = \"note\"\n\
         fields = { text = \"/text\" }\n",
    );
    let mut parser = parser(mapping);
    let events = parser.feed_line(r#"{"type":"tool_call","text":"matched"}"#);
    assert_eq!(notes(&events), ["matched"]);
}

#[test]
fn cli_argv_model_cwd_and_env_filtering() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("temp dir");
    let script = temp.path().join("harness.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGV_OUT\"\n\
         if [ -n \"$ENV_OUT\" ]; then env > \"$ENV_OUT\"; fi\n",
    )
    .expect("write harness");
    let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).expect("chmod harness");

    let toml = format!(
        "name = \"demo\"\n[command]\nargv = [\"{}\", \"--json\"]\nprompt_via = \"stdin\"\n\
         model_flag = \"--model\"\nmodel_arg = \"static-model\"\ncwd_flag = \"--cwd\"\n\
         env = [\"ARGV_OUT\", \"ENV_OUT\"]\n",
        script.display()
    );
    let adapter = CliAdapter::from_mapping(mapping(&toml)).expect("adapter");

    let argv_out = temp.path().join("argv.txt");
    let env_out = temp.path().join("env.txt");
    let input = start_input(
        temp.path(),
        "prompt",
        None,
        &[
            ("ARGV_OUT", argv_out.to_string_lossy().into_owned()),
            ("ENV_OUT", env_out.to_string_lossy().into_owned()),
            ("CANARY_VALUE", "must-not-leak".to_owned()),
        ],
    );
    let mut run = adapter.start(input).expect("start harness");
    run.wait().expect("wait");
    assert_eq!(
        argv_lines(&argv_out),
        [
            "--json".to_owned(),
            "--model".to_owned(),
            "static-model".to_owned(),
            "--cwd".to_owned(),
            temp.path().to_string_lossy().into_owned(),
        ]
    );
    let env_text = std::fs::read_to_string(&env_out).expect("read env");
    assert!(env_text.lines().any(|line| line.starts_with("ARGV_OUT=")));
    assert!(env_text.lines().any(|line| line.starts_with("ENV_OUT=")));
    assert!(
        !env_text.lines().any(|line| line.starts_with("CANARY_VALUE=")),
        "only the mapping's env names may be passed through"
    );

    // Without a static model_arg, an executor effort becomes the model value.
    let toml = format!(
        "name = \"demo\"\n[command]\nargv = [\"{}\", \"--json\"]\nprompt_via = \"stdin\"\n\
         model_flag = \"--model\"\nenv = [\"ARGV_OUT\"]\n",
        script.display()
    );
    let adapter = CliAdapter::from_mapping(mapping(&toml)).expect("adapter");
    let argv_out = temp.path().join("effort-argv.txt");
    let mut input = start_input(
        temp.path(),
        "prompt",
        None,
        &[("ARGV_OUT", argv_out.to_string_lossy().into_owned())],
    );
    input.executor.effort = Some("max".to_owned());
    let mut run = adapter.start(input).expect("start harness");
    run.wait().expect("wait");
    assert_eq!(argv_lines(&argv_out), ["--json", "--model", "max"]);
}

#[test]
fn cli_error_from_stderr_and_warning_only_stderr() {
    fn configured(extra: &str) -> Mapping {
        mapping(&format!(
            "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nquota = [\"quota exceeded\"]\n{extra}"
        ))
    }

    let (events, summary) = finish_run(parser(configured("")), Some(0), "quota exceeded");
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(limit_events(&events).len(), 1);

    let (events, summary) = finish_run(
        parser(configured("[exit]\nerror_from_stderr = false\n")),
        Some(0),
        "quota exceeded",
    );
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert!(limit_events(&events).is_empty());

    let (events, summary) = finish_run(parser(configured("")), Some(0), "warning: cache miss");
    assert_eq!(summary.outcome, RunOutcome::Ok, "warning-only stderr is not a limit");
    assert!(limit_events(&events).is_empty());
}

#[cfg(feature = "testing")]
#[test]
fn cli_gemini_fixture_sidecar_is_applied() {
    use vowp_adapters::testing::{FixtureKind, parse_fixture};

    let parsed = parse_fixture(
        FixtureKind::Cli(mapping(include_str!("fixtures/cli_gemini_like.toml"))),
        &fixture("cli_gemini_like.txt"),
    );
    assert_eq!(parsed.summary.exit_code, Some(1));
    assert_eq!(parsed.summary.outcome, RunOutcome::Limit);
    let limits = limit_events(&parsed.events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Quota);
    assert_eq!(limits[0].provider, "google");
}

#[test]
fn cli_literal_field_values() {
    let mapping = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n\
         [[map]]\nmatch = { pointer = \"/type\", equals = \"result\" }\nemit = \"tool.result\"\n\
         fields = { call_id = \"/id\", ok = \"=true\", output = \"=shell\" }\n",
    );
    let mut parser = parser(mapping);
    let events = parser.feed_line(r#"{"type":"result","id":"call_7"}"#);

    let result = events
        .iter()
        .find_map(|event| match &event.body {
            EventBody::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("literal rule emits a tool.result");
    assert_eq!(result.call_id, "call_7");
    assert!(result.ok);
    assert_eq!(result.output.as_deref(), Some("shell"));
}

#[test]
fn cli_text_format_chunks_every_n_lines_and_at_eof() {
    let mapping = mapping(
        "name = \"gemini\"\n[command]\nargv = [\"gemini\", \"-p\"]\nprompt_via = \"arg\"\n\
         [output]\nformat = \"text\"\ntext_chunk_lines = 20\n",
    );
    let mut parser = parser(mapping);
    let text = std::fs::read_to_string(fixture("cli_gemini_like.txt")).expect("read fixture");
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 45);

    let mut events = Vec::new();
    for line in &lines {
        events.extend(parser.feed_line(line));
    }
    let after_lines = events
        .iter()
        .filter(|event| matches!(event.body, EventBody::AssistantMessage(_)))
        .count();
    assert_eq!(after_lines, 2, "two full chunks of 20 lines before EOF");

    let (terminal, summary) = finish_run(parser, Some(0), "");
    events.extend(terminal);
    let messages = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::AssistantMessage(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0], lines[..20].join("\n"));
    assert_eq!(messages[1], lines[20..40].join("\n"));
    assert_eq!(messages[2], lines[40..].join("\n"));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(turn_ended(&events), &StopReason::EndTurn);
}

#[test]
fn cli_limit_regexes_by_precedence() {
    let text = "name = \"demo\"\nprovider = \"openai\"\n[command]\nargv = [\"demo\"]\n\
                [limits]\ncredits = [\"insufficient funds\"]\nauth = [\"invalid api key\"]\n\
                quota = [\"quota exceeded\", \"usage limit\"]\nrate = [\"429\", \"rate limit\"]\n\
                context = [\"context length\"]\n\
                resets_at = { regex = \"resets? at (\\\\d{10})\", kind = \"unix\" }\n\
                [[map]]\nmatch = { pointer = \"/type\", equals = \"agent_message\" }\n\
                emit = \"assistant.message\"\nfields = { text = \"/text\" }\n";
    let map = mapping(text);

    let (events, summary) = finish_run(parser(map.clone()), Some(1), "insufficient funds and rate limit 429");
    let limits = limit_events(&events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Credits);
    assert_eq!(limits[0].provider, "openai");
    assert_eq!(summary.outcome, RunOutcome::Credits);
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(summary.limit.as_ref(), Some(&limits[0]));

    let (events, summary) = finish_run(parser(map.clone()), Some(1), "usage limit resets at 1773169200");
    let limits = limit_events(&events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Quota);
    assert_eq!(
        limits[0].resets_at,
        Some(Timestamp::parse("2026-03-10T19:00:00Z").expect("invariant: timestamp literal is valid"))
    );
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(turn_ended(&events), &StopReason::Limit);

    let (events, _) = finish_run(parser(map.clone()), Some(1), "invalid api key and quota exceeded");
    assert_eq!(limit_events(&events)[0].kind, LimitKind::Auth);

    let (events, _) = finish_run(parser(map.clone()), Some(1), "rate limit 429");
    assert_eq!(limit_events(&events)[0].kind, LimitKind::Rate);

    let mut noisy = parser(map.clone());
    assert!(noisy.feed_line("429 rate limit").is_empty());
    let (_events, summary) = finish_run(noisy, Some(0), "");
    assert_eq!(summary.outcome, RunOutcome::Rate, "non-JSON noise is an error signal");

    let mut parser = parser(map);
    let spoken = parser.feed_line(r#"{"type":"agent_message","text":"you may hit your usage limit"}"#);
    assert_eq!(
        spoken
            .iter()
            .filter(|event| matches!(event.body, EventBody::AssistantMessage(_)))
            .count(),
        1
    );
    let (events, summary) = finish_run(parser, Some(0), "");
    assert!(limit_events(&events).is_empty());
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(turn_ended(&events), &StopReason::EndTurn);
}

#[test]
fn cli_exit_codes_ok_and_limit() {
    let map = mapping(
        "name = \"demo\"\n[command]\nargv = [\"demo\"]\n[limits]\nrate = [\"429\", \"rate limit\"]\n\
         [exit]\nok = [0, 2]\nlimit = [3]\n",
    );

    for code in [0, 2] {
        let (events, summary) = finish_run(parser(map.clone()), Some(code), "");
        assert_eq!(summary.outcome, RunOutcome::Ok, "exit {code} is ok");
        assert_eq!(summary.stop_reason, StopReason::EndTurn);
        assert!(limit_events(&events).is_empty());
        assert_eq!(turn_ended(&events), &StopReason::EndTurn);
    }

    let (events, summary) = finish_run(parser(map.clone()), Some(3), "");
    let limits = limit_events(&events);
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].kind, LimitKind::Quota);
    assert_eq!(limits[0].provider, "demo");
    assert_eq!(limits[0].resets_at, None);
    assert_eq!(summary.outcome, RunOutcome::Limit);
    assert_eq!(summary.stop_reason, StopReason::Limit);
    assert_eq!(summary.limit.as_ref(), Some(&limits[0]));
    assert_eq!(turn_ended(&events), &StopReason::Limit);

    let (events, summary) = finish_run(parser(map.clone()), Some(3), "rate limit 429");
    assert_eq!(limit_events(&events)[0].kind, LimitKind::Rate);
    assert_eq!(summary.outcome, RunOutcome::Rate);

    let (events, summary) = finish_run(parser(map.clone()), Some(1), "");
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert_eq!(summary.stop_reason, StopReason::Error);
    let errors = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Error(error) => Some(error.clone()),
            _ => None,
        })
        .collect::<Vec<ErrorEvent>>();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("exit code 1"));
    assert_eq!(turn_ended(&events), &StopReason::Error);

    let (events, summary) = finish_run(parser(map), None, "");
    assert_eq!(summary.outcome, RunOutcome::Crash);
    assert_eq!(summary.stop_reason, StopReason::Crash);
    assert_eq!(turn_ended(&events), &StopReason::Crash);
}

#[test]
fn cli_resume_argv_substitution() {
    let temp = TempDir::new().expect("temp dir");
    let fake = fake_cli();
    let toml = format!(
        "name = \"demo\"\n[command]\nargv = [\"{}\", \"exec\", \"--json\"]\nprompt_via = \"stdin\"\n\
         resume_argv = [\"{}\", \"exec\", \"resume\", \"{{native_id}}\", \"--json\"]\n\
         env = [\"FAKE_ARGV_OUT\"]\n",
        fake.display(),
        fake.display()
    );
    let adapter = CliAdapter::from_mapping(mapping(&toml)).expect("adapter");
    assert!(adapter.capabilities().can_resume_native);

    let argv_out = temp.path().join("argv.txt");
    let input = start_input(
        temp.path(),
        "the prompt",
        Some("thr_abc"),
        &[("FAKE_ARGV_OUT", argv_out.to_string_lossy().into_owned())],
    );
    let mut run = adapter.start(input).expect("start fake harness");
    let summary = run.wait().expect("wait");
    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(argv_lines(&argv_out), ["exec", "resume", "thr_abc", "--json"]);

    let plain = format!(
        "name = \"demo\"\n[command]\nargv = [\"{}\", \"exec\", \"--json\"]\nprompt_via = \"stdin\"\n\
         env = [\"FAKE_ARGV_OUT\"]\n",
        fake.display()
    );
    let adapter = CliAdapter::from_mapping(mapping(&plain)).expect("adapter");
    assert!(!adapter.capabilities().can_resume_native);
    let argv_out = temp.path().join("plain-argv.txt");
    let input = start_input(
        temp.path(),
        "the prompt",
        None,
        &[("FAKE_ARGV_OUT", argv_out.to_string_lossy().into_owned())],
    );
    let mut run = adapter.start(input).expect("start fake harness");
    run.wait().expect("wait");
    assert_eq!(argv_lines(&argv_out), ["exec", "--json"]);
}

#[test]
fn cli_prompt_via_stdin_arg_flag() {
    fn run_mode(prompt_via: &str, prompt: &str) -> (Vec<String>, String) {
        let temp = TempDir::new().expect("temp dir");
        let fake = fake_cli();
        let toml = format!(
            "name = \"demo\"\n[command]\nargv = [\"{}\", \"--json\"]\nprompt_via = \"{prompt_via}\"\n\
             env = [\"FAKE_ARGV_OUT\", \"FAKE_STDIN_OUT\"]\n",
            fake.display()
        );
        let adapter = CliAdapter::from_mapping(mapping(&toml)).expect("adapter");
        let argv_out = temp.path().join("argv.txt");
        let stdin_out = temp.path().join("stdin.txt");
        let input = start_input(
            temp.path(),
            prompt,
            None,
            &[
                ("FAKE_ARGV_OUT", argv_out.to_string_lossy().into_owned()),
                ("FAKE_STDIN_OUT", stdin_out.to_string_lossy().into_owned()),
            ],
        );
        let mut run = adapter.start(input).expect("start fake harness");
        let summary = run.wait().expect("wait");
        assert_eq!(summary.outcome, RunOutcome::Ok);
        let stdin = std::fs::read_to_string(&stdin_out).unwrap_or_default();
        (argv_lines(&argv_out), stdin)
    }

    let (argv, stdin) = run_mode("stdin", "hello\nworld é");
    assert_eq!(argv, ["--json"]);
    assert_eq!(stdin, "hello\nworld é");

    let prompt = "hello world é";
    let (argv, stdin) = run_mode("arg", prompt);
    assert_eq!(argv, ["--json", prompt]);
    assert_eq!(stdin, "");

    let (argv, stdin) = run_mode("flag:--prompt", prompt);
    assert_eq!(argv, ["--json", "--prompt", prompt]);
    assert_eq!(stdin, "");
}

#[test]
fn cli_end_to_end_with_fake_script() {
    let temp = TempDir::new().expect("temp dir");
    let fake = fake_cli();
    let toml = format!(
        "name = \"codex\"\nprovider = \"openai\"\n[command]\nargv = [\"{}\", \"exec\", \"--json\"]\n\
         prompt_via = \"arg\"\nenv = [\"FAKE_FIXTURE\", \"FAKE_EXIT\"]\n\
         [output]\nformat = \"jsonl\"\nnative_session_pointer = \"/session_id\"\n\
         [[map]]\nmatch = {{ pointer = \"/type\", equals = \"agent_message\" }}\n\
         emit = \"assistant.message\"\nfields = {{ text = \"/text\" }}\n\
         [[map]]\nmatch = {{ pointer = \"/type\", equals = \"command_execution\" }}\n\
         emit = \"tool.call\"\nfields = {{ call_id = \"/id\", name = \"=shell\", input = \"/command\" }}\n\
         [[map]]\nmatch = {{ pointer = \"/type\", equals = \"command_result\" }}\n\
         emit = \"tool.result\"\nfields = {{ call_id = \"/id\", ok = \"/ok\", output = \"/output\" }}\n\
         [cost]\ninput_pointer = \"/usage/input_tokens\"\noutput_pointer = \"/usage/output_tokens\"\n\
         usd_pointer = \"/usage/cost_usd\"\n",
        fake.display()
    );
    let adapter = CliAdapter::from_mapping(mapping(&toml)).expect("adapter");
    let fixture_path = fixture("cli_codex_like.jsonl");
    let input = start_input(
        temp.path(),
        "say hi",
        None,
        &[
            ("FAKE_FIXTURE", fixture_path.to_string_lossy().into_owned()),
            ("FAKE_EXIT", "0".to_owned()),
        ],
    );

    let mut run = adapter.start(input).expect("start fake harness");
    assert!(run.pid().is_some());
    let summary = run.wait().expect("wait for fake harness");
    let events = run.events().try_iter().collect::<Vec<_>>();

    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.native_session_id.as_deref(), Some("thr_abc123"));
    assert_eq!(summary.cost.source, CostSource::Receipt);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.body, EventBody::TurnEnded(_)))
            .count(),
        1
    );
    assert_eq!(turn_ended(&events), &StopReason::EndTurn);
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::AssistantMessage(message) if message.text == "hello from codex"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::ToolCall(call) if call.call_id == "call_1" && call.name == "shell"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::ToolResult(result) if result.call_id == "call_1" && result.ok
    )));
}

#[test]
fn cli_pointer_get_cases() {
    let value = serde_json::json!({
        "a": [{"b": 42}],
        "a/b": "slash",
        "x~y": "tilde",
        "": "empty key"
    });

    assert_eq!(pointer_get(&value, ""), Some(&value));
    assert_eq!(pointer_get(&value, "/a/0/b").cloned(), Some(serde_json::json!(42)));
    assert_eq!(pointer_get(&value, "/a/1/b"), None);
    assert_eq!(pointer_get(&value, "/missing"), None);
    assert_eq!(pointer_get(&value, "/a~1b").cloned(), Some(serde_json::json!("slash")));
    assert_eq!(pointer_get(&value, "/x~0y").cloned(), Some(serde_json::json!("tilde")));
    assert_eq!(pointer_get(&value, "/").cloned(), Some(serde_json::json!("empty key")));
    assert_eq!(pointer_get(&value, "a/0/b"), None);
    assert_eq!(pointer_get(&value, "/a/nope/b"), None);
}
