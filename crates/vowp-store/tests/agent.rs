//! v0.4 local-node enrollment records: migration, credentials, codes, path maps and cards.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.1, §4.2, §6.1, §6.2 and §11.4.

use std::time::Duration;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use vowp_protocol::{
    EventId, HarnessKind, NodeName, NodePresence, NodeStatus, ProjectName, SessionId, Surface, SurfaceId, SurfaceKind,
    SurfaceScope, SurfaceState, Timestamp,
};
use vowp_store::{
    EnrollmentCode, EnrollmentKind, NodeCredential, NodePath, NodePaths, Store, StoreError, SurfaceToken, migrate_v02,
    migrate_v03, migrate_v04,
};

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:10:00.000Z";
const SESSION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const EVENT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

fn node(name: &str) -> NodeName {
    NodeName::parse(name).expect("node")
}

fn project(name: &str) -> ProjectName {
    ProjectName::parse(name).expect("project")
}

/// Builds a real v2 global database: the current schema with the v3 additions removed.
fn v2_home() -> (TempDir, NodePaths) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    paths.ensure().expect("dirs");
    let conn = Connection::open(paths.db_path()).expect("v2 db");
    conn.pragma_update(None, "journal_mode", "WAL").expect("wal");
    conn.execute_batch(include_str!("../src/schema/global.sql"))
        .expect("v3 base schema");
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;
         DROP TABLE node_credentials;
         DROP TABLE enrollment_codes;
         DROP TABLE node_paths;
         DROP TABLE surface_tokens;
         CREATE TABLE surfaces_v2 (
           id            TEXT PRIMARY KEY,
           kind          TEXT NOT NULL CHECK (kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node          TEXT NOT NULL,
           holder        TEXT NOT NULL,
           app           TEXT,
           cwd           TEXT,
           metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
           created_at    TEXT NOT NULL,
           last_seen_at  TEXT NOT NULL
         ) STRICT;
         INSERT INTO surfaces_v2
           (id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at)
         SELECT id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at FROM surfaces;
         DROP TABLE surfaces;
         ALTER TABLE surfaces_v2 RENAME TO surfaces;
         DROP TABLE leases;
         CREATE TABLE leases (
           session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
           surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
           surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node         TEXT NOT NULL,
           holder       TEXT NOT NULL,
           token        TEXT NOT NULL,
           acquired_at  TEXT NOT NULL,
           heartbeat_at TEXT NOT NULL,
           CHECK (length(token) = 52)
         ) STRICT;
         CREATE INDEX leases_heartbeat ON leases(heartbeat_at);
         CREATE TABLE nodes_v2 (
           name         TEXT PRIMARY KEY,
           ssh_target   TEXT,
           is_local     INTEGER NOT NULL DEFAULT 0 CHECK (is_local IN (0,1)),
           capabilities TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(capabilities)),
           last_seen_at TEXT,
           created_at   TEXT NOT NULL,
           CHECK ((is_local = 1 AND ssh_target IS NULL) OR is_local = 0)
         ) STRICT;
         INSERT INTO nodes_v2 (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
           SELECT name, ssh_target, is_local, capabilities, last_seen_at, created_at FROM nodes;
         DROP TABLE nodes;
         ALTER TABLE nodes_v2 RENAME TO nodes;
         CREATE UNIQUE INDEX nodes_one_local ON nodes(is_local) WHERE is_local = 1;
         PRAGMA user_version = 2;",
    )
    .expect("v2 schema");
    conn.execute(
        "INSERT INTO kv (key, value, updated_at) VALUES ('node_id', 'preserved-id', ?1)",
        [T0],
    )
    .expect("kv");
    conn.execute(
        "INSERT INTO nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
         VALUES ('local', NULL, 1, '{\"os\":\"linux\"}', ?1, ?1),
                ('mac-studio', 'owner@mac', 0, '{}', NULL, ?1)",
        [T0],
    )
    .expect("nodes");
    conn.execute(
        "INSERT INTO projects (name, path, remote_url, default_branch, config_json, state, weight,
                               paused_reason, archived_at, created_at, updated_at)
         VALUES ('alpha', '/tmp/alpha', NULL, 'main', '{}', 'active', 1, '', NULL, ?1, ?1)",
        [T0],
    )
    .expect("project");
    conn.execute(
        "INSERT INTO sessions (id, project, job_id, role, title, parent_session_id, node, cwd, state,
                               executor, last_seq, last_capsule_id, log_bytes, compacted_at, created_at, updated_at)
         VALUES (?1, 'alpha', NULL, 'user', 'kept', NULL, 'local', '/tmp', 'idle', NULL, 0, NULL, 0, NULL, ?2, ?2)",
        params![SESSION_ID, T0],
    )
    .expect("session");
    conn.execute(
        "INSERT INTO native_events (session_id, harness, native_id, event_id, at)
         VALUES (?1, 'claude-code', 'native-1', ?2, ?3)",
        params![SESSION_ID, EVENT_ID, T0],
    )
    .expect("native event");
    drop(conn);
    (tmp, paths)
}

