//! Capsule writer tests: prompt rendering, JSON-repair, debounce, compaction and persistence.
//! Spec: `docs/design/sessions.md` §3 and `docs/design/crates/vowp-core.md` §3 named tests.

mod support;

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::lifecycle::compact_session;
use vowp_core::session::bundle;
use vowp_core::session::capsule::CapsuleWriter;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, BlobRef, Capsule, CapsuleId,
    CapsuleState, CapsuleTrigger, Cost, EventBody, EventEnvelope, EventSource, Executor, ExecutorRef, FakeClock,
    HarnessEvent, HarnessKind, LimitEvent, LimitKind, MemoryLog, NodeName, ProbeResult, RunOutcome, RunSummary,
    SecretName, SessionCreated, SessionId, SessionRole, StartInput, StopReason, Timestamp, ToolCall, ToolResult,
    TurnEnded,
};
use vowp_store::blobs::BlobStore;
use vowp_store::session_files::list_capsule_files;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{LedgerFilter, NodePaths, SessionFilter};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn executor() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor")
}

fn claude() -> Executor {
    Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude executor")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps_with_clock(
    tmp: &TempDir,
    config: vowp_protocol::VowpConfig,
    registry: AdapterRegistry,
    clock: FakeClock,
) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config),
        node: NodeName::parse("local").expect("node name"),
        clock: Arc::new(clock),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(registry),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(0)),
    }
}

fn create_session(service: &SessionService, tmp: &TempDir) -> SessionId {
    service
        .create_session(vowp_protocol::NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: "capsule session".to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: Some(executor()),
        })
        .expect("create session")
}

fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::Opencode,
        model: Some("deepseek/deepseek-flash".to_owned()),
        node: Some(NodeName::parse("local").expect("node")),
        native_session_id: None,
    }
}

fn envelope(session: &SessionId, seq: u64, body: EventBody) -> EventEnvelope {
    EventEnvelope::new(session.clone(), seq, ts(), Actor::System, source(), &body)
}

