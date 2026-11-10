//! Session log, capsule file and adapters.json tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6 (`log_append_assigns_seq_and_ts`,
//! `log_rejects_gap_and_duplicate_envelope`, `log_fsync_markers`,
//! `log_lock_blocks_second_writer`, `log_reader_tail_and_read_from`,
//! `log_big_tool_result_roundtrip`, `capsule_file_roundtrip`, `adapters_json_is_append_only`).

use std::sync::Arc;

use tempfile::TempDir;
use vowp_protocol::{
    Actor, CAPSULE_VERSION, Capsule, CapsuleDecision, CapsuleState, CapsuleTelemetry, CapsuleTrigger, Clock, EventBody,
    EventEnvelope, EventSource, Executor, ExecutorRef, FakeClock, FileRole, HarnessKind, NodeName, Note, SessionId,
    Timestamp, ToolResult, UserMessage,
};
use vowp_store::session_files::{
    AdapterMapping, add_adapter_mapping, read_adapters_json, read_capsule_file, write_capsule_file,
};
use vowp_store::session_log::{NewEvent, SessionLog, VerifyReport};
use vowp_store::{NodePaths, StoreError};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A temporary node root and a controllable clock.
fn node() -> (TempDir, NodePaths, FakeClock) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
    (tmp, paths, clock)
}

/// The event source used by tests.
fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::Vowp,
        model: None,
        node: None,
        native_session_id: None,
    }
}

/// A `note` event with the given text.
fn note(text: &str) -> NewEvent {
    NewEvent {
        actor: Actor::System,
        source: source(),
        body: EventBody::Note(Note { text: text.to_owned() }),
        raw: None,
        ts: None,
    }
}

/// A pre-built note envelope with an explicit sequence number.
fn envelope(session: &SessionId, seq: u64, text: &str) -> EventEnvelope {
    EventEnvelope::new(
        session.clone(),
        seq,
        Timestamp::parse(EPOCH).expect("epoch"),
        Actor::System,
        source(),
        &EventBody::Note(Note { text: text.to_owned() }),
    )
}

#[test]
fn log_append_assigns_seq_and_ts() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock.clone())).expect("create");

    let mut appended = Vec::new();
    for index in 0..5 {
        clock.advance(std::time::Duration::from_secs(1));
        let event = log.append(note(&format!("event {index}"))).expect("append");
        assert_eq!(event.envelope.seq, index + 1);
        assert_eq!(event.envelope.ts, clock.now(), "ts comes from the injected clock");
        assert_eq!(event.envelope.session_id, id);
        assert_eq!(
            event.byte_offset,
            appended
                .last()
                .map_or(0, |last: &vowp_store::session_log::AppendedEvent| last.byte_offset
                    + last.byte_len)
        );
        appended.push(event);
    }

    let ids = appended
        .iter()
        .map(|event| event.envelope.id.clone())
        .collect::<Vec<_>>();
    for pair in ids.windows(2) {
        assert!(pair[0] < pair[1], "event ids are unique and increasing");
    }

    let total: u64 = appended.iter().map(|event| event.byte_len).sum();
    assert_eq!(log.byte_len(), total);
    assert_eq!(log.last_seq(), 5);
    assert_eq!(
        std::fs::metadata(paths.log_path(&id)).expect("metadata").len(),
        total,
        "line offsets add up to the file length"
    );
    let stored = vowp_store::session_log::SessionLog::open_read(&paths, &id)
        .expect("reader")
        .read_all()
        .expect("read");
    assert_eq!(stored.len(), 5);
    assert_eq!(
        stored,
        appended.into_iter().map(|event| event.envelope).collect::<Vec<_>>()
    );
}

#[test]
fn log_rejects_gap_and_duplicate_envelope() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create");

    let first = envelope(&id, 1, "one");
    log.append_envelope(&first).expect("append seq 1");
    let length = log.byte_len();

    let third = envelope(&id, 3, "three");
    match log.append_envelope(&third) {
        Err(StoreError::SeqGap { expected, got, .. }) => {
            assert_eq!((expected, got), (2, 3));
        }
        other => panic!("expected SeqGap, got {other:?}"),
    }

    match log.append_envelope(&first) {
        Err(StoreError::SeqGap { expected, got, .. }) => {
            assert_eq!((expected, got), (2, 1));
        }
        other => panic!("expected SeqGap, got {other:?}"),
    }

    let other_session = SessionId::new();
    let foreign = EventEnvelope::new(
        other_session.clone(),
        1,
        Timestamp::parse(EPOCH).expect("epoch"),
        Actor::System,
        source(),
        &EventBody::Note(Note {
            text: "foreign".to_owned(),
        }),
    );
    match log.append_envelope(&foreign) {
        Err(StoreError::SessionMismatch { expected, got }) => {
            assert_eq!(expected, id.to_string());
            assert_eq!(got, other_session.to_string());
        }
        other => panic!("expected SessionMismatch, got {other:?}"),
    }

    assert_eq!(log.byte_len(), length, "rejected envelopes write nothing");
    assert_eq!(log.last_seq(), 1);
}

