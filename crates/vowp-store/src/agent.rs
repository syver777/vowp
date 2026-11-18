//! The v0.4 local-node records: credentials, enrollment codes, path maps and node cards.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.1, §4.2, §6.1, §6.2 and §11.1.
//!
//! A node keeps its v0.2 [`vowp_protocol::Node`] row and gains enrollment state
//! (`kind`, `status`, labels, version, protocol and last error). The records here are the
//! home-only side of that state: public keys, one-time codes, reported path maps and the
//! redacted [`NodeCard`] read model.
//!
//! No private key, raw enrollment code, absolute path map or secret name is returned by a
//! card. `NodeCard` deliberately has no path-map field.

use std::time::Duration;

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use vowp_protocol::{AgentCapacity, NodeCard, NodeName, NodePresence, NodeStatus, ProjectName, Timestamp};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text, ts_from_text,
    ts_to_text,
};
use crate::db::Store;
use crate::error::StoreError;

/// Presence threshold: a heartbeat under this age is online.
pub const PRESENCE_ONLINE_SECS: u64 = 30;

/// Presence threshold: a heartbeat at or under this age is degraded; older is offline.
pub const PRESENCE_DEGRADED_SECS: u64 = 90;

/// What an enrollment code redeems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentKind {
    /// A one-time node enrollment code.
    Node,
    /// A one-time surface pairing code (a later phase uses it).
    Surface,
}

/// One public key an enrolled node may authenticate with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCredential {
    /// Node the key belongs to.
    pub node: NodeName,
    /// Stable key id chosen by the agent, for example a fingerprint.
    pub key_id: String,
    /// Lowercase hex ed25519 public key; never a private key.
    pub public_key: String,
    /// When the key was first bound to the node.
    pub created_at: Timestamp,
    /// Last successful authentication with this key.
    pub last_used_at: Option<Timestamp>,
    /// When the key was revoked; `None` while it is active.
    pub revoked_at: Option<Timestamp>,
}

/// One one-time enrollment or surface code, stored only by its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentCode {
    /// Lowercase hex SHA-256 of the code; the code itself is never stored.
    pub code_hash: String,
    /// Node the code enrolls.
    pub node: NodeName,
    /// Whether this code enrolls a node or pairs a surface.
    pub kind: EnrollmentKind,
    /// Surface the code pairs, for [`EnrollmentKind::Surface`].
    pub surface_id: Option<String>,
    /// The code is invalid at or after this instant.
    pub expires_at: Timestamp,
    /// When the code was redeemed; `None` while unused.
    pub used_at: Option<Timestamp>,
    /// Who created the code, for example `owner`.
    pub created_by: String,
    /// When the code was created.
    pub created_at: Timestamp,
}

/// The last path map an agent reported for one project.
///
/// The row is home-only and is never copied into a [`NodeCard`], log or snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodePath {
    /// Node that reported the map.
    pub node: NodeName,
    /// Project the map belongs to.
    pub project: ProjectName,
    /// Absolute project root on the node.
    pub local_root: String,
    /// Absolute worktree root on the node.
    pub worktree_root: String,
    /// Allowed relative paths below the root.
    pub allow: Vec<String>,
    /// When the map was last reported.
    pub updated_at: Timestamp,
}

/// Column list shared by every credential query.
const CREDENTIAL_COLUMNS: &str = "node, key_id, public_key, created_at, last_used_at, revoked_at";

/// Column list shared by every enrollment-code query.
const CODE_COLUMNS: &str = "code_hash, node, kind, surface_id, expires_at, used_at, created_by, created_at";

/// Column list shared by every node-path query.
const PATH_COLUMNS: &str = "node, project, local_root, worktree_root, allow_json, updated_at";

/// Column list used to build a node card.
const CARD_COLUMNS: &str =
    "name, kind, status, labels_json, capabilities, last_seen_at, agent_version, protocol, last_error";

/// Raw TEXT values of one `node_credentials` row.
struct CredentialRow {
    node: String,
    key_id: String,
    public_key: String,
    created_at: String,
    last_used_at: Option<String>,
    revoked_at: Option<String>,
}