fn good_draft(intent: &str) -> String {
    serde_json::json!({
        "intent": intent,
        "constraints": ["no secrets"],
        "decisions": [],
        "state": {"done": ["built"], "in_progress": [], "blocked": [], "next_steps": ["test"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

fn capsule(session: &SessionId, id: CapsuleId, at_seq: u64, intent: &str, trigger: CapsuleTrigger) -> Capsule {
    Capsule {
        version: vowp_protocol::CAPSULE_VERSION,
        capsule_id: id,
        session_id: session.clone(),
        created_at: ts(),
        at_seq,
        trigger,
        from: ExecutorRef {
            executor: executor(),
            node: Some(NodeName::parse("local").expect("node")),
        },
        intent: intent.to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: Default::default(),
    }
}

#[test]
fn capsule_prompt_golden() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new(),
        FakeClock::at(ts()),
    );
    let writer = CapsuleWriter::new(deps);
    let session = SessionId::new();
    let previous = capsule(
        &session,
        CapsuleId::new(),
        2,
        "Ship the session service",
        CapsuleTrigger::Limit,
    );
    let events = vec![
        envelope(
            &session,
            3,
            EventBody::ToolCall(ToolCall {
                call_id: "call_1".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
        ),
        envelope(
            &session,
            4,
            EventBody::ToolResult(ToolResult {
                call_id: "call_1".to_owned(),
                ok: true,
                output: Some("all green".to_owned()),
                output_ref: None,
                exit_code: Some(0),
            }),
        ),
        envelope(
            &session,
            5,
            EventBody::AssistantMessage(AssistantMessage {
                text: "tests pass".to_owned(),
            }),
        ),
    ];
    let from = ExecutorRef {
        executor: executor(),
        node: Some(NodeName::parse("local").expect("node")),
    };
    let prompt = writer.build_prompt(Some(&previous), &events, &from);

    assert!(prompt.starts_with("You are the handoff writer for a software-engineering session."));
    assert!(
        prompt.contains(&format!("SESSION: {session}")),
        "no session.created in new material: title falls back to the id"
    );
    assert!(prompt.contains("EXECUTOR THAT WAS RUNNING: opencode:deepseek/deepseek-flash#max"));
    assert!(prompt.contains("REASON FOR HANDOFF: limit"));
    assert!(prompt.contains("PREVIOUS CAPSULE (covers events up to seq 2"));
    assert!(prompt.contains("Ship the session service"));
    assert!(prompt.contains("NEW MATERIAL (events seq 3..5):"));
    assert!(prompt.contains("[seq 3 · tool.call] shell"));
    assert!(prompt.contains("[seq 4 · tool.result ok] all green"));
    assert!(prompt.contains("[seq 5 · assistant] tests pass"));
    let schema = serde_json::to_string_pretty(&vowp_protocol::schema_for_name("CapsuleDraft").expect("schema"))
        .expect("schema text");
    assert!(prompt.contains(&schema));
    assert!(prompt.contains("Return ONLY one JSON object, no markdown fence, no commentary"));
    assert!(
        prompt
            .trim_end()
            .ends_with("- Never include secrets, tokens, or key values; refer to them by name only.")
    );
    assert!(!prompt.contains("```"));
}

#[test]
fn capsule_parse_draft_with_fences_and_prose() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new(),
        FakeClock::at(ts()),
    );
    let writer = CapsuleWriter::new(deps);
    let draft = good_draft("Keep going");
    let text = format!("Sure — here is the handoff you asked for:\n```json\n{draft}\n```\nLet me know.");
    let parsed = writer.parse_draft(&text).expect("parse fenced draft");
    assert_eq!(parsed.intent, "Keep going");
    assert_eq!(parsed.state.done, vec!["built".to_owned()]);
    assert!(writer.parse_draft("no json here").is_err());
}

#[test]
fn capsule_write_valid_first_try() {
    let tmp = tempdir().expect("tempdir");
    let adapter = ScriptedAdapter::new(HarnessKind::Opencode, vec![good_draft("Valid capsule")]);
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter.clone())),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let writer = CapsuleWriter::new(deps.clone());
    let written = writer.write(&session, CapsuleTrigger::Manual).expect("capsule written");
    assert_eq!(written.intent, "Valid capsule");
    assert_eq!(written.trigger, CapsuleTrigger::Manual);
    assert_eq!(written.session_id, session);
    assert_eq!(written.at_seq, 1, "only session.created was covered");
    let store = deps.open_store().expect("store");
    assert_eq!(
        store
            .latest_capsule(&session)
            .expect("latest")
            .expect("capsule")
            .capsule_id,
        written.capsule_id
    );
    assert_eq!(list_capsule_files(&deps.paths, &session).expect("files").len(), 1);
    let events = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .read_all()
        .expect("events");
    assert!(matches!(
        events.last().expect("event").body(),
        Ok(EventBody::Capsule(payload)) if payload.capsule_id == written.capsule_id
    ));
    let sessions = store.list_sessions(&SessionFilter::default()).expect("sessions");
    let capsule_session = sessions
        .iter()
        .find(|row| row.role == Some(SessionRole::Keeper))
        .expect("capsule session row");
    assert_eq!(capsule_session.parent_session_id, Some(session));
    assert_eq!(adapter.starts().len(), 1);
}

#[test]
fn capsule_compaction_keeps_first_user_and_last_20() {
    let tmp = tempdir().expect("tempdir");
    let mut config = config_for(tmp.path());
    config.capsule.max_input_chars = 2_000;
    let deps = deps_with_clock(&tmp, config, AdapterRegistry::new(), FakeClock::at(ts()));
    let writer = CapsuleWriter::new(deps);
    let session = SessionId::new();
    let mut events = Vec::new();
    events.push(envelope(
        &session,
        1,
        EventBody::UserMessage(vowp_protocol::UserMessage {
            text: "FIRST USER MESSAGE must survive".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    ));
    for index in 0..40_u64 {
        events.push(envelope(
            &session,
            index + 2,
            EventBody::ToolResult(ToolResult {
                call_id: format!("call_{index}"),
                ok: true,
                output: Some(format!("TOOL-BODY-{index}-{}", "z".repeat(5000))),
                output_ref: None,
                exit_code: Some(0),
            }),
        ));
        events.push(envelope(
            &session,
            index + 42,
            EventBody::AssistantMessage(AssistantMessage {
                text: format!("marker-{index}"),
            }),
        ));
    }
    let from = ExecutorRef {
        executor: executor(),
        node: Some(NodeName::parse("local").expect("node")),
    };
    let prompt = writer.build_prompt(None, &events, &from);

    assert!(prompt.contains("FIRST USER MESSAGE must survive"));
    assert!(
        prompt.contains("earlier events omitted"),
        "an omission marker is present"
    );
    assert!(!prompt.contains("TOOL-BODY-0-"), "tool bodies are dropped first");
    for marker in 20..40 {
        assert!(prompt.contains(&format!("marker-{marker}")), "last-20 marker {marker}");
    }
    assert!(prompt.contains("[seq 81 · assistant] marker-39"));
}

#[test]
fn capsule_limit_falls_back_to_next_keeper_rung() {
    let tmp = tempdir().expect("tempdir");
    let mut config = config_for(tmp.path());
    let keeper = config.profiles.get_mut("keeper").expect("keeper profile");
    keeper.ladder = vec![executor(), claude()];
    config.capsule.executor = Some(executor());
    let limit = LimitEvent {
        kind: LimitKind::Quota,
        provider: "deepseek".to_owned(),
        resets_at: Some(Timestamp::from_unix_secs(1_794_164_400)),
        message: "usage limit reached".to_owned(),
        http_status: None,
    };
    let limited = FakeAdapter::new(HarnessKind::Opencode, Vec::new()).with_summary(RunSummary {
        exit_code: Some(1),
        outcome: RunOutcome::Limit,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: Some(limit),
        result_text: None,
        stop_reason: StopReason::Limit,
    });
    let fallback = ScriptedAdapter::new(HarnessKind::ClaudeCode, vec![good_draft("Fallback capsule")]);
    let deps = deps_with_clock(
        &tmp,
        config,
        AdapterRegistry::new()
            .with("opencode", Arc::new(limited))
            .with("claude-code", Arc::new(fallback.clone())),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let writer = CapsuleWriter::new(deps.clone());
    let written = writer
        .write(&session, CapsuleTrigger::Limit)
        .expect("capsule written on the next rung");
    assert_eq!(written.intent, "Fallback capsule");
    assert_eq!(fallback.starts().len(), 1, "the next keeper rung ran once");
    let store = deps.open_store().expect("store");
    assert!(store.limit_for("deepseek").expect("limit").is_some());
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::State),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let row = ledger
        .iter()
        .find(|entry| entry.session_id == Some(session.clone()) && entry.summary == "capsule written")
        .expect("capsule state row");
    assert_eq!(row.detail["capsule_rung"], serde_json::json!(1));
}

#[test]
fn capsule_write_repairs_once_then_ok() {
    let tmp = tempdir().expect("tempdir");
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec!["this is not json".to_owned(), good_draft("Repaired capsule")],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter.clone())),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let writer = CapsuleWriter::new(deps.clone());
    let written = writer
        .write(&session, CapsuleTrigger::Manual)
        .expect("repaired capsule");
    assert_eq!(written.intent, "Repaired capsule");
    let starts = adapter.starts();
    assert_eq!(starts.len(), 2, "exactly one repair run");
    assert_eq!(starts[0].session_id, starts[1].session_id);
    assert!(starts[1].prompt.contains("Your previous answer was invalid"));
    assert!(starts[1].prompt.contains("Return ONLY the corrected JSON object"));
}

#[test]
fn capsule_write_twice_invalid_keeps_previous_and_logs_error_event() {
    let tmp = tempdir().expect("tempdir");
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec!["still not json".to_owned(), "also not json".to_owned()],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let store = deps.open_store().expect("store");
    let previous = capsule(
        &session,
        CapsuleId::new(),
        1,
        "Previous good capsule",
        CapsuleTrigger::TurnEnd,
    );
    store
        .insert_capsule(&previous, "previous.json")
        .expect("insert previous");

    let writer = CapsuleWriter::new(deps.clone());
    let error = writer
        .write(&session, CapsuleTrigger::Manual)
        .expect_err("both answers invalid");
    assert!(matches!(error, vowp_core::CoreError::CapsuleInvalid(_)));
    assert_eq!(
        store
            .latest_capsule(&session)
            .expect("latest")
            .expect("capsule")
            .capsule_id,
        previous.capsule_id,
        "a failed write never replaces the previous capsule"
    );
    let events = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .read_all()
        .expect("events");
    let error = events
        .iter()
        .rev()
        .find_map(|event| match event.body() {
            Ok(EventBody::Error(payload)) => Some(payload),
            _ => None,
        })
        .expect("error event");
    assert_eq!(error.kind, vowp_protocol::ErrorKind::Parse);
    assert!(error.message.starts_with("capsule invalid after repair:"));
}

#[test]
fn capsule_debounce_120s_except_manual_switch_limit_export() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let adapter = ScriptedAdapter::new(HarnessKind::Opencode, vec![good_draft("Debounced")]);
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let writer = CapsuleWriter::new(deps);

    let first = writer.write(&session, CapsuleTrigger::TurnEnd).expect("first");
    clock.advance(Duration::from_secs(60));
    let debounced = writer.write(&session, CapsuleTrigger::TurnEnd).expect("debounced");
    assert_eq!(
        debounced.capsule_id, first.capsule_id,
        "60 s is inside the 120 s window"
    );
    clock.advance(Duration::from_secs(61));
    let later = writer.write(&session, CapsuleTrigger::TurnEnd).expect("after window");
    assert_ne!(later.capsule_id, first.capsule_id, "121 s starts a new capsule");

    for trigger in [
        CapsuleTrigger::Manual,
        CapsuleTrigger::Switch,
        CapsuleTrigger::Limit,
        CapsuleTrigger::Export,
    ] {
        let capsule = writer.write(&session, trigger).expect("exempt trigger");
        assert_ne!(capsule.capsule_id, later.capsule_id, "{trigger:?} never debounces");
    }
}

