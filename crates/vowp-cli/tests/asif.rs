//! ASIF CLI integration tests.
//!
//! Every test uses a temporary node home and project. Fixtures are built from a
//! synthetic session or from one valid export of it; no test reads the real home,
//! touches the network, starts a harness or a tmux server.
//!
//! Spec: `docs/design/v0.4/asif-integration.md` §7.2.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;

use serde_json::{Value, json};
use vowp_cli::BufferIo;
use vowp_protocol::{
    Actor, AssistantMessage, EventBody, EventEnvelope, EventSource, HarnessKind, NodeName, SessionId, SessionState,
    SystemClock, ToolCall, ToolResult, UserMessage,
};
use vowp_store::blobs::BlobStore;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, SessionFilter};

/// The ASIF adapter pin named by the integration doc.
const ASIF_PIN: &str = "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f";

/// Creates a session through the CLI and returns its full id.
fn create(env: &common::TestEnv, title: &str) -> String {
    let (code, stdout, stderr) = env.run(&[
        "session",
        "new",
        "--title",
        title,
        "--executor",
        "opencode:deepseek/deepseek-flash#max",
    ]);
    assert_eq!(code, 0, "{stderr}");
    stdout.trim().to_owned()
}

/// Appends one event to a session log and indexes it.
fn append(env: &common::TestEnv, id: &str, actor: Actor, body: EventBody) -> String {
    let paths = NodePaths::new(env.home_path());
    let session = SessionId::parse(id).expect("session id");
    let log = SessionLog::open(&paths, &session, Arc::new(SystemClock)).expect("open log");
    let appended = log
        .append(NewEvent {
            actor,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: Some("deepseek/deepseek-flash".to_owned()),
                node: Some(NodeName::parse("local").expect("node")),
                native_session_id: None,
            },
            body,
            raw: None,
            ts: None,
        })
        .expect("append event");
    env.store().index_event(&index_row(&appended)).expect("index event");
    log.sync().expect("sync log");
    appended.envelope.id.to_string()
}