/// Raw TEXT values of one `enrollment_codes` row.
struct CodeRow {
    code_hash: String,
    node: String,
    kind: String,
    surface_id: Option<String>,
    expires_at: String,
    used_at: Option<String>,
    created_by: String,
    created_at: String,
}

/// Raw TEXT values of one `node_paths` row.
struct PathRow {
    node: String,
    project: String,
    local_root: String,
    worktree_root: String,
    allow: String,
    updated_at: String,
}

/// Raw TEXT values used to build one [`NodeCard`].
struct CardRow {
    name: String,
    kind: String,
    status: String,
    labels: String,
    capabilities: String,
    last_seen_at: Option<String>,
    agent_version: Option<String>,
    protocol: Option<String>,
    last_error: String,
}

/// Reads one `node_credentials` row.
fn read_credential_row(row: &Row<'_>) -> rusqlite::Result<CredentialRow> {
    Ok(CredentialRow {
        node: row.get(0)?,
        key_id: row.get(1)?,
        public_key: row.get(2)?,
        created_at: row.get(3)?,
        last_used_at: row.get(4)?,
        revoked_at: row.get(5)?,
    })
}

/// Converts a raw credential row into its public record.
fn credential_from_row(raw: CredentialRow) -> Result<NodeCredential, StoreError> {
    Ok(NodeCredential {
        node: NodeName::parse(&raw.node)?,
        key_id: raw.key_id,
        public_key: raw.public_key,
        created_at: ts_from_text(&raw.created_at, "node_credentials.created_at")?,
        last_used_at: opt_ts_from_text(raw.last_used_at.as_deref(), "node_credentials.last_used_at")?,
        revoked_at: opt_ts_from_text(raw.revoked_at.as_deref(), "node_credentials.revoked_at")?,
    })
}