#[test]
fn migration_v2_to_v3_preserves_nodes_projects_and_sessions() {
    let (_tmp, paths) = v2_home();
    let source_tables: Vec<String> = {
        let source = Connection::open(paths.db_path()).expect("source db");
        let mut statement = source
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("source tables");
        statement
            .query_map([], |row| row.get(0))
            .expect("source table rows")
            .map(|row| row.expect("source table name"))
            .collect()
    };
    let report = migrate_v02(&paths).expect("migrate");
    assert_eq!(report.from_version, 2);
    assert_eq!(report.to_version, 3);
    assert!(report.verified);
    assert!(std::path::Path::new(&report.backup).is_file(), "backup exists");
    for table in &source_tables {
        assert!(
            report.tables.contains_key(&format!("global:{table}")),
            "the migration did not copy v2 table {table}"
        );
    }
    assert_eq!(report.tables["global:nodes"].before, 2);
    assert_eq!(report.tables["global:nodes"].after, 2);
    assert_eq!(report.tables["global:projects"].before, 1);
    assert_eq!(report.tables["global:sessions"].before, 1);
    assert_eq!(report.tables["global:native_events"].before, 1);
    assert_eq!(report.tables["global:native_events"].after, 1);

    let backup = Connection::open(&report.backup).expect("backup db");
    let backup_version: i64 = backup
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 2, "the backup is the untouched v2 file");
    let backup_events: i64 = backup
        .query_row("SELECT COUNT(*) FROM native_events", [], |row| row.get(0))
        .expect("backup native events");
    assert_eq!(backup_events, 1, "the backup keeps the v2 row");
    drop(backup);

    let store = Store::open(&paths).expect("open migrated home");
    assert_eq!(store.global_schema_version().expect("version"), 7);
    assert_eq!(store.list_nodes().expect("nodes").len(), 2);
    assert_eq!(store.kv_get("node_id").expect("kv"), Some("preserved-id".to_owned()));
    let projects = store.list_projects().expect("projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, project("alpha"));
    assert_eq!(store.list_sessions(&Default::default()).expect("sessions").len(), 1);
    assert_eq!(
        store
            .native_event(
                &SessionId::parse(SESSION_ID).expect("session id"),
                HarnessKind::ClaudeCode,
                "native-1",
            )
            .expect("native event"),
        Some(EventId::parse(EVENT_ID).expect("event id"))
    );
    assert_eq!(store.local_node().expect("local").expect("present").name, node("local"));
    assert_eq!(
        store
            .require_node(&node("mac-studio"))
            .expect("remote")
            .ssh_target
            .as_deref(),
        Some("owner@mac")
    );
}

#[test]
fn local_node_is_home_after_migration() {
    let (_tmp, paths) = v2_home();
    migrate_v02(&paths).expect("migrate");
    let store = Store::open_global(&paths).expect("store");
    let conn = Connection::open(paths.db_path()).expect("inspect");
    for (name, expected) in [("local", "home"), ("mac-studio", "ssh")] {
        let kind: String = conn
            .query_row("SELECT kind FROM nodes WHERE name = ?1", [name], |row| row.get(0))
            .expect("kind");
        assert_eq!(kind, expected, "kind for {name}");
    }
    assert_eq!(store.local_node().expect("local").expect("present").name, node("local"));
    assert_eq!(
        store.require_node(&node("mac-studio")).expect("remote").name,
        node("mac-studio")
    );
}

#[test]
fn enrollment_code_is_single_use() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store
        .insert_pending_node(&node("mac-studio"), &[], ts(T0))
        .expect("pending node");
    let code = EnrollmentCode {
        code_hash: "abc123".to_owned(),
        node: node("mac-studio"),
        kind: EnrollmentKind::Node,
        surface_id: None,
        expires_at: ts(T1),
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: ts(T0),
    };
    store.insert_enrollment_code(&code).expect("code");

    let consumed = store
        .consume_enrollment_code("abc123", ts(T0))
        .expect("consume")
        .expect("first use wins");
    assert_eq!(consumed.code_hash, "abc123");
    assert_eq!(consumed.used_at, Some(ts(T0)));
    assert!(
        store
            .consume_enrollment_code("abc123", ts(T0))
            .expect("second consume")
            .is_none(),
        "a code is consumed exactly once"
    );

    // Two callers racing on separate connections must still produce one winner.
    let code = EnrollmentCode {
        code_hash: "race456".to_owned(),
        node: node("mac-studio"),
        kind: EnrollmentKind::Node,
        surface_id: None,
        expires_at: ts(T1),
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: ts(T0),
    };
    store.insert_enrollment_code(&code).expect("race code");
    drop(store);
    let winners = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            let store = Store::open_global(&paths).expect("first store");
            store
                .consume_enrollment_code("race456", ts(T0))
                .expect("first consume")
                .is_some()
        });
        let second = scope.spawn(|| {
            let store = Store::open_global(&paths).expect("second store");
            store
                .consume_enrollment_code("race456", ts(T0))
                .expect("second consume")
                .is_some()
        });
        u8::from(first.join().expect("first thread")) + u8::from(second.join().expect("second thread"))
    });
    assert_eq!(winners, 1, "exactly one racing caller redeems the code");
}

#[test]
fn enrollment_code_expires() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store
        .insert_pending_node(&node("mac-studio"), &[], ts(T0))
        .expect("pending node");
    let expired = EnrollmentCode {
        code_hash: "expired".to_owned(),
        node: node("mac-studio"),
        kind: EnrollmentKind::Node,
        surface_id: None,
        expires_at: ts(T1),
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: ts(T0),
    };
    store.insert_enrollment_code(&expired).expect("expired code");
    assert!(
        store
            .consume_enrollment_code("expired", ts(T1))
            .expect("at expiry")
            .is_none(),
        "a code is invalid at its expiry instant"
    );
    assert_eq!(
        store.purge_expired_enrollment_codes(ts(T1)).expect("purge"),
        1,
        "the expired row is purged"
    );
    assert!(
        store
            .consume_enrollment_code("expired", ts(T0))
            .expect("after purge")
            .is_none()
    );
}