#[test]
fn compacted_session_export_and_resume_start_from_the_capsule() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new(),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let store = deps.open_store().expect("store");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        let appended = log
            .append(NewEvent {
                actor: Actor::User,
                source: source(),
                body: EventBody::UserMessage(vowp_protocol::UserMessage {
                    text: "remember the frobnicator".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
                raw: None,
                ts: None,
            })
            .expect("append");
        store.index_event(&index_row(&appended)).expect("index");
        log.sync().expect("sync");
    }
    store
        .set_session_state(&session, vowp_protocol::SessionState::Closed, Some(&executor()))
        .expect("close session");

    let compacted = compact_session(&deps, &store, &session, ts()).expect("compact session");
    assert!(!compacted.skipped);
    let capsule = store.latest_capsule(&session).expect("capsule query").expect("capsule");
    assert_eq!(compacted.capsule_id.as_ref(), Some(&capsule.capsule_id));
    assert!(!deps.paths.log_path(&session).exists(), "compaction deletes the log");

    let exported = bundle::export(&deps, &session, 20).expect("export compacted session");
    assert_eq!(exported.capsule.capsule_id, capsule.capsule_id);
    assert_eq!(exported.capsule.intent, capsule.intent);
    assert!(exported.tail.is_empty(), "a compacted bundle has no event tail");
    assert_eq!(exported.compacted_at, Some(ts()));
    assert_eq!(exported.session.log_bytes, 0);

    let child = bundle::import(&deps, &exported, None, None).expect("import compacted bundle");
    let seed = bundle::imported_seed(&deps, &exported, &child, "continue the work");
    assert!(
        seed.contains(&capsule.intent),
        "resume lost the capsule history: {seed}"
    );
    assert!(
        seed.contains("(no earlier turns)"),
        "a compacted bundle must not invent an event tail: {seed}"
    );
    assert!(seed.contains("continue the work"));
}

