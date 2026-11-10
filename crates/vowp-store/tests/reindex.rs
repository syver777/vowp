//! Reindex tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §4.3 and §6
//! (`reindex_after_deleting_db_restores_index`, `reindex_preserves_switch_capsule_links`,
//! `reindex_restores_user_session_role`, `reindex_is_idempotent`,
//! `reindex_single_session_leaves_others`, `reindex_skips_corrupt_session`).

use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use rusqlite::types::Value;
use tempfile::TempDir;
use vowp_protocol::{
    Actor, CAPSULE_VERSION, Capsule, CapsuleState, CapsuleTelemetry, CapsuleTrigger, EventBody, EventSource, Executor,
    ExecutorRef, FakeClock, FileRole, HarnessKind, NodeName, Note, SessionCreated, SessionExecutor, SessionId,
    SessionRole, SwitchEvent, SwitchId, SwitchReason, Timestamp,
};
use vowp_store::reindex::reindex;
use vowp_store::session_files::{AdapterMapping, add_adapter_mapping, write_capsule_file};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A temporary node root with a controllable clock.
fn node() -> (TempDir, NodePaths, FakeClock) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
    (tmp, paths, clock)
}

/// The VOWP event source used by tests.
fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::Vowp,
        model: None,
        node: None,
        native_session_id: None,
    }
}

/// A `note` event.
fn note(text: &str) -> NewEvent {
    NewEvent {
        actor: Actor::System,
        source: source(),
        body: EventBody::Note(Note { text: text.to_owned() }),
        raw: None,
        ts: None,
    }
}

/// A `session.created` event.
fn session_created(title: &str, role: Option<SessionRole>) -> NewEvent {
    NewEvent {
        actor: Actor::System,
        source: source(),
        body: EventBody::SessionCreated(SessionCreated {
            title: title.to_owned(),
            cwd: "/home/user/project".to_owned(),
            project: None,
            job_id: None,
            role,
            parent_session_id: None,
        }),
        raw: None,
        ts: None,
    }
}

/// A `session.executor` attach event.
fn executor_attach(native_session_id: Option<&str>) -> NewEvent {
    NewEvent {
        actor: Actor::System,
        source: source(),
        body: EventBody::SessionExecutor(SessionExecutor {
            action: vowp_protocol::AttachAction::Attach,
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor"),
            node: NodeName::parse("local").expect("node"),
            native_session_id: native_session_id.map(str::to_owned),
            run_id: None,
            reason: None,
        }),
        raw: None,
        ts: None,
    }
}

/// Creates a session whose log has three events: created, executor attach and a note.
fn create_session(
    paths: &NodePaths,
    clock: &FakeClock,
    title: &str,
    role: Option<SessionRole>,
    native_session_id: Option<&str>,
) -> SessionId {
    let id = SessionId::new();
    let log = SessionLog::create(paths, &id, Arc::new(clock.clone())).expect("create log");
    log.append(session_created(title, role)).expect("session.created");
    log.append(executor_attach(native_session_id))
        .expect("session.executor");
    log.append(note("progress")).expect("note");
    drop(log);
    id
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
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor"),
            node: Some(NodeName::parse("local").expect("node")),
        },
        intent: "finish the job".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState {
            done: vec!["indexed".to_owned()],
            in_progress: Vec::new(),
            blocked: Vec::new(),
            next_steps: vec!["verify".to_owned()],
        },
        files: vec![vowp_protocol::CapsuleFile {
            path: "src/reindex.rs".to_owned(),
            role: FileRole::Modified,
            note: "rebuilt".to_owned(),
        }],
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry {
            turns: 1,
            tokens_in: 10,
            tokens_out: 5,
            usd: Some(0.1),
        },
    }
}

/// The index tables a reindex rebuilds.
#[derive(Debug, PartialEq)]
struct Snapshot {
    sessions: Vec<Vec<Value>>,
    events: Vec<Vec<Value>>,
    natives: Vec<Vec<Value>>,
    capsules: Vec<Vec<Value>>,
    switches: Vec<Vec<Value>>,
}

impl Snapshot {
    /// Returns the rows belonging to `id` from every table.
    fn of_session(&self, id: &SessionId) -> [Vec<Vec<Value>>; 5] {
        [
            filter_rows(&self.sessions, 0, id),
            filter_rows(&self.events, 0, id),
            filter_rows(&self.natives, 0, id),
            filter_rows(&self.capsules, 1, id),
            filter_rows(&self.switches, 1, id),
        ]
    }
}