#[test]
fn pending_enrollment_replacement_rotates_codes_and_labels_atomically() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let mac = node("mac-studio");
    store
        .insert_pending_node(&mac, &["old-label".to_owned()], ts(T0))
        .expect("pending node");
    store
        .insert_enrollment_code(&EnrollmentCode {
            code_hash: "old-code".to_owned(),
            node: mac.clone(),
            kind: EnrollmentKind::Node,
            surface_id: None,
            expires_at: ts(T1),
            used_at: None,
            created_by: "owner".to_owned(),
            created_at: ts(T0),
        })
        .expect("old code");

    store
        .replace_pending_enrollment(
            &mac,
            Some(&["new-label".to_owned()]),
            &EnrollmentCode {
                code_hash: "new-code".to_owned(),
                node: mac.clone(),
                kind: EnrollmentKind::Node,
                surface_id: None,
                expires_at: ts(T1),
                used_at: None,
                created_by: "owner".to_owned(),
                created_at: ts(T0),
            },
        )
        .expect("replace");

    assert!(
        store
            .consume_enrollment_code("old-code", ts(T0))
            .expect("old consume")
            .is_none(),
        "the replaced code must not redeem"
    );
    let consumed = store
        .consume_enrollment_code("new-code", ts(T0))
        .expect("new consume")
        .expect("the replacement code redeems");
    assert_eq!(consumed.node, mac);
    assert_eq!(consumed.used_at, Some(ts(T0)));
    let card = store.node_card(&mac, ts(T0)).expect("card").expect("present");
    assert_eq!(card.status, NodeStatus::Pending);
    assert_eq!(card.labels, vec!["new-label".to_owned()]);

    // Omitted labels preserve the labels already on the node.
    store
        .replace_pending_enrollment(
            &mac,
            None,
            &EnrollmentCode {
                code_hash: "third-code".to_owned(),
                node: mac.clone(),
                kind: EnrollmentKind::Node,
                surface_id: None,
                expires_at: ts(T1),
                used_at: None,
                created_by: "owner".to_owned(),
                created_at: ts(T0),
            },
        )
        .expect("replace without labels");
    assert_eq!(
        store.node_card(&mac, ts(T0)).expect("card").expect("present").labels,
        vec!["new-label".to_owned()]
    );
}

#[test]
fn pending_enrollment_replacement_refuses_active_and_revoked_nodes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let mac = node("mac-studio");
    store.insert_pending_node(&mac, &[], ts(T0)).expect("pending node");
    store
        .insert_enrollment_code(&EnrollmentCode {
            code_hash: "kept".to_owned(),
            node: mac.clone(),
            kind: EnrollmentKind::Node,
            surface_id: None,
            expires_at: ts(T1),
            used_at: None,
            created_by: "owner".to_owned(),
            created_at: ts(T0),
        })
        .expect("code");
    let replacement = EnrollmentCode {
        code_hash: "replacement".to_owned(),
        node: mac.clone(),
        kind: EnrollmentKind::Node,
        surface_id: None,
        expires_at: ts(T1),
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: ts(T0),
    };

    store
        .set_node_status(&mac, NodeStatus::Active, "", ts(T1))
        .expect("activate");
    let error = store
        .replace_pending_enrollment(&mac, None, &replacement)
        .expect_err("active node must be refused");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
    assert!(
        store
            .consume_enrollment_code("kept", ts(T0))
            .expect("kept code")
            .is_some(),
        "a refused replacement must leave the old code alone"
    );

    store
        .set_node_status(&mac, NodeStatus::Revoked, "", ts(T1))
        .expect("revoke");
    let error = store
        .replace_pending_enrollment(&mac, None, &replacement)
        .expect_err("revoked node must be refused");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
    assert_eq!(store.purge_node_enrollment_codes(&mac).expect("purge"), 1);
    assert!(
        store
            .consume_enrollment_code("replacement", ts(T0))
            .expect("replacement")
            .is_none()
    );
}

#[test]
fn node_credential_round_trips_and_revokes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store
        .insert_pending_node(&node("mac-studio"), &[], ts(T0))
        .expect("pending node");
    let credential = NodeCredential {
        node: node("mac-studio"),
        key_id: "sha256:key-one".to_owned(),
        public_key: "aa".repeat(32),
        created_at: ts(T0),
        last_used_at: None,
        revoked_at: None,
    };
    store.upsert_node_credential(&credential).expect("insert");
    assert_eq!(
        store
            .node_credential(&node("mac-studio"), "sha256:key-one")
            .expect("read"),
        Some(credential.clone())
    );

    store
        .upsert_node_credential(&NodeCredential {
            public_key: "bb".repeat(32),
            last_used_at: Some(ts(T1)),
            ..credential.clone()
        })
        .expect("update");
    let updated = store
        .node_credential(&node("mac-studio"), "sha256:key-one")
        .expect("read updated")
        .expect("present");
    assert_eq!(updated.public_key, "bb".repeat(32));
    assert_eq!(updated.created_at, ts(T0), "created_at is preserved");
    assert_eq!(updated.last_used_at, Some(ts(T1)));

    assert_eq!(
        store
            .revoke_node_credentials(&node("mac-studio"), ts(T1))
            .expect("revoke"),
        1
    );
    let revoked = store
        .node_credential(&node("mac-studio"), "sha256:key-one")
        .expect("read revoked")
        .expect("present");
    assert_eq!(revoked.revoked_at, Some(ts(T1)));
    assert_eq!(
        store
            .revoke_node_credentials(&node("mac-studio"), ts(T1))
            .expect("revoke again"),
        0,
        "revoking twice is idempotent"
    );
    assert!(
        store
            .node_credential(&node("mac-studio"), "missing")
            .expect("missing")
            .is_none()
    );
    let error = store
        .upsert_node_credential(&NodeCredential {
            node: node("unknown"),
            ..credential
        })
        .expect_err("unknown node must fail the foreign key");
    assert!(matches!(error, StoreError::InvalidInput(_)), "got {error:?}");
}

