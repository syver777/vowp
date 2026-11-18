//! Phase 0 node enrollment: create a pending node and redeem its one-time code.
//!
//! The home creates an `agent`-kind `pending` node and stores only the code hash. Redemption
//! consumes the code atomically ([`vowp_store::Store::consume_enrollment_code`]), binds the
//! node's ed25519 public key, marks the node active and records its version, protocol,
//! capabilities and initial path map. A code issued before an existing enrollment is stale
//! and cannot re-enroll an active or revoked node.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use vowp_protocol::{
    AGENT_PROTOCOL, AgentCapabilities, AgentPathMap, MAX_PATH_REF_LEN, NodeName, NodeStatus, ProjectName, Timestamp,
};
use vowp_store::{EnrollmentCode, EnrollmentKind, NodeCredential, NodePath, Store};

use super::auth;
use super::code::{self, NewEnrollmentCode};
use crate::CoreError;

/// A pending node plus the plaintext code shown once by `vowp node enroll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEnrollment {
    /// Node the code enrolls.
    pub node: NodeName,
    /// Plaintext code; callers print it once and never store it.
    pub code: String,
    /// Stored hash of [`Self::code`].
    pub code_hash: String,
    /// Instant at which the code stops redeeming.
    pub expires_at: Timestamp,
}

/// The fields of one `Enroll` frame that the home consumes.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollRequest {
    /// One-time code read from stdin by the agent.
    pub code: String,
    /// Lowercase hex ed25519 public key.
    pub public_key: String,
    /// Agent binary version.
    pub agent_version: String,
    /// Names-only capabilities.
    pub capabilities: AgentCapabilities,
    /// Initial project path map.
    pub paths: Vec<AgentPathMap>,
}

/// A successfully enrolled node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// Enrolled node.
    pub node: NodeName,
}

/// Creates a pending `agent` node and a one-time enrollment code.
///
/// A node that already exists in any enrollment state is a conflict; [`replace_pending`]
/// rotates a pending node's code and refuses an active or revoked node. Expired codes are
/// purged first so a stranded code does not linger in the store. The code is returned in
/// plaintext exactly once; only its hash is written to the store.
pub fn create_pending(
    store: &Store,
    node: &NodeName,
    labels: &[String],
    now: Timestamp,
) -> Result<PendingEnrollment, CoreError> {
    store.purge_expired_enrollment_codes(now)?;
    if let Some(card) = store.node_card(node, now)? {
        return Err(vowp_store::StoreError::Conflict(format!(
            "node {node} already exists ({:?}); pass `--replace` to rotate a pending node, \
             or revoke or remove it before enrolling again",
            card.status
        ))
        .into());
    }
    store.insert_pending_node(node, labels, now)?;
    let generated = code::new_enrollment_code(now);
    let NewEnrollmentCode {
        code,
        code_hash,
        expires_at,
    } = generated;
    store.insert_enrollment_code(&EnrollmentCode {
        code_hash: code_hash.clone(),
        node: node.clone(),
        kind: EnrollmentKind::Node,
        surface_id: None,
        expires_at,
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: now,
    })?;
    Ok(PendingEnrollment {
        node: node.clone(),
        code,
        code_hash,
        expires_at,
    })
}

/// Rotates a pending node's one-time code in one transaction.
///
/// An expired pending code is purged and replaced in the same call, so
/// `vowp node enroll <name> --replace` recovers a stranded pending enrollment in one step.
/// An `active` node is refused: replacing it would wipe a live enrollment. A `revoked` node
/// must be removed first, which keeps every destructive action explicit.
pub fn replace_pending(
    store: &Store,
    node: &NodeName,
    labels: &[String],
    now: Timestamp,
) -> Result<PendingEnrollment, CoreError> {
    store.purge_expired_enrollment_codes(now)?;
    let Some(card) = store.node_card(node, now)? else {
        return Err(CoreError::NotFound(format!("node {node}")));
    };
    match card.status {
        NodeStatus::Pending => {}
        NodeStatus::Active => {
            return Err(vowp_store::StoreError::Conflict(format!(
                "node {node} is active; run `vowp node revoke {node} --yes` or \
                 `vowp node rm {node} --yes` before enrolling a replacement"
            ))
            .into());
        }
        NodeStatus::Revoked => {
            return Err(vowp_store::StoreError::Conflict(format!(
                "node {node} is revoked; run `vowp node rm {node} --yes` before enrolling again"
            ))
            .into());
        }
    }
    let generated = code::new_enrollment_code(now);
    let NewEnrollmentCode {
        code,
        code_hash,
        expires_at,
    } = generated;
    let labels = (!labels.is_empty()).then_some(labels);
    store.replace_pending_enrollment(
        node,
        labels,
        &EnrollmentCode {
            code_hash: code_hash.clone(),
            node: node.clone(),
            kind: EnrollmentKind::Node,
            surface_id: None,
            expires_at,
            used_at: None,
            created_by: "owner".to_owned(),
            created_at: now,
        },
    )?;
    Ok(PendingEnrollment {
        node: node.clone(),
        code,
        code_hash,
        expires_at,
    })
}