/// A synthetic session with text, a tool exchange and one binary blob.
///
/// Returns the id, the ordered text events and the blob reference + bytes.
fn text_session(env: &common::TestEnv) -> (String, Vec<(String, String)>, String, Vec<u8>) {
    let id = create(env, "asif cli session");
    append(
        env,
        &id,
        Actor::User,
        EventBody::UserMessage(UserMessage {
            text: "hello from the asif cli".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    );
    append(
        env,
        &id,
        Actor::Assistant,
        EventBody::AssistantMessage(AssistantMessage {
            text: "the package keeps this answer".to_owned(),
        }),
    );
    append(
        env,
        &id,
        Actor::Assistant,
        EventBody::ToolCall(ToolCall {
            call_id: "call-1".to_owned(),
            name: "shell".to_owned(),
            input: json!({"cmd": "true"}),
        }),
    );
    let blob_bytes = b"blob bytes for the asif cli\0with binary\x01".to_vec();
    let paths = NodePaths::new(env.home_path());
    let blob = BlobStore::new(paths.blobs_dir(&SessionId::parse(&id).expect("id")))
        .put(&blob_bytes)
        .expect("put blob");
    append(
        env,
        &id,
        Actor::Tool,
        EventBody::ToolResult(ToolResult {
            call_id: "call-1".to_owned(),
            ok: true,
            output: None,
            output_ref: Some(blob.clone()),
            exit_code: Some(0),
        }),
    );
    let texts = vec![
        ("user.message".to_owned(), "hello from the asif cli".to_owned()),
        (
            "assistant.message".to_owned(),
            "the package keeps this answer".to_owned(),
        ),
    ];
    (id, texts, blob.sha256, blob_bytes)
}

/// Exports one session with the ASIF format and returns the command result.
fn export(env: &common::TestEnv, id: &str, out: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args = vec![
        "session",
        "export",
        id,
        "--format",
        "asif",
        "--out",
        out.to_str().expect("utf-8 path"),
    ];
    args.extend_from_slice(extra);
    env.run(&args)
}

/// Imports one ASIF input and returns the command result.
fn import(env: &common::TestEnv, file: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args = vec![
        "session",
        "import",
        file.to_str().expect("utf-8 path"),
        "--format",
        "asif",
    ];
    args.extend_from_slice(extra);
    env.run(&args)
}

/// Runs one command in-process with an explicit TTY flag and stdin.
fn run_tty(env: &common::TestEnv, args: &[&str], tty: bool, input: &[u8]) -> (i32, String, String) {
    let mut full = vec![
        "vowp".to_owned(),
        "--home".to_owned(),
        env.home_path().display().to_string(),
        "--project".to_owned(),
        env.project_path().display().to_string(),
    ];
    full.extend(args.iter().map(|value| (*value).to_owned()));
    let mut io = BufferIo::with_tty(input, tty);
    let code = vowp_cli::run(full, &mut io);
    (code, io.stdout(), io.stderr())
}

/// Runs the real binary with a temporary home/project, optional cwd and stdin.
fn run_process(env: &common::TestEnv, args: &[&str], cwd: Option<&Path>, stdin: Option<&[u8]>) -> Output {
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn().expect("spawn vowp");
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(bytes)
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait for vowp")
}

/// Returns the number of session rows in the environment.
fn session_count(env: &common::TestEnv) -> usize {
    env.store()
        .list_sessions(&SessionFilter::default())
        .expect("list sessions")
        .len()
}

/// Returns the number of entries left in `<home>/tmp`.
fn tmp_entries(env: &common::TestEnv) -> usize {
    std::fs::read_dir(env.home_path().join("tmp"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Reads one session log.
fn read_log(env: &common::TestEnv, id: &str) -> Vec<EventEnvelope> {
    SessionLog::open_read(
        &NodePaths::new(env.home_path()),
        &SessionId::parse(id).expect("session id"),
    )
    .expect("open log")
    .read_all()
    .expect("read log")
}

/// Returns the ordered user/assistant text events.
fn text_events(events: &[EventEnvelope]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event.body().ok()? {
            EventBody::UserMessage(message) => Some(("user.message".to_owned(), message.text)),
            EventBody::AssistantMessage(message) => Some(("assistant.message".to_owned(), message.text)),
            _ => None,
        })
        .collect()
}

/// Unpacks one package into its exact `session.json` value.
fn unpack_session(path: &Path) -> (vowp_asif::Package, Value) {
    let package = vowp_asif::unpack(path).expect("unpack package");
    let value = serde_json::from_slice(&package.session).expect("session.json value");
    (package, value)
}

/// Writes one package with the same files and a replaced `session.json`.
fn repack(package: &vowp_asif::Package, value: &Value, path: &Path) {
    let mut package = package.clone();
    package.session = serde_json::to_vec_pretty(value).expect("session JSON");
    let bytes = vowp_asif::pack(&package).expect("pack package");
    std::fs::write(path, bytes).expect("write package");
}

/// Writes one standalone ASIF document.
fn write_document(dir: &Path, name: &str, value: &Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec_pretty(value).expect("document JSON")).expect("write document");
    path
}

#[test]
fn session_export_asif_uses_default_asif_zip_name() {
    let env = common::TestEnv::new();
    let id = create(&env, "default asif name");
    let output = run_process(
        &env,
        &["session", "export", &id, "--format", "asif"],
        Some(env.project_path()),
        None,
    );
    assert!(
        output.status.success(),
        "default export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = format!("{id}.asif.zip");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    let package = env.project_path().join(&expected);
    assert!(package.is_file(), "default package was not written");
    assert!(
        std::fs::read(&package)
            .expect("package bytes")
            .starts_with(b"PK\x03\x04")
    );
    assert!(
        !env.project_path().join(format!("{id}.vowp-bundle.json")).exists(),
        "ASIF export must not write a bundle"
    );
}

#[test]
fn session_export_asif_stdout_is_binary_and_refuses_json() {
    let env = common::TestEnv::new();
    let id = create(&env, "binary stdout");
    let reference = env.project_path().join("reference.asif.zip");
    let (code, _, stderr) = export(&env, &id, &reference, &[]);
    assert_eq!(code, 0, "{stderr}");

    let (code, stdout, _) = env.run(&["--json", "session", "export", &id, "--format", "asif", "--out", "-"]);
    assert_eq!(code, 2);
    let error: Value = serde_json::from_str(&stdout).expect("JSON error document");
    assert_eq!(error["error"]["kind"], "invalid_input");

    let output = run_process(
        &env,
        &["session", "export", &id, "--format", "asif", "--out", "-"],
        None,
        None,
    );
    assert!(
        output.status.success(),
        "stdout export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"PK\x03\x04"));
    assert_eq!(
        output.stdout,
        std::fs::read(&reference).expect("reference bytes"),
        "stdout must be the exact deterministic package"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("declared losses"),
        "the loss summary belongs on stderr"
    );
}

#[test]
fn session_export_asif_refuses_binary_stdout_on_a_tty() {
    let env = common::TestEnv::new();
    let id = create(&env, "tty refusal");
    let (code, stdout, stderr) = run_tty(
        &env,
        &["session", "export", &id, "--format", "asif", "--out", "-"],
        true,
        b"",
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty(), "a terminal must receive no ZIP bytes");
    assert!(stderr.contains("terminal"), "{stderr}");
}

#[test]
fn session_export_asif_rejects_tail_and_bundle_only_flags() {
    let env = common::TestEnv::new();
    let id = create(&env, "flag validation");
    let out = env.project_path().join("flags.asif.zip");
    let report = env.project_path().join("flags-report.json");
    let report_text = report.to_str().expect("path");
    let cases: &[(&[&str], &str)] = &[
        (&["session", "export", &id, "--format", "asif", "--tail", "5"], "--tail"),
        (&["session", "export", &id, "--redact", "value"], "--redact"),
        (
            &["session", "export", &id, "--redact-file", "values.txt"],
            "--redact-file",
        ),
        (&["session", "export", &id, "--keep-host-paths"], "--keep-host-paths"),
        (&["session", "export", &id, "--report", report_text], "--report"),
        (&["session", "export", &id, "--strict"], "--strict"),
        (&["session", "export", &id, "--allow-loss"], "--allow-loss"),
        (
            &["session", "export", &id, "--format", "asif", "--strict", "--allow-loss"],
            "cannot be used with",
        ),
        (
            &["session", "export", &id, "--format", "asif", "--report", "-"],
            "--report -",
        ),
    ];
    for (args, needle) in cases {
        let (code, _, stderr) = env.run(args);
        assert_eq!(code, 2, "args {args:?} must exit 2: {stderr}");
        assert!(stderr.contains(needle), "args {args:?} stderr: {stderr}");
    }

    let import_cases: &[(&[&str], &str)] = &[
        (&["session", "import", "input.json", "--branch", "main"], "--branch"),
        (
            &["session", "import", "input.json", "--report", report_text],
            "--report",
        ),
        (&["session", "import", "input.json", "--strict"], "--strict"),
        (&["session", "import", "input.json", "--allow-loss"], "--allow-loss"),
    ];
    for (args, needle) in import_cases {
        let (code, _, stderr) = env.run(args);
        assert_eq!(code, 2, "args {args:?} must exit 2: {stderr}");
        assert!(stderr.contains(needle), "args {args:?} stderr: {stderr}");
    }

    let (code, _, _) = export(&env, &id, &out, &["--tail", "5"]);
    assert_eq!(code, 2);
    assert!(!out.exists());
}

#[test]
fn session_export_asif_human_and_json_shapes() {
    let env = common::TestEnv::new();
    let (id, _, _, _) = text_session(&env);
    let out = env.project_path().join("human.asif.zip");
    let (code, stdout, stderr) = export(&env, &id, &out, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), out.display().to_string());
    assert!(stderr.contains("declared losses"), "{stderr}");
    assert!(stderr.contains("cwd-omitted"), "{stderr}");

    let json_out = env.project_path().join("json.asif.zip");
    let report_path = env.project_path().join("summary-report.json");
    let (code, value, stderr) = env.run_json(&[
        "session",
        "export",
        &id,
        "--format",
        "asif",
        "--out",
        json_out.to_str().expect("path"),
        "--report",
        report_path.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["format"], "asif");
    assert_eq!(value["path"], json_out.display().to_string());
    assert_eq!(value["session"], id);
    assert_eq!(
        value["bytes"].as_u64().expect("bytes"),
        std::fs::metadata(&json_out).expect("metadata").len()
    );
    assert_eq!(value["report_path"], report_path.display().to_string());
    let report = &value["report"];
    assert_eq!(report["adapter_version"], "0.1.1");
    assert_eq!(report["asif_version"], "0.4");
    assert_eq!(report["asif_pin"], ASIF_PIN);
    assert_eq!(report["source_session"], id);
    assert!(report["target_session"].is_null());
    assert!(
        report["counts"]["events_in"].as_u64().unwrap_or_default() >= 5,
        "{report}"
    );
    assert_eq!(
        report["loss_count"].as_u64().expect("loss count"),
        report["losses"].as_array().expect("losses").len() as u64
    );
    assert!(
        report["unsupported"]
            .as_array()
            .expect("unsupported")
            .contains(&json!("vowp.jobs")),
        "{report}"
    );
    assert!(report_path.is_file(), "the requested full report was not written");
}

#[test]
fn session_export_asif_then_import_round_trip() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, texts, blob_sha, blob_bytes) = text_session(&source);
    let package = env.project_path().join("roundtrip.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");

    let (code, stdout, stderr) = import(&env, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let new_id = stdout.trim().to_owned();
    assert_ne!(new_id, id);
    let events = read_log(&env, &new_id);
    assert_eq!(text_events(&events), texts);
    assert!(matches!(
        events.first().and_then(|event| event.body().ok()),
        Some(EventBody::SessionCreated(_))
    ));
    let paths = NodePaths::new(env.home_path());
    let restored = paths.blobs_dir(&SessionId::parse(&new_id).expect("id")).join(&blob_sha);
    assert_eq!(std::fs::read(&restored).expect("restored blob"), blob_bytes);

    let reexport = env.project_path().join("roundtrip-again.asif.zip");
    let (code, value, stderr) = env.run_json(&[
        "session",
        "export",
        &new_id,
        "--format",
        "asif",
        "--out",
        reexport.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["report"]["source_session"], new_id);
    assert_eq!(
        value["report"]["counts"]["events_in"].as_u64().expect("events in"),
        events.len() as u64
    );
}

#[test]
fn session_export_asif_redact_file_is_read_locally_and_never_echoed() {
    const SECRET: &str = "cli-redact-secret-do-not-echo";
    const LITERAL: &str = "cli-redact-literal-do-not-echo";
    let env = common::TestEnv::new();
    let id = create(&env, "redacted cli export");
    append(
        &env,
        &id,
        Actor::User,
        EventBody::UserMessage(UserMessage {
            text: format!("the literals {SECRET} and {LITERAL} must not survive"),
            attachments: Vec::new(),
            note: None,
        }),
    );
    let literals = env.project_path().join("redact-values.txt");
    std::fs::write(&literals, format!("{SECRET}\r\n\r\n")).expect("redact file");
    let out = env.project_path().join("redacted.asif.zip");
    let (code, stdout, stderr) = export(
        &env,
        &id,
        &out,
        &["--redact", LITERAL, "--redact-file", literals.to_str().expect("path")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains(SECRET) && !stdout.contains(LITERAL), "{stdout}");
    assert!(!stderr.contains(SECRET) && !stderr.contains(LITERAL), "{stderr}");
    let bytes = std::fs::read(&out).expect("package bytes");
    for secret in [SECRET, LITERAL] {
        assert!(
            !bytes.windows(secret.len()).any(|window| window == secret.as_bytes()),
            "the secret survives in the package bytes"
        );
    }
    let (_, document) = unpack_session(&out);
    let text = document.to_string();
    assert!(
        !text.contains(SECRET) && !text.contains(LITERAL),
        "a secret survives in session.json"
    );
    assert!(text.contains("<redacted>"), "the literal was not replaced");
}

#[test]
fn session_export_asif_keep_host_paths_is_opt_in() {
    let env = common::TestEnv::new();
    let id = create(&env, "host path scrubbing");
    append(
        &env,
        &id,
        Actor::User,
        EventBody::UserMessage(UserMessage {
            text: "inspect /tmp/alice/private/project before continuing".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    );
    let scrubbed = env.project_path().join("scrubbed.asif.zip");
    let (code, _, stderr) = export(&env, &id, &scrubbed, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (_, document) = unpack_session(&scrubbed);
    assert!(!document.to_string().contains("/tmp/alice"), "{document}");

    let kept = env.project_path().join("kept.asif.zip");
    let (code, _, stderr) = export(&env, &id, &kept, &["--keep-host-paths"]);
    assert_eq!(code, 0, "{stderr}");
    let (_, document) = unpack_session(&kept);
    assert!(document.to_string().contains("/tmp/alice"), "{document}");
}

#[test]
fn session_export_asif_reports_losses_and_writes_report_only_when_asked() {
    let env = common::TestEnv::new();
    let id = create(&env, "loss reporting");
    let out = env.project_path().join("no-report.asif.zip");
    let (code, value, stderr) = env.run_json(&[
        "session",
        "export",
        &id,
        "--format",
        "asif",
        "--out",
        out.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(value["report_path"].is_null());
    assert!(
        value["report"]["loss_count"].as_u64().unwrap_or_default() > 0,
        "a normal session must report its inherent losses"
    );
    assert!(
        !env.project_path().join("no-report.asif.zip.report.json").exists(),
        "no report sidecar may be written without --report"
    );

    let with_report = env.project_path().join("with-report.asif.zip");
    let report_path = env.project_path().join("full-report.json");
    let (code, stdout, stderr) = export(
        &env,
        &id,
        &with_report,
        &["--report", report_path.to_str().expect("path")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&with_report.display().to_string()), "{stdout}");
    assert!(stderr.contains("declared losses"), "{stderr}");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("report file")).expect("report JSON");
    assert_eq!(report["format"], "vowp-asif-report-v1");
    assert_eq!(report["direction"], "export");
    assert_eq!(report["adapter"]["version"], "0.1.1");
    assert!(report["losses"].is_array(), "{report}");
}

#[test]
fn session_export_asif_strict_exits_five_and_writes_nothing() {
    let env = common::TestEnv::new();
    let id = create(&env, "strict export");
    for _ in 0..2 {
        append(
            &env,
            &id,
            Actor::Assistant,
            EventBody::ToolCall(ToolCall {
                call_id: "duplicate-call".to_owned(),
                name: "shell".to_owned(),
                input: json!({"cmd": "true"}),
            }),
        );
    }
    let normal = env.project_path().join("strict-normal.asif.zip");
    let (code, value, stderr) = env.run_json(&[
        "session",
        "export",
        &id,
        "--format",
        "asif",
        "--out",
        normal.to_str().expect("path"),
        "--allow-loss",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        value["report"]["losses"]
            .as_array()
            .expect("losses")
            .iter()
            .any(|loss| loss["kind"] == "call-id-disambiguated"),
        "fixture must declare an integrity loss: {value}"
    );

    let out = env.project_path().join("strict-refused.asif.zip");
    let report = env.project_path().join("strict-refused-report.json");
    std::fs::write(&out, b"existing output must stay").expect("seed output");
    std::fs::write(&report, b"existing report must stay").expect("seed report");
    let (code, _, stderr) = export(
        &env,
        &id,
        &out,
        &["--strict", "--report", report.to_str().expect("path")],
    );
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("STRICT_LOSS"), "{stderr}");
    assert_eq!(
        std::fs::read(&out).expect("existing output"),
        b"existing output must stay"
    );
    assert_eq!(
        std::fs::read(&report).expect("existing report"),
        b"existing report must stay"
    );
    assert_eq!(tmp_entries(&env), 0, "strict export must remove its private work");
}

#[test]
fn session_import_asif_creates_a_new_session_and_prints_its_id() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("import.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let before = session_count(&env);

    let (code, stdout, stderr) = import(&env, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let new_id = stdout.trim().to_owned();
    assert_ne!(new_id, id);
    assert_eq!(session_count(&env), before + 1);
    let session_id = SessionId::parse(&new_id).expect("new id");
    let store = env.store();
    let session = store.require_session(&session_id).expect("new session");
    assert_eq!(session.title, "asif cli session");
    assert_eq!(session.state, SessionState::Idle);
    assert!(store.lease(&session_id).expect("lease query").is_none());
    let events = read_log(&env, &new_id);
    let Some(EventBody::SessionCreated(created)) = events.first().and_then(|event| event.body().ok()) else {
        panic!("the imported log must start with session.created");
    };
    assert_eq!(created.cwd, env.project_path().display().to_string());
    assert_eq!(
        std::fs::read_dir(NodePaths::new(env.home_path()).capsules_dir(&session_id))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "import must not write a new capsule"
    );
}

#[test]
fn session_import_asif_json_shape_and_full_report() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("json-import.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let report_path = env.project_path().join("import-report.json");
    let (code, value, stderr) = env.run_json(&[
        "session",
        "import",
        package.to_str().expect("path"),
        "--format",
        "asif",
        "--report",
        report_path.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["session"]["state"], "idle");
    assert_eq!(value["session"]["id"], value["report"]["target_session"]);
    assert_eq!(value["report"]["source_session"], id);
    assert!(value["warnings"].is_array(), "{value}");
    assert!(report_path.is_file(), "the requested import report was not written");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("report file")).expect("report JSON");
    assert_eq!(report["format"], "vowp-asif-report-v1");
    assert_eq!(report["direction"], "import");
    assert_eq!(report["target_session_id"], value["session"]["id"]);
}

#[test]
fn session_import_asif_requires_an_existing_cwd() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("cwd-source.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let before = session_count(&env);
    let missing = env.project_path().join("missing-directory");
    let (code, _, stderr) = import(&env, &package, &["--cwd", missing.to_str().expect("path")]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--cwd"), "{stderr}");
    assert_eq!(session_count(&env), before);
}

#[test]
fn session_import_asif_stdin_stages_and_imports() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("stdin-source.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let bytes = std::fs::read(&package).expect("package bytes");

    let output = run_process(
        &env,
        &["session", "import", "-", "--format", "asif"],
        None,
        Some(&bytes),
    );
    assert!(
        output.status.success(),
        "stdin import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_ne!(new_id, id);
    env.store()
        .require_session(&SessionId::parse(&new_id).expect("id"))
        .expect("imported session");
    assert_eq!(tmp_entries(&env), 0, "the stdin staging directory must be removed");
}

#[test]
fn session_import_asif_invalid_package_exits_two_and_writes_nothing() {
    let env = common::TestEnv::new();
    let before = session_count(&env);
    let bad = env.project_path().join("bad.asif.zip");
    std::fs::write(&bad, b"this is not an asif package").expect("bad package");
    let (code, _, stderr) = import(&env, &bad, &[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("ZIP") || stderr.contains("package"),
        "the error must name the invalid input: {stderr}"
    );
    assert_eq!(session_count(&env), before);
    assert_eq!(tmp_entries(&env), 0);

    let (code, _, _) = run_tty(
        &env,
        &["session", "import", "-", "--format", "asif"],
        false,
        b"not a package",
    );
    assert_eq!(code, 2, "an invalid stdin upload must exit 2");
}

#[test]
fn session_import_asif_same_package_twice_exits_four_and_keeps_the_first_session() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("twice.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stdout, stderr) = import(&env, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let first = stdout.trim().to_owned();
    let before = session_count(&env);
    let log_path = NodePaths::new(env.home_path()).log_path(&SessionId::parse(&first).expect("id"));
    let log_before = std::fs::read(&log_path).expect("first log");

    let (code, _, stderr) = import(&env, &package, &[]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("event id") || stderr.contains("already"), "{stderr}");
    assert_eq!(session_count(&env), before);
    assert_eq!(std::fs::read(&log_path).expect("first log"), log_before);
}

#[test]
fn session_import_asif_multiple_branches_requires_branch() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, texts, _, _) = text_session(&source);
    let package = env.project_path().join("branches-source.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (package, mut value) = unpack_session(&package);
    let main = value["branches"][0].clone();
    let mut side = main;
    side["id"] = json!("side");
    value["branches"].as_array_mut().expect("branches").push(side);
    let dir = tempfile::tempdir().expect("fixture dir");
    for (path, bytes) in &package.files {
        let target = dir.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("resource parent");
        }
        std::fs::write(target, bytes).expect("resource bytes");
    }
    write_document(dir.path(), "session.json", &value);

    let before = session_count(&env);
    let (code, _, stderr) = import(&env, dir.path(), &[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("more than one branch"), "{stderr}");
    assert!(stderr.contains("main") && stderr.contains("side"), "{stderr}");
    assert_eq!(session_count(&env), before);

    let (code, _, stderr) = import(&env, dir.path(), &["--branch", "missing"]);
    assert_eq!(code, 2, "{stderr}");
    assert_eq!(session_count(&env), before);

    let (code, stdout, stderr) = import(&env, dir.path(), &["--branch", "side"]);
    assert_eq!(code, 0, "{stderr}");
    let new_id = stdout.trim();
    assert_eq!(text_events(&read_log(&env, new_id)), texts);
    assert_eq!(session_count(&env), before + 1);
}

#[test]
fn session_import_asif_same_id_and_executor_are_refused() {
    let env = common::TestEnv::new();
    let (id, _, _, _) = text_session(&env);
    let package = env.project_path().join("refused-flags.asif.zip");
    let (code, _, stderr) = export(&env, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let before = session_count(&env);

    let (code, _, stderr) = import(&env, &package, &["--same-id"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--same-id"), "{stderr}");
    let (code, _, stderr) = import(&env, &package, &["--executor", "opencode:deepseek/deepseek-flash"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--executor"), "{stderr}");
    assert_eq!(session_count(&env), before);
}

#[test]
fn session_import_asif_strict_and_allow_loss_map_to_the_documented_exit_codes() {
    let env = common::TestEnv::new();
    let source = common::TestEnv::new();
    let (id, _, _, _) = text_session(&source);
    let package = env.project_path().join("orphan-source.asif.zip");
    let (code, _, stderr) = export(&source, &id, &package, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (package, mut value) = unpack_session(&package);
    let result = value["events"]
        .as_array_mut()
        .expect("events")
        .iter_mut()
        .find(|event| event["kind"] == "tool_result")
        .expect("tool_result event");
    result["data"]["call_id"] = json!("orphan-call-id");
    let orphan = env.project_path().join("orphan.asif.zip");
    repack(&package, &value, &orphan);

    let before = session_count(&env);
    let (code, _, stderr) = import(&env, &orphan, &["--strict"]);
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("STRICT_LOSS"), "{stderr}");
    assert_eq!(session_count(&env), before);
    assert_eq!(tmp_entries(&env), 0);

    let (code, stdout, stderr) = import(&env, &orphan, &["--allow-loss"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(session_count(&env), before + 1);
    let new_id = stdout.trim();
    let events = read_log(&env, new_id);
    assert!(!events.is_empty());
}