#[test]
fn node_path_upsert_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    store
        .insert_pending_node(&node("mac-studio"), &[], ts(T0))
        .expect("pending node");
    let path = NodePath {
        node: node("mac-studio"),
        project: project("vowp"),
        local_root: "/Users/me/src/vowp".to_owned(),
        worktree_root: "/Users/me/src/vowp/.vowp/worktrees".to_owned(),
        allow: vec!["src".to_owned(), "docs".to_owned()],
        updated_at: ts(T0),
    };
    store.upsert_node_path(&path).expect("insert");
    store
        .upsert_node_path(&NodePath {
            local_root: "/Users/me/src/vowp-renamed".to_owned(),
            allow: vec!["src".to_owned()],
            updated_at: ts(T1),
            ..path.clone()
        })
        .expect("update");
    let paths_for_node = store.list_node_paths(&node("mac-studio")).expect("paths");
    assert_eq!(paths_for_node.len(), 1, "the primary key makes the upsert idempotent");
    assert_eq!(paths_for_node[0].local_root, "/Users/me/src/vowp-renamed");
    assert_eq!(paths_for_node[0].allow, vec!["src".to_owned()]);
    assert_eq!(paths_for_node[0].updated_at, ts(T1));
    assert_eq!(paths_for_node[0].worktree_root, path.worktree_root);
}

#[test]
fn node_status_and_presence_persist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let mac = node("mac-studio");
    store
        .insert_pending_node(&mac, &["darwin-arm64".to_owned()], ts(T0))
        .expect("pending node");
    let pending = store.node_card(&mac, ts(T0)).expect("card").expect("present");
    assert_eq!(pending.status, NodeStatus::Pending);
    assert_eq!(pending.labels, vec!["darwin-arm64".to_owned()]);
    assert_eq!(pending.presence, NodePresence::Offline);
    assert!(pending.capacity.is_none());

    store
        .set_node_status(&mac, NodeStatus::Active, "", ts(T0))
        .expect("activate");
    store
        .touch_node(
            &mac,
            ts(T0),
            Some("0.4.0"),
            Some("vowp-agent/1"),
            &serde_json::json!({
                "capacity": {
                    "cpu_cores": 8,
                    "load1": 0.5,
                    "mem_total_bytes": 17179869184_u64,
                    "mem_free_bytes": 8589934592_u64,
                    "disk_free_bytes": 107374182400_u64,
                    "busy_slots": 1,
                    "max_slots": 4,
                    "on_battery": false,
                    "thermal_state": "nominal"
                }
            }),
        )
        .expect("heartbeat");
    let online = store
        .node_card(&mac, ts(T0).plus(Duration::from_secs(10)))
        .expect("card")
        .expect("present");
    assert_eq!(online.status, NodeStatus::Active);
    assert_eq!(online.presence, NodePresence::Online);
    assert_eq!(online.agent_version.as_deref(), Some("0.4.0"));
    assert_eq!(online.protocol.as_deref(), Some("vowp-agent/1"));
    assert_eq!(online.capacity.expect("capacity").cpu_cores, 8);

    let degraded = store
        .node_card(&mac, ts(T0).plus(Duration::from_secs(60)))
        .expect("card")
        .expect("present");
    assert_eq!(degraded.presence, NodePresence::Degraded);
    let offline = store
        .node_card(&mac, ts(T0).plus(Duration::from_secs(120)))
        .expect("card")
        .expect("present");
    assert_eq!(offline.presence, NodePresence::Offline);
    assert_eq!(store.list_node_cards(ts(T0)).expect("cards").len(), 1);

    store
        .set_node_status(&mac, NodeStatus::Active, "path_missing", ts(T1))
        .expect("record error");
    let with_error = store.node_card(&mac, ts(T1)).expect("card").expect("present");
    assert_eq!(with_error.last_error.as_deref(), Some("path_missing"));
}

#[test]
fn revoked_node_is_not_active() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let mac = node("mac-studio");
    store.insert_pending_node(&mac, &[], ts(T0)).expect("pending node");
    store
        .set_node_status(&mac, NodeStatus::Active, "", ts(T0))
        .expect("activate");
    store
        .touch_node(&mac, ts(T0), None, None, &serde_json::json!({}))
        .expect("touch");
    store
        .set_node_status(&mac, NodeStatus::Revoked, "credential revoked", ts(T1))
        .expect("revoke");
    // A heartbeat after revocation must not make the card look active.
    store
        .touch_node(
            &mac,
            ts(T1).plus(Duration::from_secs(1)),
            None,
            None,
            &serde_json::json!({}),
        )
        .expect("late heartbeat");
    let card = store
        .node_card(&mac, ts(T1).plus(Duration::from_secs(2)))
        .expect("card")
        .expect("present");
    assert_eq!(card.status, NodeStatus::Revoked);
    assert_eq!(card.presence, NodePresence::Revoked);
    assert_eq!(card.last_error.as_deref(), Some("credential revoked"));
    assert_ne!(card.status, NodeStatus::Active);
}