/// Validates and stores one path map reported on an authenticated connection.
///
/// The roots exist on the node, not on the home, so validation is lexical: absolute,
/// non-empty, free of control bytes and parent traversal. The home never touches its own
/// disk here and never copies the raw roots into a card, log or frame.
pub fn apply_path_map(store: &Store, node: &NodeName, map: &AgentPathMap, now: Timestamp) -> Result<(), CoreError> {
    validate_paths(std::slice::from_ref(map))?;
    let Some(card) = store.node_card(node, now)? else {
        return Err(CoreError::NotFound(format!("node {node}")));
    };
    if card.status != NodeStatus::Active {
        return Err(CoreError::Refused(format!("node {node} is not active")));
    }
    store.upsert_node_path(&node_path(node, map, now))?;
    Ok(())
}

/// Converts one validated map into the home-only `node_paths` row.
fn node_path(node: &NodeName, map: &AgentPathMap, now: Timestamp) -> NodePath {
    NodePath {
        node: node.clone(),
        project: map.project.clone(),
        local_root: map.local_root.clone(),
        worktree_root: map.worktree_root.clone().unwrap_or_else(|| map.local_root.clone()),
        allow: map.allow.clone(),
        updated_at: now,
    }
}

/// Redeems one enrollment code and binds the node's public key.
///
/// The store consumes the code in one immediate transaction, so two racing gateways cannot
/// both redeem it. An expired, unknown or already-used code is refused with a message that
/// never echoes the code.
pub fn redeem(store: &Store, request: &EnrollRequest, now: Timestamp) -> Result<Enrolled, CoreError> {
    if !code::has_code_shape(&request.code) {
        return Err(CoreError::Invalid("enrollment code must be 52 characters".to_owned()));
    }
    if !auth::is_public_key_hex(&request.public_key) {
        return Err(CoreError::Invalid(
            "node public key must be 32 bytes in lowercase hex".to_owned(),
        ));
    }
    validate_paths(&request.paths)?;

    let code_hash = code::hash_code(&request.code);
    let Some(stored) = store.consume_enrollment_code(&code_hash, now)? else {
        return Err(CoreError::Refused(
            "enrollment code is invalid, expired or already used".to_owned(),
        ));
    };
    if stored.kind != EnrollmentKind::Node {
        return Err(CoreError::Invalid("enrollment code is not a node code".to_owned()));
    }
    let Some(card) = store.node_card(&stored.node, now)? else {
        return Err(CoreError::NotFound(format!("node {}", stored.node)));
    };
    match card.status {
        NodeStatus::Pending => {}
        NodeStatus::Active | NodeStatus::Revoked => {
            // A fresh code may rotate an existing node's key, but a code created before the
            // last successful contact is stale: re-enrollment must be an explicit new action.
            let node = store.require_node(&stored.node)?;
            let fresh = node.last_seen_at.is_none_or(|seen| stored.created_at > seen);
            if !fresh {
                return Err(CoreError::Refused(
                    "enrollment code predates the node's current enrollment".to_owned(),
                ));
            }
        }
    }

    // Rotate every previous key; only the key in this frame stays active.
    store.revoke_node_credentials(&stored.node, now)?;
    store.upsert_node_credential(&NodeCredential {
        node: stored.node.clone(),
        key_id: request.public_key.clone(),
        public_key: request.public_key.clone(),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
    })?;
    store.set_node_status(&stored.node, NodeStatus::Active, "", now)?;
    store.touch_node(
        &stored.node,
        now,
        Some(&request.agent_version),
        Some(AGENT_PROTOCOL),
        &capabilities_value(&request.capabilities)?,
    )?;
    for path in &request.paths {
        store.upsert_node_path(&node_path(&stored.node, path, now))?;
    }
    Ok(Enrolled { node: stored.node })
}