#[test]
fn log_fsync_markers() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create");

    assert_eq!(log.sync_count(), 0);
    log.sync().expect("sync clean log");
    assert_eq!(log.sync_count(), 0, "a clean log is not fsynced");

    log.append(note("dirty")).expect("append");
    log.sync().expect("sync dirty log");
    assert_eq!(log.sync_count(), 1, "sync clears the dirty flag with one fsync");
    log.sync().expect("second sync");
    assert_eq!(log.sync_count(), 1);

    log.append(note("dirty again")).expect("append");
    log.sync().expect("sync again");
    assert_eq!(log.sync_count(), 2);
}

#[test]
fn log_lock_blocks_second_writer() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock.clone())).expect("create");
    log.append(note("locked")).expect("append");

    match SessionLog::open(&paths, &id, Arc::new(clock.clone())) {
        Err(StoreError::Locked { pid }) => assert_eq!(pid, std::process::id()),
        Ok(_) => panic!("a second writer must not acquire the lock"),
        Err(err) => panic!("expected Locked, got {err}"),
    }

    drop(log);
    assert!(!paths.lock_path(&id).exists(), "drop releases the lock file");
    let reopened = SessionLog::open(&paths, &id, Arc::new(clock.clone())).expect("reopen after drop");
    drop(reopened);

    std::fs::write(paths.lock_path(&id), "999999\n0\n").expect("write stale lock");
    let log = SessionLog::open(&paths, &id, Arc::new(clock)).expect("stale lock is replaced");
    let contents = std::fs::read_to_string(paths.lock_path(&id)).expect("read lock");
    assert_eq!(
        contents.lines().next(),
        Some(std::process::id().to_string().as_str()),
        "lock now names this process"
    );
    drop(log);
    assert!(!paths.lock_path(&id).exists());
}

#[test]
fn log_reader_tail_and_read_from() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create");
    let mut appended = Vec::new();
    for index in 1..=5 {
        appended.push(log.append(note(&format!("event {index}"))).expect("append"));
    }
    drop(log);

    let reader = SessionLog::open_read(&paths, &id).expect("reader");
    let tail = reader.tail(2).expect("tail");
    assert_eq!(tail.iter().map(|event| event.seq).collect::<Vec<_>>(), vec![4, 5]);

    let from = reader.read_from(3, Some(2)).expect("read_from");
    assert_eq!(from, vec![appended[3].envelope.clone(), appended[4].envelope.clone()]);
    assert_eq!(reader.read_from(4, None).expect("unbounded").len(), 1);
    assert!(reader.read_from(5, None).expect("after last").is_empty());

    let second = &appended[1];
    let at = reader.read_at(second.byte_offset, second.byte_len).expect("read_at");
    assert_eq!(at, second.envelope);

    let offsets = reader
        .iter()
        .expect("iter")
        .map(|event| event.expect("event").byte_offset)
        .collect::<Vec<_>>();
    assert_eq!(
        offsets,
        appended.iter().map(|event| event.byte_offset).collect::<Vec<_>>()
    );
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn log_big_tool_result_roundtrip() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create");

    let output = "big output line\n".repeat(7000);
    assert!(output.len() > 100_000);
    log.append(NewEvent {
        actor: Actor::Tool,
        source: source(),
        body: EventBody::ToolResult(ToolResult {
            call_id: "big".to_owned(),
            ok: true,
            output: Some(output.clone()),
            output_ref: None,
            exit_code: None,
        }),
        raw: None,
        ts: None,
    })
    .expect("append big result");
    drop(log);

    let reader = SessionLog::open_read(&paths, &id).expect("reader");
    let events = reader.read_all().expect("read");
    let EventBody::ToolResult(result) = events[0].body().expect("body") else {
        panic!("expected tool.result");
    };
    assert!(result.output.is_none());
    let blob_ref = result.output_ref.expect("spilled ref");
    assert_eq!(blob_ref.bytes, output.len() as u64);
    let bytes = reader.blobs().get(&blob_ref.sha256).expect("blob");
    assert_eq!(String::from_utf8(bytes).expect("utf8"), output);
}