/// Builds one pending agent surface fixture.
fn surface(id: &SurfaceId) -> Surface {
    Surface {
        id: id.clone(),
        kind: SurfaceKind::Agent,
        node: node("mac-studio"),
        holder: "mac opencode".to_owned(),
        app: Some("opencode".to_owned()),
        cwd: Some("/Users/me/vowp".to_owned()),
        metadata: serde_json::json!({"terminal": "tmux"}),
        created_at: ts(T0),
        last_seen_at: ts(T0),
    }
}

/// Builds one token fixture with a hash and no plaintext value.
fn surface_token(surface_id: &SurfaceId, hash: &str) -> SurfaceToken {
    SurfaceToken {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FB0".to_owned(),
        surface_id: surface_id.clone(),
        token_hash: hash.to_owned(),
        prefix: "vowp_surf_abc".to_owned(),
        scopes: vec![
            SurfaceScope::new("session.read:01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            SurfaceScope::new("fleet.read"),
        ],
        created_at: ts(T0),
        expires_at: Some(ts(T1)),
        revoked_at: None,
        last_used_at: None,
    }
}

#[test]
fn surface_token_round_trips_and_hash_is_unique() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let surface_id = SurfaceId::new();
    store
        .insert_pending_surface(&surface(&surface_id))
        .expect("pending surface");

    let hash = hex::encode(Sha256::digest(b"vowp_surf_test_token"));
    let token = surface_token(&surface_id, &hash);
    store.insert_surface_token(&token).expect("insert token");
    assert_eq!(store.surface_token_by_hash(&hash).expect("lookup"), Some(token.clone()));
    assert_eq!(
        store.list_surface_tokens(&surface_id).expect("list"),
        vec![token.clone()]
    );

    let duplicate = SurfaceToken {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FB1".to_owned(),
        ..token
    };
    let error = store.insert_surface_token(&duplicate).expect_err("duplicate hash");
    assert!(matches!(error, StoreError::Conflict(_)), "got {error:?}");
    assert_eq!(store.list_surface_tokens(&surface_id).expect("list").len(), 1);
}

#[test]
fn surface_token_revoke_and_expiry_are_recorded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let surface_id = SurfaceId::new();
    store
        .insert_pending_surface(&surface(&surface_id))
        .expect("pending surface");
    let hash = hex::encode(Sha256::digest(b"vowp_surf_expiring"));
    let token = surface_token(&surface_id, &hash);
    store.insert_surface_token(&token).expect("insert token");

    let stored = store.surface_token_by_hash(&hash).expect("lookup").expect("token");
    assert_eq!(stored.expires_at, Some(ts(T1)), "expiry is preserved");
    assert_eq!(stored.revoked_at, None);
    assert_eq!(stored.last_used_at, None);

    assert!(store.touch_surface_token(&token.id, ts(T1)).expect("touch"));
    let touched = store.surface_token_by_hash(&hash).expect("lookup").expect("token");
    assert_eq!(touched.last_used_at, Some(ts(T1)));

    assert!(store.revoke_surface_token(&token.id, ts(T1)).expect("first revoke"));
    let revoked = store.surface_token_by_hash(&hash).expect("lookup").expect("token");
    assert_eq!(revoked.revoked_at, Some(ts(T1)));
    assert!(
        !store.revoke_surface_token(&token.id, ts(T1)).expect("second revoke"),
        "a revoked token stays revoked and does not move the timestamp"
    );
    assert!(!store.touch_surface_token("no-such-token", ts(T1)).expect("touch"));
    assert!(!store.revoke_surface_token("no-such-token", ts(T1)).expect("revoke"));
}

#[test]
fn surface_state_round_trips_pending_attached_revoked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let surface_id = SurfaceId::new();
    store
        .insert_pending_surface(&surface(&surface_id))
        .expect("pending surface");

    let pending = store.surface_by_id(&surface_id).expect("lookup").expect("surface");
    assert_eq!(pending.state, SurfaceState::Pending);
    assert_eq!(pending.revoked_at, None);
    assert_eq!(pending.kind, SurfaceKind::Agent);
    assert_eq!(pending.holder, "mac opencode");

    store
        .set_surface_state(&surface_id, SurfaceState::Attached, ts(T0))
        .expect("attach");
    let attached = store.surface_by_id(&surface_id).expect("lookup").expect("surface");
    assert_eq!(attached.state, SurfaceState::Attached);
    assert_eq!(attached.revoked_at, None);

    store
        .set_surface_state(&surface_id, SurfaceState::Revoked, ts(T1))
        .expect("revoke");
    let revoked = store.surface_by_id(&surface_id).expect("lookup").expect("surface");
    assert_eq!(revoked.state, SurfaceState::Revoked);
    assert_eq!(revoked.revoked_at, Some(ts(T1)));

    let missing = SurfaceId::new();
    let error = store
        .set_surface_state(&missing, SurfaceState::Attached, ts(T1))
        .expect_err("missing surface");
    assert!(matches!(error, StoreError::NotFound { .. }), "got {error:?}");
}