#[test]
fn bundle_export_redacts_secret_from_capsule_file_row_and_json() {
    let tmp = tempdir().expect("tempdir");
    let secret = "bundle-\"quoted\"\\path\nsecond line";
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec![good_draft(&format!("Continue with {secret}"))],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let name = SecretName::parse("BUNDLE_KEY").expect("secret name");
    vowp_core::vault::write_secret(&deps, &name, secret).expect("write secret");
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);

    let exported = bundle::export(&deps, &session, 20).expect("export");
    let marker = "[REDACTED:BUNDLE_KEY]";
    assert_eq!(exported.capsule.intent, format!("Continue with {marker}"));
    let json = bundle::to_json(&exported).expect("bundle json");
    let escaped = serde_json::to_string(secret).expect("json-escape the secret");
    let escaped = &escaped[1..escaped.len() - 1];
    assert!(!json.contains(secret), "the raw secret is not exported");
    assert!(!json.contains(escaped), "the JSON-escaped secret is not exported");
    assert!(json.contains(marker), "the bundle records the redaction");

    let store = deps.open_store().expect("store");
    assert_eq!(
        store
            .latest_capsule(&session)
            .expect("latest capsule")
            .expect("capsule row")
            .intent,
        exported.capsule.intent,
        "the cached capsule body is redacted too"
    );
    let files = list_capsule_files(&deps.paths, &session).expect("capsule files");
    assert!(!files.is_empty(), "export wrote a capsule file");
    for path in files {
        let text = std::fs::read_to_string(path).expect("read capsule file");
        assert!(!text.contains(secret), "the capsule file does not hold the raw secret");
        assert!(
            !text.contains(escaped),
            "the capsule file does not hold the JSON-escaped secret"
        );
    }
}

