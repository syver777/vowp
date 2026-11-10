//! Blob store and tool-output spill tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6 (`blob_spill_boundary`,
//! `blob_put_is_atomic_and_deduped`).

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use vowp_protocol::{Actor, BlobRef, EventBody, EventSource, FakeClock, HarnessKind, SessionId, Timestamp, ToolResult};
use vowp_store::blobs::BlobStore;
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, StoreError};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A temporary node root with one created session log.
fn session() -> (TempDir, NodePaths, SessionId, SessionLog) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock)).expect("create log");
    (tmp, paths, id, log)
}

/// A `tool.result` event with inline output.
fn tool_result(call_id: &str, output: &str) -> NewEvent {
    NewEvent {
        actor: Actor::Tool,
        source: EventSource {
            harness: HarnessKind::ClaudeCode,
            model: Some("claude-sonnet-5-5".to_owned()),
            node: None,
            native_session_id: None,
        },
        body: EventBody::ToolResult(ToolResult {
            call_id: call_id.to_owned(),
            ok: true,
            output: Some(output.to_owned()),
            output_ref: None,
            exit_code: Some(0),
        }),
        raw: None,
        ts: None,
    }
}

/// Returns the tool-result payload of an envelope.
fn tool_payload(envelope: &vowp_protocol::EventEnvelope) -> ToolResult {
    match envelope.body().expect("typed body") {
        EventBody::ToolResult(result) => result,
        other => panic!("expected tool.result, got {:?}", other.event_type()),
    }
}

#[test]
fn blob_spill_boundary() {
    let (_tmp, paths, id, log) = session();

    let inline = "a".repeat(8192);
    let first = log.append(tool_result("call-inline", &inline)).expect("append inline");
    let first_payload = tool_payload(&first.envelope);
    assert_eq!(
        first_payload.output.as_deref(),
        Some(inline.as_str()),
        "an 8192-byte output stays inline"
    );
    assert!(first_payload.output_ref.is_none());

    let spilled_bytes = "b".repeat(8193);
    let second = log
        .append(tool_result("call-spilled", &spilled_bytes))
        .expect("append spilled");
    let second_payload = tool_payload(&second.envelope);
    assert!(second_payload.output.is_none(), "8193 bytes are spilled");
    let blob_ref: BlobRef = second_payload.output_ref.expect("output_ref");
    assert_eq!(blob_ref.bytes, 8193);
    assert_eq!(blob_ref.preview, "b".repeat(512));
    assert_eq!(blob_ref.sha256.len(), 64);
    assert!(
        paths.blobs_dir(&id).join(&blob_ref.sha256).is_file(),
        "blob file exists at blobs/<sha256>"
    );
    assert_eq!(
        log.blobs().get(&blob_ref.sha256).expect("read blob"),
        spilled_bytes.as_bytes()
    );
}

#[test]
fn blob_put_is_atomic_and_deduped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = PathBuf::from(tmp.path()).join("blobs");
    let store = BlobStore::new(&dir);

    let bytes = b"hello, blob\nsecond line\n";
    let first = store.put(bytes).expect("put once");
    let second = store.put(bytes).expect("put twice");
    assert_eq!(first, second);
    assert!(store.exists(&first.sha256));

    let files = std::fs::read_dir(&dir)
        .expect("read blob dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1, "identical content is stored once: {files:?}");
    assert_eq!(store.get(&first.sha256).expect("get"), bytes);

    // Tampering with the content must be detected by the digest check.
    let path = store.path_of(&first.sha256).expect("valid name");
    std::fs::write(&path, b"tampered").expect("tamper");
    match store.get(&first.sha256) {
        Err(StoreError::Corrupt { reason, .. }) => assert!(reason.contains("digest mismatch"), "{reason}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    for bad in ["../x", "", "not-a-digest", "abc"] {
        match store.path_of(bad) {
            Err(StoreError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput for {bad:?}, got {other:?}"),
        }
    }
    assert!(matches!(store.get("../x"), Err(StoreError::InvalidInput(_))));
    assert!(!store.exists("not-a-digest"));
}