/// Builds a real v3 global database: the v4 schema with the surface-token additions removed.
fn v3_home() -> (TempDir, NodePaths, SurfaceId, SessionId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    paths.ensure().expect("dirs");
    let conn = Connection::open(paths.db_path()).expect("v3 db");
    conn.pragma_update(None, "journal_mode", "WAL").expect("wal");
    conn.execute_batch(include_str!("../src/schema/global.sql"))
        .expect("v4 base schema");
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;
         DROP TABLE surface_tokens;
         CREATE TABLE surfaces_v3 (
           id            TEXT PRIMARY KEY,
           kind          TEXT NOT NULL CHECK (kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node          TEXT NOT NULL,
           holder        TEXT NOT NULL,
           app           TEXT,
           cwd           TEXT,
           metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
           created_at    TEXT NOT NULL,
           last_seen_at  TEXT NOT NULL
         ) STRICT;
         INSERT INTO surfaces_v3
           (id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at)
         SELECT id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at FROM surfaces;
         DROP TABLE surfaces;
         ALTER TABLE surfaces_v3 RENAME TO surfaces;
         DROP TABLE leases;
         CREATE TABLE leases (
           session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
           surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
           surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node         TEXT NOT NULL,
           holder       TEXT NOT NULL,
           token        TEXT NOT NULL,
           acquired_at  TEXT NOT NULL,
           heartbeat_at TEXT NOT NULL,
           CHECK (length(token) = 52)
         ) STRICT;
         CREATE INDEX leases_heartbeat ON leases(heartbeat_at);
         PRAGMA user_version = 3;",
    )
    .expect("v3 schema");

    let session_id = SessionId::parse(SESSION_ID).expect("session id");
    let surface_id = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAX".to_owned());
    conn.execute(
        "INSERT INTO kv (key, value, updated_at) VALUES ('node_id', 'preserved-id', ?1)",
        [T0],
    )
    .expect("kv");
    conn.execute(
        "INSERT INTO nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
         VALUES ('local', NULL, 1, '{}', ?1, ?1)",
        [T0],
    )
    .expect("node");
    conn.execute(
        "INSERT INTO node_credentials (node, key_id, public_key, created_at)
         VALUES ('local', 'key-1', 'aa', ?1)",
        [T0],
    )
    .expect("credential");
    conn.execute(
        "INSERT INTO node_paths (node, project, local_root, worktree_root, updated_at)
         VALUES ('local', 'alpha', '/tmp/alpha', '/tmp/alpha-wt', ?1)",
        [T0],
    )
    .expect("path");
    conn.execute(
        "INSERT INTO enrollment_codes (code_hash, node, kind, expires_at, created_by, created_at)
         VALUES ('hash', 'local', 'node', ?1, 'owner', ?2)",
        params![T1, T0],
    )
    .expect("code");
    conn.execute(
        "INSERT INTO projects (name, path, remote_url, default_branch, config_json, state, weight,
                               paused_reason, archived_at, created_at, updated_at)
         VALUES ('alpha', '/tmp/alpha', NULL, 'main', '{}', 'active', 1, '', NULL, ?1, ?1)",
        [T0],
    )
    .expect("project");
    conn.execute(
        "INSERT INTO sessions (id, project, job_id, role, title, parent_session_id, node, cwd, state,
                               executor, last_seq, last_capsule_id, log_bytes, compacted_at, created_at, updated_at)
         VALUES (?1, 'alpha', NULL, 'user', 'kept', NULL, 'local', '/tmp', 'idle', NULL, 0, NULL, 0, NULL, ?2, ?2)",
        params![SESSION_ID, T0],
    )
    .expect("session");
    conn.execute(
        "INSERT INTO native_events (session_id, harness, native_id, event_id, at)
         VALUES (?1, 'claude-code', 'native-1', ?2, ?3)",
        params![SESSION_ID, EVENT_ID, T0],
    )
    .expect("native event");
    conn.execute(
        "INSERT INTO surfaces (id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at)
         VALUES (?1, 'chat', 'local', 'laptop', 'vowp chat', '/tmp', '{\"kept\":true}', ?2, ?2)",
        params![surface_id.0.as_str(), T0],
    )
    .expect("surface");
    conn.execute(
        "INSERT INTO leases (session_id, surface_id, surface_kind, node, holder, token, acquired_at, heartbeat_at)
         VALUES (?1, ?2, 'chat', 'local', 'laptop', ?3, ?4, ?4)",
        params![SESSION_ID, surface_id.0.as_str(), "A".repeat(52), T0],
    )
    .expect("lease");
    drop(conn);
    (tmp, paths, surface_id, session_id)
}