#[test]
fn bundle_import_redacts_secret_from_old_bundle_capsule_file_and_row() {
    let tmp = tempdir().expect("tempdir");
    let secret = "import-\"quoted\"\\path\nsecond line";
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec![good_draft(&format!("Continue with {secret}"))],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);

    // This bundle is old/foreign: it was produced before the local home knew the secret,
    // so its embedded capsule still carries the raw value.
    let exported = bundle::export(&deps, &session, 20).expect("export");
    assert!(
        exported.capsule.intent.contains(secret),
        "the source bundle carries the secret"
    );

    let name = SecretName::parse("IMPORT_KEY").expect("secret name");
    vowp_core::vault::write_secret(&deps, &name, secret).expect("write secret");
    let marker = "[REDACTED:IMPORT_KEY]";

    let child = bundle::import(&deps, &exported, None, None).expect("import");
    let store = deps.open_store().expect("store");
    let row = store
        .latest_capsule(&child)
        .expect("latest capsule")
        .expect("imported capsule row");
    assert_eq!(row.intent, format!("Continue with {marker}"));
    assert!(!row.intent.contains(secret), "the row does not hold the raw secret");
    let row_json = serde_json::to_string(&row).expect("capsule row json");
    let escaped = serde_json::to_string(secret).expect("json-escape the secret");
    let escaped = &escaped[1..escaped.len() - 1];
    assert!(
        !row_json.contains(escaped),
        "the row does not hold the JSON-escaped secret"
    );

    let files = list_capsule_files(&deps.paths, &child).expect("capsule files");
    assert!(!files.is_empty(), "import wrote a capsule file");
    for path in files {
        let text = std::fs::read_to_string(path).expect("read capsule file");
        assert!(!text.contains(secret), "the capsule file does not hold the raw secret");
        assert!(
            !text.contains(escaped),
            "the capsule file does not hold the JSON-escaped secret"
        );
        assert!(text.contains(marker), "the capsule file records the redaction");
    }
}

#[test]
fn bundle_import_sidecar_and_first_turn_seed_do_not_carry_secret() {
    let tmp = tempdir().expect("tempdir");
    let secret = "sidecar-\"quoted\"\\path\nsecond line";
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec![good_draft(&format!("Continue with {secret}"))],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);

    // Build the bundle before this node knows the secret, like a bundle from a node
    // that never had it: the embedded capsule still carries the raw value.
    let exported = bundle::export(&deps, &session, 20).expect("export");
    assert!(
        exported.capsule.intent.contains(secret),
        "the source bundle carries the secret"
    );

    let name = SecretName::parse("SIDECAR_KEY").expect("secret name");
    vowp_core::vault::write_secret(&deps, &name, secret).expect("write secret");
    let marker = "[REDACTED:SIDECAR_KEY]";
    let escaped = serde_json::to_string(secret).expect("json-escape the secret");
    let escaped = &escaped[1..escaped.len() - 1];

    let child = bundle::import(&deps, &exported, None, None).expect("import");

    let sidecar_path = deps.paths.session_dir(&child).join("import.json");
    let sidecar = std::fs::read_to_string(&sidecar_path).expect("import sidecar");
    assert!(!sidecar.contains(secret), "import.json does not hold the raw secret");
    assert!(
        !sidecar.contains(escaped),
        "import.json does not hold the JSON-escaped secret"
    );
    assert!(sidecar.contains(marker), "import.json records the redaction");
    let stored = bundle::from_json(&sidecar).expect("the redacted sidecar still validates");
    assert_eq!(stored.capsule.intent, format!("Continue with {marker}"));

    // The public seed still gets a raw bundle from a caller that exported it earlier.
    let seed = bundle::imported_seed(&deps, &exported, &child, "next step");
    assert!(!seed.contains(secret), "the raw value does not reach the seed: {seed}");
    assert!(
        !seed.contains(escaped),
        "the JSON-escaped value does not reach the seed"
    );
    assert!(seed.contains(marker), "the seed records the redaction: {seed}");
    assert!(seed.ends_with("next step"));

    let stored_seed = bundle::imported_seed(&deps, &stored, &child, "next step");
    assert!(!stored_seed.contains(secret));
    assert!(!stored_seed.contains(escaped));
    assert!(stored_seed.contains(marker));

    let store = deps.open_store().expect("store");
    let row = store
        .latest_capsule(&child)
        .expect("latest capsule")
        .expect("imported capsule row");
    assert_eq!(row.intent, format!("Continue with {marker}"));
    for path in list_capsule_files(&deps.paths, &child).expect("capsule files") {
        let text = std::fs::read_to_string(path).expect("read capsule file");
        assert!(!text.contains(secret), "the capsule file does not hold the raw secret");
        assert!(
            !text.contains(escaped),
            "the capsule file does not hold the JSON-escaped secret"
        );
    }
}

