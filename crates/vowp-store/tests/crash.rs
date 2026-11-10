//! Torn-tail, corruption and reopen tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §4.1 rules 5–6 and §6
//! (`log_torn_tail_recovery`, `log_corrupt_middle_is_error_not_repair`,
//! `log_survives_reopen_proptest`).

use std::sync::Arc;

use proptest::prelude::*;
use tempfile::TempDir;
use vowp_protocol::{Actor, EventBody, EventEnvelope, EventSource, FakeClock, HarnessKind, Note, SessionId, Timestamp};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, StoreError};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A temporary node root with a controllable clock.
fn node() -> (TempDir, NodePaths, FakeClock) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
    (tmp, paths, clock)
}

/// A `note` event with the given text.
fn note(text: &str) -> NewEvent {
    NewEvent {
        actor: Actor::System,
        source: EventSource {
            harness: HarnessKind::Vowp,
            model: None,
            node: None,
            native_session_id: None,
        },
        body: EventBody::Note(Note { text: text.to_owned() }),
        raw: None,
        ts: None,
    }
}

/// Writes `count` events and returns the log plus each event's end offset.
fn write_events(paths: &NodePaths, id: &SessionId, clock: &FakeClock, count: usize) -> Vec<u64> {
    let log = SessionLog::create(paths, id, Arc::new(clock.clone())).expect("create");
    let mut ends = Vec::new();
    for index in 0..count {
        let event = log.append(note(&format!("event {index}"))).expect("append");
        ends.push(event.byte_offset + event.byte_len);
    }
    drop(log);
    ends
}

#[test]
fn log_torn_tail_recovery() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let ends = write_events(&paths, &id, &clock, 4);
    let log_path = paths.log_path(&id);
    let full_len = std::fs::metadata(&log_path).expect("metadata").len();
    let good_end = ends[2];
    assert!(full_len - good_end > 25, "the final line is long enough to cut");

    // Cut 25 bytes off the final line: the remaining fragment is what open truncates.
    let remaining_fragment = full_len - 25 - good_end;
    let file = std::fs::OpenOptions::new().write(true).open(&log_path).expect("open");
    file.set_len(full_len - 25).expect("truncate mid-line");
    drop(file);

    let log = SessionLog::open(&paths, &id, Arc::new(clock.clone())).expect("open repairs the torn tail");
    assert_eq!(log.open_report().torn_tail_bytes, remaining_fragment);
    assert_eq!(log.open_report().events_found, 3);
    assert_eq!(log.last_seq(), 3);
    assert_eq!(log.byte_len(), good_end);
    assert_eq!(
        std::fs::metadata(&log_path).expect("metadata").len(),
        good_end,
        "the file is truncated to the end of the last good line"
    );

    let next = log.append(note("after recovery")).expect("append after repair");
    assert_eq!(next.envelope.seq, 4);
    log.sync().expect("sync");
    drop(log);

    let report = SessionLog::open_read(&paths, &id)
        .expect("reader")
        .verify()
        .expect("verify");
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.last_seq, 4);
}

#[test]
fn log_corrupt_middle_is_error_not_repair() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let ends = write_events(&paths, &id, &clock, 4);
    let log_path = paths.log_path(&id);

    // Flip the opening brace of line 2, which makes the line invalid JSON but leaves a
    // perfectly parsable line 3 after it.
    let line_two_start = ends[0];
    let mut bytes = std::fs::read(&log_path).expect("read log");
    assert_eq!(bytes[line_two_start as usize], b'{');
    bytes[line_two_start as usize] = b'#';
    std::fs::write(&log_path, &bytes).expect("write corruption");

    match SessionLog::open(&paths, &id, Arc::new(clock.clone())) {
        Err(StoreError::Corrupt { line, .. }) => assert_eq!(line, Some(2)),
        Ok(_) => panic!("a corrupt middle line must not open"),
        Err(err) => panic!("expected Corrupt on line 2, got {err}"),
    }
    assert_eq!(
        std::fs::read(&log_path).expect("read after failed open"),
        bytes,
        "a corrupt middle line is never repaired"
    );

    let report = SessionLog::open_read(&paths, &id)
        .expect("reader")
        .verify()
        .expect("verify");
    assert!(!report.is_ok());
    assert!(
        report.problems.iter().any(|problem| problem.contains("line 2")),
        "{:?}",
        report.problems
    );
    assert_eq!(report.torn_tail_bytes, 0, "the bad line is not the tail");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn log_survives_reopen_proptest(ops in prop::collection::vec((0u8..5, "[a-zA-Z0-9 ]{0,20}"), 0..40)) {
        let (_tmp, paths, clock) = node();
        let id = SessionId::new();
        let mut log = Some(SessionLog::create(&paths, &id, Arc::new(clock.clone())).expect("create"));
        let mut model: Vec<EventEnvelope> = Vec::new();

        for (op, text) in ops {
            match op {
                0..=2 => {
                    let writer = log.as_ref().expect("writer is open");
                    let appended = writer.append(note(&text)).expect("append");
                    model.push(appended.envelope);
                }
                3 => {
                    log.as_ref().expect("writer is open").sync().expect("sync");
                }
                _ => {
                    drop(log.take());
                    let reopened = SessionLog::open(&paths, &id, Arc::new(clock.clone())).expect("reopen");
                    prop_assert_eq!(reopened.open_report().torn_tail_bytes, 0);
                    log = Some(reopened);
                }
            }
        }

        let writer = match log {
            Some(writer) => writer,
            None => SessionLog::open(&paths, &id, Arc::new(clock.clone())).expect("final reopen"),
        };
        writer.sync().expect("final sync");
        drop(writer);

        let stored = SessionLog::open_read(&paths, &id)
            .expect("reader")
            .read_all()
            .expect("read all");
        prop_assert_eq!(&stored, &model);
        for (index, event) in stored.iter().enumerate() {
            prop_assert_eq!(event.seq, index as u64 + 1);
        }
    }
}