#[test]
fn migration_v3_to_v4_preserves_surfaces_and_rows() {
    let (_tmp, paths, surface_id, session_id) = v3_home();
    let source_tables: Vec<String> = {
        let source = Connection::open(paths.db_path()).expect("source db");
        let mut statement = source
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("source tables");
        statement
            .query_map([], |row| row.get(0))
            .expect("source table rows")
            .map(|row| row.expect("source table name"))
            .collect()
    };
    assert_eq!(source_tables.len(), 24, "the v3 schema has 24 tables");

    let report = migrate_v03(&paths).expect("migrate");
    assert_eq!(report.from_version, 3);
    assert_eq!(report.to_version, 4);
    assert!(report.verified);
    assert!(std::path::Path::new(&report.backup).is_file(), "backup exists");
    for table in &source_tables {
        assert!(
            report.tables.contains_key(&format!("global:{table}")),
            "the migration did not copy v3 table {table}"
        );
        assert_eq!(
            report.tables[&format!("global:{table}")].before,
            report.tables[&format!("global:{table}")].after,
            "row count drift for {table}"
        );
    }
    assert_eq!(report.tables["global:surfaces"].before, 1);
    assert_eq!(report.tables["global:surfaces"].after, 1);
    assert_eq!(report.tables["global:surface_tokens"].before, 0);
    assert_eq!(report.tables["global:surface_tokens"].after, 0);

    let backup = Connection::open(&report.backup).expect("backup db");
    let backup_version: i64 = backup
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 3, "the backup is the untouched v3 file");
    let backup_surface: String = backup
        .query_row(
            "SELECT holder FROM surfaces WHERE id = ?1",
            [surface_id.0.as_str()],
            |row| row.get(0),
        )
        .expect("backup surface");
    assert_eq!(backup_surface, "laptop");
    drop(backup);

    let store = Store::open(&paths).expect("open migrated home");
    assert_eq!(store.global_schema_version().expect("version"), 7);
    assert_eq!(store.kv_get("node_id").expect("kv"), Some("preserved-id".to_owned()));
    assert_eq!(store.require_session(&session_id).expect("session").title, "kept");
    let stored_surface = store
        .surface_by_id(&surface_id)
        .expect("surface lookup")
        .expect("surface preserved");
    assert_eq!(stored_surface.holder, "laptop");
    assert_eq!(stored_surface.state, SurfaceState::Attached);
    assert_eq!(stored_surface.revoked_at, None);
    assert_eq!(stored_surface.metadata, serde_json::json!({"kept": true}));
    assert!(store.list_surface_tokens(&surface_id).expect("tokens").is_empty());
    assert!(
        store.lease(&session_id).expect("lease").is_none(),
        "the v5 migration clears the ephemeral plaintext lease"
    );

    // The rebuilt kind check now accepts an `agent` surface.
    let agent = surface(&SurfaceId::new());
    store.insert_pending_surface(&agent).expect("agent surface after v4");
    let agent_state = store
        .surface_by_id(&agent.id)
        .expect("agent lookup")
        .expect("agent surface");
    assert_eq!(agent_state.kind, SurfaceKind::Agent);
    assert_eq!(agent_state.state, SurfaceState::Pending);
}

/// Builds a real v4 global database: the v5 schema with plaintext lease tokens restored.
fn v4_home() -> (TempDir, NodePaths, SurfaceId, SessionId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    paths.ensure().expect("dirs");
    let conn = Connection::open(paths.db_path()).expect("v4 db");
    conn.pragma_update(None, "journal_mode", "WAL").expect("wal");
    conn.execute_batch(include_str!("../src/schema/global.sql"))
        .expect("v5 base schema");
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;
         DROP TABLE leases;
         CREATE TABLE leases (
           session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
           surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
           surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node         TEXT NOT NULL,
           holder       TEXT NOT NULL,
           token        TEXT NOT NULL,
           acquired_at  TEXT NOT NULL,
           heartbeat_at TEXT NOT NULL,
           CHECK (length(token) = 52)
         ) STRICT;
         CREATE INDEX leases_heartbeat ON leases(heartbeat_at);
         PRAGMA user_version = 4;",
    )
    .expect("v4 schema");

    let session_id = SessionId::parse(SESSION_ID).expect("session id");
    let surface_id = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAY".to_owned());
    conn.execute(
        "INSERT INTO kv (key, value, updated_at) VALUES ('node_id', 'preserved-id', ?1)",
        [T0],
    )
    .expect("kv");
    conn.execute(
        "INSERT INTO nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
         VALUES ('local', NULL, 1, '{}', ?1, ?1)",
        [T0],
    )
    .expect("node");
    conn.execute(
        "INSERT INTO projects (name, path, remote_url, default_branch, config_json, state, weight,
                               paused_reason, archived_at, created_at, updated_at)
         VALUES ('alpha', '/tmp/alpha', NULL, 'main', '{}', 'active', 1, '', NULL, ?1, ?1)",
        [T0],
    )
    .expect("project");
    conn.execute(
        "INSERT INTO sessions (id, project, job_id, role, title, parent_session_id, node, cwd, state,
                               executor, last_seq, last_capsule_id, log_bytes, compacted_at, created_at, updated_at)
         VALUES (?1, 'alpha', NULL, 'user', 'kept', NULL, 'local', '/tmp', 'idle', NULL, 0, NULL, 0, NULL, ?2, ?2)",
        params![SESSION_ID, T0],
    )
    .expect("session");
    conn.execute(
        "INSERT INTO surfaces (id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at)
         VALUES (?1, 'chat', 'local', 'laptop', 'vowp chat', '/tmp', '{\"kept\":true}', ?2, ?2)",
        params![surface_id.0.as_str(), T0],
    )
    .expect("surface");
    conn.execute(
        "INSERT INTO surface_tokens (id, surface_id, token_hash, prefix, scopes_json, created_at)
         VALUES ('token-1', ?1, ?2, 'vowp_surf_ab', '[]', ?3)",
        params![surface_id.0.as_str(), "b".repeat(64), T0],
    )
    .expect("surface token");
    conn.execute(
        "INSERT INTO leases (session_id, surface_id, surface_kind, node, holder, token, acquired_at, heartbeat_at)
         VALUES (?1, ?2, 'chat', 'local', 'laptop', ?3, ?4, ?4)",
        params![SESSION_ID, surface_id.0.as_str(), "A".repeat(52), T0],
    )
    .expect("lease");
    drop(conn);
    (tmp, paths, surface_id, session_id)
}