#[test]
fn bundle_import_redacts_inline_text_and_json_blob_secrets() {
    let tmp = tempdir().expect("tempdir");
    let adapter = ScriptedAdapter::new(HarnessKind::Opencode, vec![good_draft("Inline blob handoff")]);
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let secret = "blob-\"quoted\"\\path\nsecond line";
    let name = SecretName::parse("BLOB_KEY").expect("secret name");
    let marker = "[REDACTED:BLOB_KEY]";
    let escaped = serde_json::to_string(secret).expect("json-escape the secret");
    let escaped = &escaped[1..escaped.len() - 1];

    // Hand-built inline blobs on an otherwise exported bundle: one JSON document and one
    // plain-text document carry the secret, and one binary blob must pass through.
    let json_blob = serde_json::json!({
        "command": "printf",
        "token": secret,
        "filler": "x".repeat(9000),
    })
    .to_string();
    let text_blob = format!("plain text before {secret} after");
    let stable_blob = "unchanged text without any secret";
    let binary = vec![0_u8, 0xff, 0x80, 0x01];
    let json_ref = BlobRef {
        sha256: sha256_hex(json_blob.as_bytes()),
        bytes: json_blob.len() as u64,
        preview: json_blob.chars().take(vowp_protocol::BLOB_PREVIEW_CHARS).collect(),
    };
    let text_ref = BlobRef {
        sha256: sha256_hex(text_blob.as_bytes()),
        bytes: text_blob.len() as u64,
        preview: text_blob.chars().take(vowp_protocol::BLOB_PREVIEW_CHARS).collect(),
    };
    let stable_sha256 = sha256_hex(stable_blob.as_bytes());
    let binary_sha256 = sha256_hex(&binary);

    let mut exported = bundle::export(&deps, &session, 20).expect("export");
    exported.blobs_inline.clear();
    exported
        .blobs_inline
        .insert(json_ref.sha256.clone(), base64_encode(json_blob.as_bytes()));
    exported
        .blobs_inline
        .insert(text_ref.sha256.clone(), base64_encode(text_blob.as_bytes()));
    exported
        .blobs_inline
        .insert(stable_sha256.clone(), base64_encode(stable_blob.as_bytes()));
    exported
        .blobs_inline
        .insert(binary_sha256.clone(), base64_encode(&binary));
    exported.tail.push(envelope(
        &session,
        exported.session.last_seq + 1,
        EventBody::ToolResult(ToolResult {
            call_id: "inline-json".to_owned(),
            ok: true,
            output: None,
            output_ref: Some(json_ref.clone()),
            exit_code: Some(0),
        }),
    ));
    exported.tail.push(envelope(
        &session,
        exported.session.last_seq + 2,
        EventBody::UserMessage(vowp_protocol::UserMessage {
            text: "see the attached notes".to_owned(),
            attachments: vec![
                text_ref.clone(),
                BlobRef {
                    sha256: stable_sha256.clone(),
                    bytes: stable_blob.len() as u64,
                    preview: stable_blob.to_owned(),
                },
            ],
            note: None,
        }),
    ));
    seal_bundle(&mut exported);
    let source_json = bundle::to_json(&exported).expect("source bundle json");
    assert!(
        source_json.contains(escaped),
        "the source bundle carries the secret in the text blob preview"
    );
    assert!(
        source_json.contains(&json_ref.sha256) && source_json.contains(&text_ref.sha256),
        "the source bundle references the raw blobs"
    );

    // The local node learns the secret only after the bundle was produced.
    vowp_core::vault::write_secret(&deps, &name, secret).expect("write secret");

    let child = bundle::import(&deps, &exported, None, None).expect("import");

    let sidecar_path = deps.paths.session_dir(&child).join("import.json");
    let sidecar = std::fs::read_to_string(&sidecar_path).expect("import sidecar");
    assert!(!sidecar.contains(secret), "import.json does not hold the raw secret");
    assert!(
        !sidecar.contains(escaped),
        "import.json does not hold the JSON-escaped secret"
    );
    assert!(sidecar.contains(marker), "import.json records the redaction");
    let stored = bundle::from_json(&sidecar).expect("the redacted sidecar still validates");
    for old in [&json_ref.sha256, &text_ref.sha256] {
        assert!(
            !stored.blobs_inline.contains_key(old),
            "the raw blob key {old} is gone from import.json"
        );
    }
    assert!(
        stored.blobs_inline.contains_key(&binary_sha256),
        "the binary blob keeps its digest"
    );
    assert!(
        stored.blobs_inline.contains_key(&stable_sha256),
        "a text blob without a secret keeps its digest"
    );

    let blobs = BlobStore::new(deps.paths.blobs_dir(&child));
    let mut saw_json = false;
    let mut saw_text = false;
    for (sha256, encoded) in &stored.blobs_inline {
        let bytes = base64_decode(encoded);
        assert_eq!(sha256_hex(&bytes), *sha256, "the sidecar key matches its content");
        assert_eq!(
            blobs.get(sha256).expect("sidecar blob is in the child store"),
            bytes,
            "the child store holds exactly the sidecar bytes"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(secret), "blob {sha256} holds the raw secret");
        assert!(!text.contains(escaped), "blob {sha256} holds the JSON-escaped secret");
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if value.get("token").is_some() {
                assert_eq!(value["token"], marker);
                assert_eq!(value["filler"].as_str().expect("filler").len(), 9000);
                saw_json = true;
            }
        } else if text.contains(marker) {
            assert_eq!(text.trim_end(), format!("plain text before {marker} after"));
            saw_text = true;
        }
    }
    assert!(saw_json, "the JSON blob is redacted through its string values");
    assert!(saw_text, "the plain-text blob is redacted as text");
    assert_eq!(blobs.get(&binary_sha256).expect("binary blob"), binary);
    assert_eq!(
        blobs.get(&stable_sha256).expect("stable blob"),
        stable_blob.as_bytes().to_vec()
    );
    assert!(
        !blobs.exists(&json_ref.sha256) && !blobs.exists(&text_ref.sha256),
        "the raw blobs never enter the child store"
    );

    let seed = bundle::imported_seed(&deps, &exported, &child, "next step");
    assert!(!seed.contains(secret), "the raw value does not reach the seed: {seed}");
    assert!(
        !seed.contains(escaped),
        "the JSON-escaped value does not reach the seed"
    );
    let stored_seed = bundle::imported_seed(&deps, &stored, &child, "next step");
    assert!(!stored_seed.contains(secret));
    assert!(!stored_seed.contains(escaped));
}