/// Keeps rows whose text column `column` equals `id`.
fn filter_rows(rows: &[Vec<Value>], column: usize, id: &SessionId) -> Vec<Vec<Value>> {
    rows.iter()
        .filter(|row| match row.get(column) {
            Some(Value::Text(value)) => value == id.as_str(),
            _ => false,
        })
        .cloned()
        .collect()
}

/// Reads the index tables with a second connection.
fn snapshot(paths: &NodePaths) -> Snapshot {
    let conn = Connection::open(paths.db_path()).expect("open db for snapshot");
    Snapshot {
        sessions: rows(
            &conn,
            "SELECT id, project, job_id, role, title, parent_session_id, node, cwd, executor, last_seq,
                    last_capsule_id, log_bytes, created_at, updated_at
             FROM sessions ORDER BY id",
        ),
        events: rows(
            &conn,
            "SELECT session_id, seq, id, ts, type, actor, harness, model, byte_offset, byte_len, summary
             FROM session_events ORDER BY session_id, seq",
        ),
        natives: rows(
            &conn,
            "SELECT session_id, harness, native_session_id, node, first_seq, created_at
             FROM session_natives ORDER BY session_id, harness, native_session_id",
        ),
        capsules: rows(
            &conn,
            "SELECT id, session_id, at_seq, version, trigger, author, path, body_json, created_at
             FROM capsules ORDER BY id",
        ),
        switches: rows(
            &conn,
            "SELECT id, session_id, from_executor, to_executor, from_node, to_node, capsule_id, reason, state,
                    error, started_at, ended_at, at_seq
             FROM switches ORDER BY id",
        ),
    }
}

/// Runs a query and returns every column as a SQLite value.
fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = conn.prepare(sql).expect("prepare snapshot query");
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            let mut values = Vec::with_capacity(columns);
            for index in 0..columns {
                values.push(row.get::<_, Value>(index)?);
            }
            Ok(values)
        })
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect rows")
}

/// Deletes the database file so the next `Store::open` starts empty.
fn delete_db(paths: &NodePaths) {
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{}{suffix}", paths.db_path().display());
        if Path::new(&path).exists() {
            std::fs::remove_file(path).expect("remove database file");
        }
    }
}

#[test]
fn reindex_after_deleting_db_restores_index() {
    let (tmp, paths, clock) = node();
    let alpha = create_session(&paths, &clock, "alpha", Some(SessionRole::User), Some("native-alpha"));
    let beta = create_session(&paths, &clock, "beta", Some(SessionRole::Planner), Some("native-beta"));
    let gamma = create_session(&paths, &clock, "gamma", Some(SessionRole::Worker), None);

    let alpha_capsule = capsule(&alpha, 3);
    let capsule_path = write_capsule_file(&paths, &alpha_capsule).expect("write capsule file");
    add_adapter_mapping(
        &paths,
        &beta,
        AdapterMapping {
            harness: HarnessKind::Opencode,
            native_session_id: "native-beta-opencode".to_owned(),
            node: NodeName::parse("local").expect("node"),
            first_seq: 2,
            created_at: Timestamp::parse(EPOCH).expect("epoch"),
        },
    )
    .expect("write adapters.json");

    let store = Store::open(&paths).expect("open store");
    let report = reindex(&store, &paths, None).expect("first reindex");
    assert_eq!(report.sessions, 3);
    assert_eq!(report.events, 9, "three events per session");
    assert_eq!(report.capsules, 1);
    assert_eq!(report.problems, Vec::<String>::new());
    let before = snapshot(&paths);

    drop(store);
    delete_db(&paths);
    let store = Store::open(&paths).expect("reopen empty store");
    let report = reindex(&store, &paths, None).expect("reindex after deletion");
    assert_eq!(report.sessions, 3);
    assert_eq!(report.events, 9);
    assert_eq!(report.capsules, 1);
    let after = snapshot(&paths);

    assert_eq!(before.sessions, after.sessions, "sessions rows are restored");
    assert_eq!(before.events, after.events, "session_events rows are restored");
    assert_eq!(before.natives, after.natives, "session_natives rows are restored");
    assert_eq!(before.capsules, after.capsules, "capsules rows are restored");

    let conn = Connection::open(paths.db_path()).expect("inspect");
    let title: String = conn
        .query_row("SELECT title FROM sessions WHERE id = ?1", [alpha.as_str()], |row| {
            row.get(0)
        })
        .expect("alpha row");
    assert_eq!(title, "alpha");
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_events", [], |row| row.get(0))
        .expect("count events");
    assert_eq!(events, 9);
    let last_seq: i64 = conn
        .query_row("SELECT last_seq FROM sessions WHERE id = ?1", [gamma.as_str()], |row| {
            row.get(0)
        })
        .expect("gamma row");
    assert_eq!(last_seq, 3);
    assert!(capsule_path.is_file());
    drop(conn);

    assert!(tmp.path().join("vowp.db").is_file());
}