#[test]
fn migration_v4_to_v5_clears_leases_and_preserves_surfaces() {
    let (_tmp, paths, surface_id, session_id) = v4_home();
    let source_tables: Vec<String> = {
        let source = Connection::open(paths.db_path()).expect("source db");
        let mut statement = source
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("source tables");
        statement
            .query_map([], |row| row.get(0))
            .expect("source table rows")
            .map(|row| row.expect("source table name"))
            .collect()
    };
    assert_eq!(source_tables.len(), 25, "the v4 schema has 25 tables");

    let report = migrate_v04(&paths).expect("migrate");
    assert_eq!(report.from_version, 4);
    assert_eq!(report.to_version, 5);
    assert!(report.verified);
    assert!(std::path::Path::new(&report.backup).is_file(), "backup exists");
    for table in &source_tables {
        assert!(
            report.tables.contains_key(&format!("global:{table}")),
            "the migration did not copy v4 table {table}"
        );
        if table == "leases" {
            continue;
        }
        assert_eq!(
            report.tables[&format!("global:{table}")].before,
            report.tables[&format!("global:{table}")].after,
            "row count drift for {table}"
        );
    }
    assert_eq!(report.tables["global:leases"].before, 1);
    assert_eq!(report.tables["global:leases"].after, 0);
    assert_eq!(report.tables["global:surfaces"].before, 1);
    assert_eq!(report.tables["global:surfaces"].after, 1);
    assert_eq!(report.tables["global:surface_tokens"].after, 1);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("cleared 1 ephemeral lease")),
        "the report records the cleared lease: {:?}",
        report.warnings
    );

    let backup = Connection::open(&report.backup).expect("backup db");
    let backup_version: i64 = backup
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 4, "the backup is the untouched v4 file");
    let backup_lease: String = backup
        .query_row("SELECT token FROM leases WHERE session_id = ?1", [SESSION_ID], |row| {
            row.get(0)
        })
        .expect("backup keeps the plaintext lease row");
    assert_eq!(backup_lease, "A".repeat(52));
    drop(backup);

    let store = Store::open(&paths).expect("open migrated home");
    assert_eq!(store.global_schema_version().expect("version"), 7);
    assert_eq!(store.kv_get("node_id").expect("kv"), Some("preserved-id".to_owned()));
    assert_eq!(store.require_session(&session_id).expect("session").title, "kept");
    let stored_surface = store
        .surface_by_id(&surface_id)
        .expect("surface lookup")
        .expect("surface preserved");
    assert_eq!(stored_surface.holder, "laptop");
    assert_eq!(stored_surface.state, SurfaceState::Attached);
    assert_eq!(
        store.list_surface_tokens(&surface_id).expect("tokens").len(),
        1,
        "surface tokens are preserved"
    );
    assert!(
        store.lease(&session_id).expect("lease").is_none(),
        "the ephemeral lease is cleared on upgrade"
    );

    let conn = Connection::open(paths.db_path()).expect("migrated db");
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('leases') ORDER BY cid")
        .expect("lease columns")
        .query_map([], |row| row.get(0))
        .expect("column rows")
        .map(|row| row.expect("column name"))
        .collect();
    assert!(columns.iter().any(|column| column == "token_hash"));
    assert!(!columns.iter().any(|column| column == "token"));
}

#[test]
fn surface_token_schema_has_no_plaintext_token() {
    let schema = include_str!("../src/schema/global.sql");
    let start = schema
        .find("CREATE TABLE surface_tokens (")
        .expect("surface_tokens table");
    let rest = &schema[start..];
    let end = rest.find("STRICT;").expect("surface_tokens end") + "STRICT;".len();
    let block = &rest[..end];
    for column in [
        "id",
        "surface_id",
        "token_hash",
        "prefix",
        "scopes_json",
        "created_at",
        "expires_at",
        "revoked_at",
        "last_used_at",
    ] {
        assert!(block.contains(column), "surface_tokens is missing {column}: {block}");
    }
    assert!(
        !block.lines().any(|line| line.trim_start().starts_with("token ")),
        "surface_tokens must not declare a plaintext token column: {block}"
    );
    assert!(
        schema.contains("CREATE UNIQUE INDEX surface_tokens_hash ON surface_tokens(token_hash);"),
        "surface_tokens.token_hash must have a unique index"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("store");
    let surface_id = SurfaceId::new();
    store
        .insert_pending_surface(&surface(&surface_id))
        .expect("pending surface");
    let plaintext = "vowp_surf_plaintext_must_never_be_stored";
    let hash = hex::encode(Sha256::digest(plaintext.as_bytes()));
    let token = surface_token(&surface_id, &hash);
    store.insert_surface_token(&token).expect("insert token");
    assert!(!format!("{token:?}").contains(plaintext));

    let conn = Connection::open(paths.db_path()).expect("inspect db");
    let mut statement = conn
        .prepare("SELECT id, surface_id, token_hash, prefix, scopes_json, created_at, expires_at, revoked_at, last_used_at FROM surface_tokens")
        .expect("select tokens");
    let mut found = 0;
    let rows = statement
        .query_map([], |row| {
            let mut text = String::new();
            for index in 0..9 {
                text.push_str(&row.get::<_, Option<String>>(index)?.unwrap_or_default());
                text.push('|');
            }
            Ok(text)
        })
        .expect("token rows");
    for row in rows {
        found += 1;
        assert!(
            !row.expect("token text").contains(plaintext),
            "the plaintext token was stored"
        );
    }
    assert_eq!(found, 1);
}