#[test]
fn bundle_import_redaction_error_leaves_no_raw_capsule_file_or_row() {
    let tmp = tempdir().expect("tempdir");
    let marker = "IMPORT-FAILURE-RAW-CAPSULE-9d2";
    let adapter = ScriptedAdapter::new(
        HarnessKind::Opencode,
        vec![good_draft(&format!("Continue with {marker}"))],
    );
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        FakeClock::at(ts()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp);
    let exported = bundle::export(&deps, &session, 20).expect("export");
    assert!(
        exported.capsule.intent.contains(marker),
        "the source bundle carries the marker"
    );

    // A secret file the vault refuses (mode 0644) makes Redactor::load fail, the same
    // error path as an unreadable vault during import.
    let secrets = deps.paths.root().join("secrets");
    std::fs::create_dir_all(&secrets).expect("secrets dir");
    let broken = secrets.join("BROKEN_IMPORT_KEY");
    std::fs::write(&broken, "not a usable secret").expect("write broken secret");
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).expect("break secret mode");

    // The origin session and its export capsule already exist and legitimately carry the
    // marker. The ordering guarantee is stronger than "no raw capsule survives": a failed
    // redaction must do nothing at all. The old write-then-redact order created the child
    // session row and log, then the raw capsule file/row, before it noticed the vault error.
    let mut files_before = files_under(&deps.paths.sessions_dir());
    files_before.sort();
    let store = deps.open_store().expect("store");
    let mut sessions_before: Vec<SessionId> = store
        .list_sessions(&SessionFilter::default())
        .expect("sessions before")
        .into_iter()
        .map(|row| row.id)
        .collect();
    sessions_before.sort();

    let error = bundle::import(&deps, &exported, None, None).expect_err("the redaction must fail");
    match &error {
        vowp_core::CoreError::Invalid(message) => assert!(
            message.contains("must be mode 0600"),
            "unexpected redaction error: {message}"
        ),
        other => panic!("unexpected error: {other:?}"),
    }

    let mut files_after = files_under(&deps.paths.sessions_dir());
    files_after.sort();
    assert_eq!(
        files_after, files_before,
        "a failed import must not leave a session directory, log, capsule file or sidecar"
    );
    let mut sessions_after: Vec<SessionId> = store
        .list_sessions(&SessionFilter::default())
        .expect("sessions after")
        .into_iter()
        .map(|row| row.id)
        .collect();
    sessions_after.sort();
    assert_eq!(
        sessions_after, sessions_before,
        "a failed import must not leave a child session row or capsule row"
    );
}