#[test]
fn reindex_preserves_switch_capsule_links() {
    let (_tmp, paths, clock) = node();
    let id = SessionId::new();
    let log = SessionLog::create(&paths, &id, Arc::new(clock.clone())).expect("create log");
    log.append(session_created("switchy", None)).expect("created");
    log.append(executor_attach(Some("native-1"))).expect("attach");
    let capsule = capsule(&id, 3);
    write_capsule_file(&paths, &capsule).expect("capsule file");
    let switch = SwitchEvent {
        switch_id: SwitchId::new(),
        from: Some(ExecutorRef {
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("from"),
            node: Some(NodeName::parse("local").expect("node")),
        }),
        to: ExecutorRef {
            executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("to"),
            node: Some(NodeName::parse("local").expect("node")),
        },
        capsule_id: Some(capsule.capsule_id.clone()),
        reason: SwitchReason::User,
    };
    log.append(NewEvent {
        actor: Actor::System,
        source: source(),
        body: EventBody::Switch(switch.clone()),
        raw: None,
        ts: None,
    })
    .expect("switch event");
    log.append(note("after switch")).expect("note");
    drop(log);

    let store = Store::open(&paths).expect("open store");
    let report = reindex(&store, &paths, None).expect("first reindex");
    assert_eq!(report.capsules, 1);
    let conn = Connection::open(paths.db_path()).expect("inspect");
    let (state, linked): (String, Option<String>) = conn
        .query_row(
            "SELECT state, capsule_id FROM switches WHERE id = ?1",
            [switch.switch_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("switch row");
    assert_eq!(state, "completed");
    assert_eq!(linked.as_deref(), Some(capsule.capsule_id.as_str()));
    let switches: i64 = conn
        .query_row("SELECT COUNT(*) FROM switches", [], |row| row.get(0))
        .expect("count switches");
    assert_eq!(switches, 1);
    drop(conn);

    let second = reindex(&store, &paths, None).expect("second reindex");
    assert_eq!(second.capsules, 1);
    let conn = Connection::open(paths.db_path()).expect("inspect again");
    let linked: Option<String> = conn
        .query_row(
            "SELECT capsule_id FROM switches WHERE id = ?1",
            [switch.switch_id.as_str()],
            |row| row.get(0),
        )
        .expect("switch row after second reindex");
    assert_eq!(
        linked.as_deref(),
        Some(capsule.capsule_id.as_str()),
        "reindex never deletes capsules, so the switch link survives"
    );
    let capsules: i64 = conn
        .query_row("SELECT COUNT(*) FROM capsules", [], |row| row.get(0))
        .expect("count capsules");
    assert_eq!(capsules, 1);
    drop(conn);

    // After losing the database entirely the switch row is recreated from the log event,
    // and its capsule link is restored because capsule files are indexed first.
    drop(store);
    delete_db(&paths);
    let store = Store::open(&paths).expect("reopen empty store");
    let report = reindex(&store, &paths, None).expect("reindex after deletion");
    assert_eq!(report.sessions, 1);
    assert_eq!(report.capsules, 1);
    let conn = Connection::open(paths.db_path()).expect("inspect restored switches");
    let (state, linked, at_seq): (String, Option<String>, i64) = conn
        .query_row(
            "SELECT state, capsule_id, at_seq FROM switches WHERE id = ?1",
            [switch.switch_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("restored switch row");
    assert_eq!(state, "completed");
    assert_eq!(linked.as_deref(), Some(capsule.capsule_id.as_str()));
    assert_eq!(at_seq, 3);
}

#[test]
fn reindex_restores_user_session_role() {
    let (_tmp, paths, clock) = node();
    let id = create_session(
        &paths,
        &clock,
        "user session",
        Some(SessionRole::User),
        Some("native-user"),
    );

    let store = Store::open(&paths).expect("open store");
    reindex(&store, &paths, None).expect("first reindex");
    drop(store);
    delete_db(&paths);

    let store = Store::open(&paths).expect("reopen empty store");
    reindex(&store, &paths, None).expect("reindex after deletion");
    let conn = Connection::open(paths.db_path()).expect("inspect");
    let role: Option<String> = conn
        .query_row("SELECT role FROM sessions WHERE id = ?1", [id.as_str()], |row| {
            row.get(0)
        })
        .expect("session row");
    assert_eq!(role.as_deref(), Some("user"));
}

#[test]
fn reindex_is_idempotent() {
    let (_tmp, paths, clock) = node();
    let alpha = create_session(&paths, &clock, "alpha", Some(SessionRole::User), Some("native-alpha"));
    let beta = create_session(&paths, &clock, "beta", Some(SessionRole::Planner), Some("native-beta"));
    let capsule = capsule(&alpha, 3);
    write_capsule_file(&paths, &capsule).expect("capsule file");
    add_adapter_mapping(
        &paths,
        &beta,
        AdapterMapping {
            harness: HarnessKind::ClaudeCode,
            native_session_id: "adapter-native".to_owned(),
            node: NodeName::parse("local").expect("node"),
            first_seq: 2,
            created_at: Timestamp::parse(EPOCH).expect("epoch"),
        },
    )
    .expect("adapters.json");

    let store = Store::open(&paths).expect("open store");
    let first = reindex(&store, &paths, None).expect("first reindex");
    let snapshot_one = snapshot(&paths);
    let second = reindex(&store, &paths, None).expect("second reindex");
    let snapshot_two = snapshot(&paths);

    assert_eq!(first, second);
    assert_eq!(snapshot_one, snapshot_two);
    assert_eq!(second.sessions, 2);
    assert_eq!(second.events, 6);
    assert_eq!(second.capsules, 1);
}

#[test]
fn reindex_single_session_leaves_others() {
    let (_tmp, paths, clock) = node();
    let alpha = create_session(&paths, &clock, "alpha", Some(SessionRole::Worker), Some("native-alpha"));
    let beta = create_session(&paths, &clock, "beta", Some(SessionRole::Worker), Some("native-beta"));

    let store = Store::open(&paths).expect("open store");
    reindex(&store, &paths, None).expect("initial reindex");
    let before = snapshot(&paths);
    let beta_before = before.of_session(&beta);

    {
        let log = SessionLog::open(&paths, &alpha, Arc::new(clock.clone())).expect("reopen alpha");
        log.append(note("one more")).expect("append");
        drop(log);
    }
    let report = reindex(&store, &paths, Some(&alpha)).expect("reindex alpha only");
    assert_eq!(report.sessions, 1);
    assert_eq!(report.events, 4);

    let after = snapshot(&paths);
    assert_eq!(after.of_session(&beta), beta_before, "beta is untouched");
    assert_eq!(filter_rows(&after.events, 0, &alpha).len(), 4, "alpha is updated");

    let conn = Connection::open(paths.db_path()).expect("inspect");
    let alpha_last: i64 = conn
        .query_row("SELECT last_seq FROM sessions WHERE id = ?1", [alpha.as_str()], |row| {
            row.get(0)
        })
        .expect("alpha row");
    let beta_last: i64 = conn
        .query_row("SELECT last_seq FROM sessions WHERE id = ?1", [beta.as_str()], |row| {
            row.get(0)
        })
        .expect("beta row");
    assert_eq!(alpha_last, 4);
    assert_eq!(beta_last, 3);
}

#[test]
fn reindex_skips_corrupt_session() {
    let (_tmp, paths, clock) = node();
    let good = create_session(&paths, &clock, "good", Some(SessionRole::Worker), Some("native-good"));
    let corrupt = create_session(&paths, &clock, "corrupt", Some(SessionRole::Worker), Some("native-bad"));

    let log_path = paths.log_path(&corrupt);
    let mut bytes = std::fs::read(&log_path).expect("read corrupt log");
    // Find the start of the second line and break its opening brace.
    let first_newline = bytes.iter().position(|byte| *byte == b'\n').expect("newline") + 1;
    bytes[first_newline] = b'#';
    std::fs::write(&log_path, bytes).expect("write corruption");

    let store = Store::open(&paths).expect("open store");
    let report = reindex(&store, &paths, None).expect("reindex");
    assert_eq!(report.sessions, 1);
    assert_eq!(report.events, 3);
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains(corrupt.as_str()) && problem.contains("invalid JSON")),
        "{:?}",
        report.problems
    );

    let conn = Connection::open(paths.db_path()).expect("inspect");
    let good_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_events WHERE session_id = ?1",
            [good.as_str()],
            |row| row.get(0),
        )
        .expect("good events");
    assert_eq!(good_rows, 3);
    let corrupt_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            [corrupt.as_str()],
            |row| row.get(0),
        )
        .expect("corrupt session");
    assert_eq!(corrupt_rows, 0, "the corrupt session is skipped, not half-indexed");
}
