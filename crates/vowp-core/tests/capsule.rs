//! Capsule writer tests: prompt rendering, JSON-repair, debounce, compaction and persistence.
//! Spec: `docs/design/sessions.md` §3 and `docs/design/crates/vowp-core.md` §3 named tests.

mod support;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::capsule::CapsuleWriter;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, Capsule, CapsuleId, CapsuleState,
    CapsuleTrigger, Cost, EventBody, EventEnvelope, EventSource, Executor, ExecutorRef, FakeClock, HarnessEvent,
    HarnessKind, LimitEvent, LimitKind, MemoryLog, NodeName, ProbeResult, RunOutcome, RunSummary, SessionCreated,
    SessionId, SessionRole, StartInput, StopReason, Timestamp, ToolCall, ToolResult, TurnEnded,
};
use vowp_store::session_files::list_capsule_files;
use vowp_store::session_log::SessionLog;
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