/// Returns the lowercase hex SHA-256 of `bytes` for a hand-built bundle.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Encodes bytes as padded standard base64 for a hand-built bundle.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(ALPHABET[(b[0] >> 2) as usize] as char);
        out.push(ALPHABET[(((b[0] & 0b11) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b[1] & 0b1111) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b[2] & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decodes the padded standard base64 stored in a bundle sidecar.
fn base64_decode(text: &str) -> Vec<u8> {
    fn value(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("invalid base64 character {:?}", other as char),
        }
    }
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(4) {
        let a = value(chunk[0]);
        let b = value(chunk[1]);
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            let c = value(chunk[2]);
            out.push(((b & 0b1111) << 4) | (c >> 2));
            if chunk[3] != b'=' {
                let d = value(chunk[3]);
                out.push(((c & 0b11) << 6) | d);
            }
        }
    }
    out
}

/// Recomputes a hand-built bundle's canonical checksum, like `bundle::export` does.
fn seal_bundle(bundle: &mut bundle::Bundle) {
    let mut value = serde_json::to_value(&*bundle).expect("bundle value");
    if let Some(map) = value.as_object_mut() {
        map.remove("checksum");
    }
    let canonical = serde_json::to_vec(&value).expect("canonical bundle");
    bundle.checksum = format!("sha256:{}", sha256_hex(&canonical));
}

/// Returns every regular file below `dir`, recursively.
fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
}

/// An adapter that returns the next scripted assistant message, reusing the last one.
#[derive(Clone)]
struct ScriptedAdapter {
    kind: HarnessKind,
    responses: Arc<Mutex<VecDeque<String>>>,
    starts: Arc<Mutex<Vec<StartInput>>>,
}

impl ScriptedAdapter {
    fn new(kind: HarnessKind, responses: Vec<String>) -> Self {
        Self {
            kind,
            responses: Arc::new(Mutex::new(responses.into())),
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn starts(&self) -> Vec<StartInput> {
        self.starts.lock().expect("starts").clone()
    }

    fn next(&self) -> String {
        let mut responses = self.responses.lock().expect("responses");
        if responses.len() > 1 {
            responses.pop_front().unwrap_or_default()
        } else {
            responses.front().cloned().unwrap_or_default()
        }
    }
}

impl Adapter for ScriptedAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("scripted".to_owned()),
            path: None,
            detail: String::new(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.starts.lock().expect("starts").push(input);
        let text = self.next();
        let (sender, receiver) = channel();
        let _ = sender.send(HarnessEvent {
            ts: Some(ts()),
            actor: Actor::Assistant,
            body: EventBody::AssistantMessage(AssistantMessage { text: text.clone() }),
            raw: None,
        });
        let _ = sender.send(HarnessEvent {
            ts: Some(ts()),
            actor: Actor::System,
            body: EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: StopReason::EndTurn,
                duration_ms: Some(1),
            }),
            raw: None,
        });
        drop(sender);
        Ok(Box::new(ScriptedRun {
            receiver,
            summary: RunSummary {
                exit_code: Some(0),
                outcome: RunOutcome::Ok,
                native_session_id: None,
                cost: Cost::unknown(),
                limit: None,
                result_text: Some(text),
                stop_reason: StopReason::EndTurn,
            },
        }))
    }
}

struct ScriptedRun {
    receiver: Receiver<HarnessEvent>,
    summary: RunSummary,
}

impl AdapterRun for ScriptedRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(self.summary.clone())
    }
}

#[test]
fn capsule_build_prompt_includes_limit_and_switch_lines() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_clock(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new(),
        FakeClock::at(ts()),
    );
    let writer = CapsuleWriter::new(deps);
    let session = SessionId::new();
    let events = vec![
        envelope(
            &session,
            1,
            EventBody::SessionCreated(SessionCreated {
                title: "golden".to_owned(),
                cwd: "/home/user/project".to_owned(),
                project: None,
                job_id: None,
                role: Some(SessionRole::Worker),
                parent_session_id: None,
            }),
        ),
        envelope(
            &session,
            2,
            EventBody::Limit(LimitEvent {
                kind: LimitKind::Quota,
                provider: "claude".to_owned(),
                resets_at: Some(Timestamp::from_unix_secs(1_794_164_400)),
                message: "five_hour rejected".to_owned(),
                http_status: None,
            }),
        ),
    ];
    let from = ExecutorRef {
        executor: executor(),
        node: None,
    };
    let prompt = writer.build_prompt(None, &events, &from);
    assert!(prompt.contains("SESSION: golden  (job: none, working directory: /home/user/project)"));
    assert!(prompt.contains("REASON FOR HANDOFF: limit"));
    assert!(prompt.contains("[seq 2 · limit] five_hour rejected"));
    assert!(prompt.contains("PREVIOUS CAPSULE (covers events up to seq 0"));
}