/// Revokes a node and every active credential, returning how many credentials changed.
pub fn revoke(store: &Store, node: &NodeName, now: Timestamp) -> Result<u64, CoreError> {
    if store.node_card(node, now)?.is_none() {
        return Err(CoreError::NotFound(format!("node {node}")));
    }
    let revoked = store.revoke_node_credentials(node, now)?;
    store.set_node_status(node, NodeStatus::Revoked, "", now)?;
    Ok(revoked)
}

/// Serialises names-only capabilities for the `nodes.capabilities` column.
pub(crate) fn capabilities_value(capabilities: &AgentCapabilities) -> Result<serde_json::Value, CoreError> {
    serde_json::to_value(capabilities)
        .map_err(vowp_protocol::ProtocolError::from)
        .map_err(CoreError::from)
}

/// Refuses a path map entry the home cannot store safely.
///
/// The rules mirror the node's `AgentConfig::validate_path_maps` lexically. The roots are
/// remote paths, so the home never resolves them on its own filesystem.
fn validate_paths(paths: &[AgentPathMap]) -> Result<(), CoreError> {
    let mut projects = BTreeSet::new();
    for path in paths {
        validate_path_map(path)?;
        if !projects.insert(path.project.as_str()) {
            return Err(CoreError::Invalid(format!(
                "project {} is mapped more than once",
                path.project
            )));
        }
    }
    Ok(())
}

/// Refuses one path map whose roots or allow entries are not safe home records.
fn validate_path_map(path: &AgentPathMap) -> Result<(), CoreError> {
    // `AgentPathMap.project` is already a validated slug; re-parsing keeps this function
    // usable when a future caller passes raw text and names the project in its errors.
    if ProjectName::parse(path.project.as_str()).is_err() {
        return Err(CoreError::Invalid(format!(
            "project {:?} is not a valid project name",
            path.project.as_str()
        )));
    }
    validate_root(&path.local_root, &path.project, "local root")?;
    if let Some(root) = &path.worktree_root {
        validate_root(root, &path.project, "worktree root")?;
    }
    let mut allow = BTreeSet::new();
    for entry in &path.allow {
        validate_allow_entry(entry, &path.project)?;
        if !allow.insert(entry.as_str()) {
            return Err(CoreError::Invalid(format!(
                "project {} has a duplicate allow entry",
                path.project
            )));
        }
    }
    Ok(())
}

/// Refuses one absolute remote root with traversal, NUL or control bytes.
fn validate_root(root: &str, project: &ProjectName, label: &str) -> Result<(), CoreError> {
    if root.is_empty() || !root.starts_with('/') {
        return Err(CoreError::Invalid(format!(
            "project {project} {label} must be an absolute path"
        )));
    }
    if root.contains('\0') || root.chars().any(char::is_control) {
        return Err(CoreError::Invalid(format!(
            "project {project} {label} contains a control character"
        )));
    }
    if Path::new(root)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CoreError::Invalid(format!(
            "project {project} {label} must not contain parent traversal"
        )));
    }
    Ok(())
}

/// Refuses one allow entry that is not a normalised relative path.
fn validate_allow_entry(entry: &str, project: &ProjectName) -> Result<(), CoreError> {
    let invalid = || {
        CoreError::Invalid(format!(
            "project {project} allow entry must be a normalised relative path"
        ))
    };
    if entry.is_empty()
        || entry.len() > MAX_PATH_REF_LEN
        || entry.contains('\0')
        || entry.contains('\\')
        || entry.starts_with('/')
    {
        return Err(invalid());
    }
    for part in entry.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(invalid());
        }
    }
    Ok(())
}