/// Reads one `enrollment_codes` row.
fn read_code_row(row: &Row<'_>) -> rusqlite::Result<CodeRow> {
    Ok(CodeRow {
        code_hash: row.get(0)?,
        node: row.get(1)?,
        kind: row.get(2)?,
        surface_id: row.get(3)?,
        expires_at: row.get(4)?,
        used_at: row.get(5)?,
        created_by: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Converts a raw enrollment-code row into its public record.
fn code_from_row(raw: CodeRow) -> Result<EnrollmentCode, StoreError> {
    Ok(EnrollmentCode {
        code_hash: raw.code_hash,
        node: NodeName::parse(&raw.node)?,
        kind: enum_from_text(&raw.kind, "enrollment_codes.kind")?,
        surface_id: raw.surface_id,
        expires_at: ts_from_text(&raw.expires_at, "enrollment_codes.expires_at")?,
        used_at: opt_ts_from_text(raw.used_at.as_deref(), "enrollment_codes.used_at")?,
        created_by: raw.created_by,
        created_at: ts_from_text(&raw.created_at, "enrollment_codes.created_at")?,
    })
}

/// Reads one `node_paths` row.
fn read_path_row(row: &Row<'_>) -> rusqlite::Result<PathRow> {
    Ok(PathRow {
        node: row.get(0)?,
        project: row.get(1)?,
        local_root: row.get(2)?,
        worktree_root: row.get(3)?,
        allow: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// Converts a raw path row into its public record.
fn path_from_row(raw: PathRow) -> Result<NodePath, StoreError> {
    Ok(NodePath {
        node: NodeName::parse(&raw.node)?,
        project: ProjectName::parse(&raw.project)?,
        local_root: raw.local_root,
        worktree_root: raw.worktree_root,
        allow: json_from_text(&raw.allow, "node_paths.allow_json")?,
        updated_at: ts_from_text(&raw.updated_at, "node_paths.updated_at")?,
    })
}

/// Reads one card source row.
fn read_card_row(row: &Row<'_>) -> rusqlite::Result<CardRow> {
    Ok(CardRow {
        name: row.get(0)?,
        kind: row.get(1)?,
        status: row.get(2)?,
        labels: row.get(3)?,
        capabilities: row.get(4)?,
        last_seen_at: row.get(5)?,
        agent_version: row.get(6)?,
        protocol: row.get(7)?,
        last_error: row.get(8)?,
    })
}

/// Computes presence from the enrollment status and the last heartbeat.
fn presence(status: NodeStatus, last_seen_at: Option<Timestamp>, now: Timestamp) -> NodePresence {
    if status == NodeStatus::Revoked {
        return NodePresence::Revoked;
    }
    let Some(last_seen_at) = last_seen_at else {
        return NodePresence::Offline;
    };
    let age = now.since(last_seen_at);
    if age < Duration::from_secs(PRESENCE_ONLINE_SECS) {
        NodePresence::Online
    } else if age <= Duration::from_secs(PRESENCE_DEGRADED_SECS) {
        NodePresence::Degraded
    } else {
        NodePresence::Offline
    }
}

/// Extracts an optional capacity snapshot from the stored capabilities JSON.
///
/// The gateway writes an `AgentCapabilities` object under `capabilities`; when a heartbeat
/// carries capacity it stores it under the same object's `capacity` key. Accepting a bare
/// capacity object as well keeps old fixtures readable without inventing a second column.
fn capacity_from_capabilities(capabilities: &serde_json::Value) -> Result<Option<AgentCapacity>, StoreError> {
    let value = match capabilities.get("capacity") {
        Some(value) if !value.is_null() => value,
        Some(_) => return Ok(None),
        None if capabilities.get("cpu_cores").is_some() => capabilities,
        None => return Ok(None),
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|err| StoreError::Corrupt {
            path: "nodes.capabilities.capacity".to_owned(),
            line: None,
            reason: err.to_string(),
        })
}

/// Converts one card source row into a redacted [`NodeCard`].
fn card_from_row(store: &Store, raw: CardRow, now: Timestamp) -> Result<NodeCard, StoreError> {
    let status: NodeStatus = enum_from_text(&raw.status, "nodes.status")?;
    let last_seen_at = opt_ts_from_text(raw.last_seen_at.as_deref(), "nodes.last_seen_at")?;
    let capabilities: serde_json::Value = json_from_text(&raw.capabilities, "nodes.capabilities")?;
    let node = NodeName::parse(&raw.name)?;
    let mapped = store
        .list_node_paths(&node)?
        .into_iter()
        .map(|path| path.project)
        .collect();
    Ok(NodeCard {
        name: node,
        kind: enum_from_text(&raw.kind, "nodes.kind")?,
        status,
        presence: presence(status, last_seen_at, now),
        labels: json_from_text(&raw.labels, "nodes.labels_json")?,
        capacity: capacity_from_capabilities(&capabilities)?,
        runs: 0,
        mapped,
        agent_version: non_empty(raw.agent_version),
        protocol: non_empty(raw.protocol),
        last_error: non_empty(Some(raw.last_error)),
    })
}

/// Maps an empty stored string to `None`, keeping the card free of blank fields.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

impl Store {
    /// Returns one credential of `node`, when it exists.
    pub fn node_credential(&self, node: &NodeName, key_id: &str) -> Result<Option<NodeCredential>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {CREDENTIAL_COLUMNS} FROM main.node_credentials WHERE node = ?1 AND key_id = ?2"),
                params![node.as_str(), key_id],
                read_credential_row,
            )
            .optional()?;
        raw.map(credential_from_row).transpose()
    }

    /// Inserts or updates one node credential, keyed by node and key id.
    ///
    /// `created_at` is preserved for an existing key; the public key and revocation fields are
    /// replaced. Rebinding a revoked key is allowed and is how re-enrollment rotates a key.
    pub fn upsert_node_credential(&self, credential: &NodeCredential) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.node_credentials
               (node, key_id, public_key, created_at, last_used_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node, key_id) DO UPDATE SET
               public_key = excluded.public_key,
               last_used_at = excluded.last_used_at,
               revoked_at = excluded.revoked_at",
            params![
                credential.node.as_str(),
                credential.key_id,
                credential.public_key,
                ts_to_text(credential.created_at),
                opt_ts_to_text(credential.last_used_at),
                opt_ts_to_text(credential.revoked_at),
            ],
        )?;
        Ok(())
    }

    /// Revokes every active credential of `node` at `at` and returns the number changed.
    pub fn revoke_node_credentials(&self, node: &NodeName, at: Timestamp) -> Result<u64, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.node_credentials SET revoked_at = ?1
             WHERE node = ?2 AND revoked_at IS NULL",
            params![ts_to_text(at), node.as_str()],
        )?;
        Ok(changed as u64)
    }

    /// Inserts one one-time enrollment code.
    pub fn insert_enrollment_code(&self, code: &EnrollmentCode) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.enrollment_codes
               (code_hash, node, kind, surface_id, expires_at, used_at, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                code.code_hash,
                code.node.as_str(),
                enum_to_text(&code.kind)?,
                code.surface_id,
                ts_to_text(code.expires_at),
                opt_ts_to_text(code.used_at),
                code.created_by,
                ts_to_text(code.created_at),
            ],
        )?;
        Ok(())
    }

    /// Redeems an unused, unexpired code exactly once and returns the consumed row.
    ///
    /// The read and the `used_at` write happen in one `BEGIN IMMEDIATE` transaction, so two
    /// callers racing for the same hash cannot both receive `Some`. An expired or already-used
    /// code returns `Ok(None)` and is left untouched.
    pub fn consume_enrollment_code(
        &self,
        code_hash: &str,
        now: Timestamp,
    ) -> Result<Option<EnrollmentCode>, StoreError> {
        self.transaction(|store| {
            let changed = store.conn.execute(
                "UPDATE main.enrollment_codes SET used_at = ?1
                 WHERE code_hash = ?2 AND used_at IS NULL AND expires_at > ?1",
                params![ts_to_text(now), code_hash],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let raw = store
                .conn
                .query_row(
                    &format!("SELECT {CODE_COLUMNS} FROM main.enrollment_codes WHERE code_hash = ?1"),
                    [code_hash],
                    read_code_row,
                )
                .optional()?;
            raw.map(code_from_row).transpose()
        })
    }

    /// Deletes enrollment codes that are expired at `before` and returns the number removed.
    pub fn purge_expired_enrollment_codes(&self, before: Timestamp) -> Result<u64, StoreError> {
        let removed = self.conn.execute(
            "DELETE FROM main.enrollment_codes WHERE expires_at <= ?1",
            [ts_to_text(before)],
        )?;
        Ok(removed as u64)
    }

    /// Deletes every enrollment code of one node and returns the number removed.
    ///
    /// `enrollment_codes` has no foreign key to `nodes`, so removing a node leaves its codes
    /// behind unless the caller removes them explicitly.
    pub fn purge_node_enrollment_codes(&self, node: &NodeName) -> Result<u64, StoreError> {
        let removed = self
            .conn
            .execute("DELETE FROM main.enrollment_codes WHERE node = ?1", [node.as_str()])?;
        Ok(removed as u64)
    }

    /// Rotates a pending node's one-time code in one immediate transaction.
    ///
    /// Every unconsumed node code of `node` is deleted, the labels are replaced when
    /// `labels` is `Some`, and `code` is inserted. The node must still be `pending`: an
    /// active or revoked node is refused so a live enrollment is never wiped silently.
    pub fn replace_pending_enrollment(
        &self,
        node: &NodeName,
        labels: Option<&[String]>,
        code: &EnrollmentCode,
    ) -> Result<(), StoreError> {
        if code.node != *node || code.kind != EnrollmentKind::Node {
            return Err(StoreError::InvalidInput(
                "replacement code must be a node code for the same node".to_owned(),
            ));
        }
        self.transaction(|store| {
            let status: Option<String> = store
                .conn
                .query_row(
                    "SELECT status FROM main.nodes WHERE name = ?1",
                    [node.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(status) = status else {
                return Err(StoreError::not_found("node", node.as_str()));
            };
            if status != "pending" {
                return Err(StoreError::Conflict(format!(
                    "node {node} is {status}; only a pending enrollment can be replaced"
                )));
            }
            if let Some(labels) = labels {
                store.conn.execute(
                    "UPDATE main.nodes SET labels_json = ?1 WHERE name = ?2",
                    params![json_to_text(&labels.to_vec())?, node.as_str()],
                )?;
            }
            store.conn.execute(
                "DELETE FROM main.enrollment_codes WHERE node = ?1 AND kind = 'node' AND used_at IS NULL",
                [node.as_str()],
            )?;
            store.conn.execute(
                "INSERT INTO main.enrollment_codes
                   (code_hash, node, kind, surface_id, expires_at, used_at, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    code.code_hash,
                    code.node.as_str(),
                    enum_to_text(&code.kind)?,
                    code.surface_id,
                    ts_to_text(code.expires_at),
                    opt_ts_to_text(code.used_at),
                    code.created_by,
                    ts_to_text(code.created_at),
                ],
            )?;
            Ok(())
        })
    }

    /// Inserts or replaces the path map for one node and project.
    pub fn upsert_node_path(&self, path: &NodePath) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.node_paths (node, project, local_root, worktree_root, allow_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node, project) DO UPDATE SET
               local_root = excluded.local_root,
               worktree_root = excluded.worktree_root,
               allow_json = excluded.allow_json,
               updated_at = excluded.updated_at",
            params![
                path.node.as_str(),
                path.project.as_str(),
                path.local_root,
                path.worktree_root,
                json_to_text(&path.allow)?,
                ts_to_text(path.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Lists one node's path maps, ordered by project.
    pub fn list_node_paths(&self, node: &NodeName) -> Result<Vec<NodePath>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {PATH_COLUMNS} FROM main.node_paths WHERE node = ?1 ORDER BY project"
        ))?;
        let rows = statement.query_map([node.as_str()], read_path_row)?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(path_from_row(row?)?);
        }
        Ok(paths)
    }

    /// Sets a node's enrollment status and last error, recording `at` as its last contact.
    ///
    /// The `nodes` table has no separate status timestamp; `last_seen_at` is the update time.
    /// A revoked node computes presence `revoked` regardless of that value.
    pub fn set_node_status(
        &self,
        node: &NodeName,
        status: NodeStatus,
        last_error: &str,
        at: Timestamp,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.nodes SET status = ?1, last_error = ?2, last_seen_at = ?3 WHERE name = ?4",
            params![enum_to_text(&status)?, last_error, ts_to_text(at), node.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("node", node.as_str()));
        }
        Ok(())
    }

    /// Records a successful contact: heartbeat time, agent version, protocol and capabilities.
    ///
    /// The enrollment status, kind, labels and any last error are left untouched.
    pub fn touch_node(
        &self,
        node: &NodeName,
        at: Timestamp,
        agent_version: Option<&str>,
        protocol: Option<&str>,
        capabilities: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.nodes SET
               last_seen_at = ?1,
               agent_version = ?2,
               protocol = ?3,
               capabilities = ?4
             WHERE name = ?5",
            params![
                ts_to_text(at),
                agent_version,
                protocol,
                json_to_text(capabilities)?,
                node.as_str()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("node", node.as_str()));
        }
        Ok(())
    }

    /// Inserts an `agent`-kind, `pending` node with owner-assigned labels.
    ///
    /// The existing [`vowp_protocol::Node`] type has no enrollment fields, so this is the only
    /// constructor that writes `kind='agent'` and `status='pending'`.
    pub fn insert_pending_node(&self, node: &NodeName, labels: &[String], at: Timestamp) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.nodes
               (name, ssh_target, is_local, capabilities, last_seen_at, created_at,
                kind, status, labels_json, agent_version, protocol, last_error)
             VALUES (?1, NULL, 0, '{}', NULL, ?2, 'agent', 'pending', ?3, NULL, NULL, '')",
            params![node.as_str(), ts_to_text(at), json_to_text(&labels.to_vec())?],
        )?;
        Ok(())
    }

    /// Returns the redacted card of one node, with presence computed at `now`.
    pub fn node_card(&self, node: &NodeName, now: Timestamp) -> Result<Option<NodeCard>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {CARD_COLUMNS} FROM main.nodes WHERE name = ?1"),
                [node.as_str()],
                read_card_row,
            )
            .optional()?;
        raw.map(|row| card_from_row(self, row, now)).transpose()
    }

    /// Lists every node as a redacted card, ordered by name, with presence computed at `now`.
    pub fn list_node_cards(&self, now: Timestamp) -> Result<Vec<NodeCard>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {CARD_COLUMNS} FROM main.nodes ORDER BY name"))?;
        let rows = statement.query_map([], read_card_row)?;
        let mut cards = Vec::new();
        for row in rows {
            cards.push(card_from_row(self, row?, now)?);
        }
        Ok(cards)
    }
}