/// Builds a valid capsule for `session` at `at_seq`.
fn capsule(session: &SessionId, at_seq: u64) -> Capsule {
    Capsule {
        version: CAPSULE_VERSION,
        capsule_id: vowp_protocol::CapsuleId::new(),
        session_id: session.clone(),
        created_at: Timestamp::parse(EPOCH).expect("epoch"),
        at_seq,
        trigger: CapsuleTrigger::Manual,
        from: ExecutorRef {
            executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
            node: Some(NodeName::parse("local").expect("node")),
        },
        intent: "finish the job".to_owned(),
        constraints: vec!["stay hermetic".to_owned()],
        decisions: vec![CapsuleDecision {
            what: "use the store".to_owned(),
            why: "it is the only writer".to_owned(),
            at_seq: Some(at_seq),
        }],
        state: CapsuleState {
            done: vec!["wrote the log".to_owned()],
            in_progress: vec!["wrote the capsule".to_owned()],
            blocked: Vec::new(),
            next_steps: vec!["run the tests".to_owned()],
        },
        files: vec![vowp_protocol::CapsuleFile {
            path: "src/lib.rs".to_owned(),
            role: FileRole::Modified,
            note: "module list".to_owned(),
        }],
        gotchas: vec!["seq must stay dense".to_owned()],
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry {
            turns: 1,
            tokens_in: 10,
            tokens_out: 20,
            usd: Some(0.25),
        },
    }
}

#[test]
fn capsule_file_roundtrip() {
    let (tmp, paths, _clock) = node();
    let id = SessionId::new();
    let first_capsule = capsule(&id, 42);

    let path = write_capsule_file(&paths, &first_capsule).expect("write capsule");
    assert!(
        path.file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with(&format!("{:010}", 42)),
        "file name starts with the zero-padded at_seq: {}",
        path.display()
    );
    let read = read_capsule_file(&path).expect("read capsule");
    assert_eq!(read, first_capsule);

    let second = capsule(&id, 43);
    let second_path = write_capsule_file(&paths, &second).expect("write second capsule");
    let listed = vowp_store::session_files::list_capsule_files(&paths, &id).expect("list");
    assert_eq!(listed, vec![path.clone(), second_path]);
    assert!(
        tmp.path().exists(),
        "the sandbox root is the temporary directory, never the real home"
    );

    let bad = tmp.path().join("capsules").join("bad.json");
    std::fs::create_dir_all(bad.parent().expect("parent")).expect("dir");
    std::fs::write(&bad, b"{\"not\": \"a capsule\"}").expect("write bad");
    match read_capsule_file(&bad) {
        Err(StoreError::Corrupt { .. }) | Err(StoreError::Protocol(_)) | Err(StoreError::Json(_)) => {}
        other => panic!("invalid capsule JSON must be rejected, got {other:?}"),
    }
}

#[test]
fn adapters_json_is_append_only() {
    let (_tmp, paths, _clock) = node();
    let id = SessionId::new();
    let node_name = NodeName::parse("local").expect("node");

    assert!(matches!(
        read_adapters_json(&paths, &id),
        Ok(map) if map.natives.is_empty()
    ));

    let first = AdapterMapping {
        harness: HarnessKind::ClaudeCode,
        native_session_id: "claude-1".to_owned(),
        node: node_name.clone(),
        first_seq: 1,
        created_at: Timestamp::parse(EPOCH).expect("epoch"),
    };
    let second = AdapterMapping {
        harness: HarnessKind::Opencode,
        native_session_id: "opencode-1".to_owned(),
        node: node_name,
        first_seq: 7,
        created_at: Timestamp::parse("2026-01-01T00:00:01.000Z").expect("ts"),
    };

    add_adapter_mapping(&paths, &id, first.clone()).expect("add first");
    add_adapter_mapping(&paths, &id, second.clone()).expect("add second");
    add_adapter_mapping(&paths, &id, first.clone()).expect("re-add first is a no-op");

    let map = read_adapters_json(&paths, &id).expect("read");
    assert_eq!(map.natives, vec![first, second], "append order is preserved");
}

#[test]
fn verify_reports_clean_log() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create");
    log.append(note("one")).expect("append");
    log.append(NewEvent {
        actor: Actor::User,
        source: source(),
        body: EventBody::UserMessage(UserMessage {
            text: "hello".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
        raw: None,
        ts: None,
    })
    .expect("append");
    drop(log);

    let report: VerifyReport = SessionLog::open_read(&paths, &id)
        .expect("reader")
        .verify()
        .expect("verify");
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.lines, 2);
    assert_eq!(report.last_seq, 2);
    assert_eq!(report.torn_tail_bytes, 0);
}
