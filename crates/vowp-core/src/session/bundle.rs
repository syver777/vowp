//! Session export/import bundles: one self-contained JSON document per session.
//! Spec: `docs/design/sessions.md` §6 and `docs/design/crates/vowp-core.md` §3.
//!
//! [`export`] snapshots the session row, a fresh export capsule, the last replayable
//! events and the switch lineage, inlines small blobs and signs the result with a
//! sha256 checksum. [`import`] verifies that document before touching any state and
//! creates a *child* session whose log carries exactly four setup events; the origin
//! session is never modified and tail events are not copied. [`import_with_mode`] adds
//! [`ImportMode::SameId`], which keeps the origin id when it is absent locally, copies
//! the bundle tail into the log and refuses a duplicate instead of overwriting.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vowp_protocol::{
    Actor, BLOB_PREVIEW_CHARS, BlobRef, Capsule, CapsuleEvent, CapsuleId, CapsuleTrigger, EventBody, EventEnvelope,
    EventSource, EventType, ExecutorRef, HarnessKind, NewSession, NodeName, Note, PROTOCOL_VERSION, Session, SessionId,
    SessionState, SwitchEvent, SwitchId, SwitchReason, Timestamp,
};
use vowp_store::blobs::BlobStore;
use vowp_store::reindex::reindex;
use vowp_store::session_files::{list_capsule_files, read_capsule_file, write_capsule_file};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, Store, StoreError};

use crate::deps::Deps;
use crate::error::CoreError;
use crate::session::capsule::CapsuleWriter;
use crate::session::seed::seed_prompt;
use crate::session::service::{SessionHead, SessionService};

/// The only bundle format this build reads and writes.
pub const BUNDLE_FORMAT: &str = "vowp-bundle-v1";

/// Default number of trailing events in a bundle.
const DEFAULT_TAIL: usize = 20;

/// Upper bound accepted for `--tail`.
const MAX_TAIL: usize = 500;

/// Blobs at most this many bytes are inlined as base64.
const INLINE_BLOB_MAX: u64 = 65_536;

/// Sidecar holding the origin bundle until the child's first turn consumes it.
const IMPORT_SIDECAR: &str = "import.json";

/// One decoded inline blob: its lowercase hex SHA-256 and content.
type DecodedBlob = (String, Vec<u8>);

/// One exported session, self-contained enough to continue on another node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    /// Always [`BUNDLE_FORMAT`].
    pub format: String,
    /// Protocol version of the exporting node.
    pub protocol_version: u32,
    /// When the bundle was produced.
    pub exported_at: Timestamp,
    /// Node that owned the origin session.
    pub source_node: NodeName,
    /// Origin session index row.
    pub session: Session,
    /// Fresh capsule written for this export.
    pub capsule: Capsule,
    /// Last replayable events; reasoning is never included.
    pub tail: Vec<EventEnvelope>,
    /// Every switch row of the origin session.
    pub lineage: Vec<vowp_protocol::Switch>,
    /// Base64 content of small blobs referenced by `tail`, keyed by sha256.
    pub blobs_inline: BTreeMap<String, String>,
    /// When the origin log was replaced by compaction, so `tail` is empty and `capsule`
    /// carries the whole history. Absent for a live-log export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at: Option<Timestamp>,
    /// `"sha256:"` followed by the digest of the canonical bundle without this field.
    pub checksum: String,
}

impl Bundle {
    /// Verifies format, protocol version, checksum and the capsule body.
    ///
    /// This changes no state and is called by [`from_json`] and [`import`] before any
    /// session or file is created.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.format != BUNDLE_FORMAT {
            return Err(CoreError::Invalid(format!("unknown bundle format: {}", self.format)));
        }
        if self.protocol_version > PROTOCOL_VERSION {
            return Err(CoreError::Invalid(format!(
                "bundle too new: protocol {} > {}",
                self.protocol_version, PROTOCOL_VERSION
            )));
        }
        let expected = checksum(self)?;
        if expected != self.checksum {
            return Err(CoreError::Invalid("checksum mismatch".to_owned()));
        }
        if self.compacted_at.is_some() && !self.tail.is_empty() {
            return Err(CoreError::Invalid("compacted bundle carries an event tail".to_owned()));
        }
        self.capsule.validate()?;
        Ok(())
    }
}

/// Exports `session` into a [`Bundle`] with at most `tail` trailing replayable events.
///
/// `tail == 0` selects the default of 20 events; values above 500 are capped. A fresh
/// capsule with trigger `Export` is written first so the bundle never depends on the
/// origin node being alive later. A compacted session has no log to read: its latest
/// capsule is exported as-is and the event tail is empty (with [`Bundle::compacted_at`]
/// recording why), so compaction's history is preserved rather than dropped.
pub fn export(deps: &Deps, session: &SessionId, tail: usize) -> Result<Bundle, CoreError> {
    let store = deps.open_store()?;
    let mut row = store.require_session(session)?;
    let compacted_at = store.lifecycle_compacted_at(session)?;
    let log_present = deps.paths.log_path(session).is_file();
    let compacted = compacted_at.is_some() || !log_present;

    let (capsule, tail_events, blobs_inline, last_seq) = if compacted {
        let capsule = latest_capsule(&store, &deps.paths, session)?.ok_or_else(|| {
            CoreError::NotFound(format!("session {session} was compacted but has no capsule to export"))
        })?;
        // Compaction deleted the log on purpose: there is no full-log replay after it.
        (capsule, Vec::new(), BTreeMap::new(), row.last_seq)
    } else {
        let reader = SessionLog::open_read(&deps.paths, session)?;
        let events = reader.read_all()?;
        let count = if tail == 0 { DEFAULT_TAIL } else { tail.min(MAX_TAIL) };
        let eligible: Vec<EventEnvelope> = events.into_iter().filter(is_tail_event).collect();
        let start = eligible.len().saturating_sub(count);
        let tail_events = eligible[start..].to_vec();

        let capsule = CapsuleWriter::new(deps.clone()).write(session, CapsuleTrigger::Export)?;

        let mut blobs_inline = BTreeMap::new();
        for event in &tail_events {
            for blob in blob_refs(event) {
                if blob.bytes > INLINE_BLOB_MAX || blobs_inline.contains_key(&blob.sha256) {
                    continue;
                }
                match reader.blobs().get(&blob.sha256) {
                    Ok(bytes) => {
                        blobs_inline.insert(blob.sha256.clone(), base64_encode(&bytes));
                    }
                    Err(vowp_store::StoreError::NotFound { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        (capsule, tail_events, blobs_inline, reader.last_seq()?)
    };

    // The writer persists before the caller can inspect the capsule, so the export path
    // applies the same final redaction the session service applies to its capsules.
    let capsule = crate::vault::redact_capsule(deps, &store, &capsule)?;

    row.last_seq = last_seq;
    row.log_bytes = if compacted {
        0
    } else {
        std::fs::metadata(deps.paths.log_path(session))
            .map(|meta| meta.len())
            .unwrap_or(row.log_bytes)
    };
    row.last_capsule_id = Some(capsule.capsule_id.clone());

    let mut bundle = Bundle {
        format: BUNDLE_FORMAT.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        exported_at: deps.clock.now(),
        source_node: deps.node.clone(),
        session: row,
        capsule,
        tail: tail_events,
        lineage: store.list_switches(session)?,
        blobs_inline,
        compacted_at: if compacted { compacted_at } else { None },
        checksum: String::new(),
    };
    bundle.checksum = checksum(&bundle)?;
    Ok(bundle)
}

/// Returns a session's latest capsule, falling back to the authoritative capsule files.
///
/// Compaction keeps the capsule files even when the index cache is rebuilt, so the file
/// scan is what makes an export possible from a compacted session whose row is missing.
fn latest_capsule(store: &Store, paths: &NodePaths, session: &SessionId) -> Result<Option<Capsule>, CoreError> {
    if let Some(capsule) = store.latest_capsule(session)? {
        return Ok(Some(capsule));
    }
    let mut latest: Option<Capsule> = None;
    for path in list_capsule_files(paths, session)? {
        let capsule = read_capsule_file(&path)?;
        if capsule.session_id != *session {
            continue;
        }
        let newer = latest
            .as_ref()
            .is_none_or(|current| (capsule.at_seq, capsule.created_at) > (current.at_seq, current.created_at));
        if newer {
            latest = Some(capsule);
        }
    }
    Ok(latest)
}

/// Serialises a bundle as pretty JSON.
pub fn to_json(bundle: &Bundle) -> Result<String, CoreError> {
    serde_json::to_string_pretty(bundle).map_err(json_error)
}

/// Parses a bundle and verifies its format, protocol version and checksum.
///
/// A rejected document changes no state.
pub fn from_json(text: &str) -> Result<Bundle, CoreError> {
    let bundle: Bundle = serde_json::from_str(text).map_err(json_error)?;
    bundle.validate()?;
    Ok(bundle)
}

/// Returns the source head a bundle reports for a divergence decision.
///
/// The head's sequence number, event type and timestamp come from the last replayable tail
/// event; state and executor come from the exported row. A compacted bundle has an empty
/// tail, so its row still supplies `last_seq` and `updated_at` and there is no last event.
pub fn head(bundle: &Bundle) -> SessionHead {
    let mut head = SessionHead::from_session(&bundle.session, bundle.tail.last());
    head.node = bundle.source_node.clone();
    if let Some(last) = bundle.tail.last() {
        head.last_seq = last.seq;
    }
    head
}

/// How an import chooses the target id and parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Create a fresh child session whose parent is the origin (the default).
    Child,
    /// Keep the origin session id, with no parent; refuse when it exists locally.
    SameId,
}

/// Imports a bundle as a new child session on this node and returns its id.
///
/// The child log contains exactly `session.created` (with `parent_session_id`),
/// `note`, `switch` and `capsule`; the bundle tail is *not* copied. `executor` and
/// `cwd` are the `vowp session import --executor/--cwd` overrides.
pub fn import(
    deps: &Deps,
    bundle: &Bundle,
    executor: Option<ExecutorRef>,
    cwd: Option<&Path>,
) -> Result<SessionId, CoreError> {
    import_with_mode(deps, bundle, executor, cwd, ImportMode::Child)
}

/// Imports a bundle with an explicit [`ImportMode`].
///
/// [`ImportMode::SameId`] preserves the origin id only when neither a session row nor a
/// session directory exists locally; a duplicate is a conflict and writes nothing. It
/// writes a fresh `session.created` with no parent, then every other replayable tail
/// event and the capsule, with fresh contiguous sequence numbers and the original event
/// ids and timestamps. A duplicate event id is a conflict, not a warning.
pub fn import_with_mode(
    deps: &Deps,
    bundle: &Bundle,
    executor: Option<ExecutorRef>,
    cwd: Option<&Path>,
    mode: ImportMode,
) -> Result<SessionId, CoreError> {
    bundle.validate()?;
    // Decode and verify the origin blobs before redaction re-keys them. The redacted
    // bytes returned by `redact_bundle` are what the imported store receives.
    let origin_blobs = decode_blobs(bundle)?;
    // Redact before anything is persisted. A bundle may predate the local vault (or come
    // from a node that never knew these secrets); if the redactor cannot be loaded the
    // import fails with no raw capsule file, row or sidecar left behind.
    let (bundle, inline) = redact_bundle(deps, bundle, origin_blobs)?;
    let store = deps.open_store()?;
    let target = target_executor(deps, &bundle, executor);
    let cwd = resolve_cwd(deps, &bundle, cwd);
    let origin = bundle.session.id.clone();

    if mode == ImportMode::SameId {
        return import_same_id(deps, &store, &bundle, &target, cwd, inline);
    }

    // The origin row may name a project/job this node has never seen. The session
    // index has foreign keys to both tables, so an absent row becomes NULL exactly as
    // reindex does instead of failing the import with a foreign-key error.
    let project = match &bundle.session.project {
        Some(name) if store.project(name)?.is_some() => Some(name.clone()),
        _ => None,
    };
    let job_id = match &bundle.session.job_id {
        Some(id) if store.job(id)?.is_some() => Some(id.clone()),
        _ => None,
    };

    let child = SessionService::new(deps.clone()).create_session(NewSession {
        project,
        job_id,
        role: bundle.session.role.unwrap_or(vowp_protocol::SessionRole::User),
        title: format!("{} (imported)", bundle.session.title),
        cwd,
        parent: Some(origin.clone()),
        executor: Some(target.executor.clone()),
    })?;

    let log = SessionLog::open(&deps.paths, &child, deps.clock.clone())?;
    let note_text = format!(
        "imported from {} session {} at seq {} (origin capsule {})",
        bundle.source_node, origin, bundle.session.last_seq, bundle.capsule.capsule_id
    );
    let note = log.append(NewEvent {
        actor: Actor::System,
        source: vowp_source(deps),
        body: EventBody::Note(Note { text: note_text }),
        raw: None,
        ts: None,
    })?;
    store.index_event(&index_row(&note))?;

    let mut capsule = bundle.capsule.clone();
    capsule.capsule_id = CapsuleId::new();
    capsule.session_id = child.clone();
    let from = bundle
        .session
        .executor
        .clone()
        .map(|executor| ExecutorRef {
            executor,
            node: Some(bundle.source_node.clone()),
        })
        .or_else(|| Some(bundle.capsule.from.clone()));

    let switch_event = log.append(NewEvent {
        actor: Actor::System,
        source: vowp_source(deps),
        body: EventBody::Switch(SwitchEvent {
            switch_id: SwitchId::new(),
            from,
            to: target.clone(),
            capsule_id: Some(capsule.capsule_id.clone()),
            reason: SwitchReason::User,
        }),
        raw: None,
        ts: None,
    })?;
    store.index_event(&index_row(&switch_event))?;

    let path = write_capsule_file(&deps.paths, &capsule)?;
    store.insert_capsule(&capsule, &path.to_string_lossy())?;
    // `capsule` already carries the local redactions applied to the whole bundle above,
    // so the file and cached row are never written with a raw value first.
    let capsule_event = log.append(NewEvent {
        actor: Actor::System,
        source: vowp_source(deps),
        body: EventBody::Capsule(CapsuleEvent {
            capsule_id: capsule.capsule_id.clone(),
            version: capsule.version,
            trigger: capsule.trigger,
            at_seq: capsule.at_seq,
        }),
        raw: None,
        ts: None,
    })?;
    store.index_event(&index_row(&capsule_event))?;
    log.sync()?;
    drop(log);

    let blobs = BlobStore::new(deps.paths.blobs_dir(&child));
    for (sha256, bytes) in inline {
        let stored = blobs.put(&bytes)?;
        if stored.sha256 != sha256 {
            return Err(CoreError::Invalid(format!(
                "blob {sha256} hashes to {} on import",
                stored.sha256
            )));
        }
    }

    reindex(&store, &deps.paths, Some(&child))?;
    store.set_session_state(&child, SessionState::Idle, Some(&target.executor))?;
    write_import_sidecar(deps, &child, &bundle)?;
    Ok(child)
}

/// Imports with the origin id, no parent and the bundle's replayable tail.
///
/// Every check that can fail without touching files runs first: the local-absence check,
/// the event-body parse, the duplicate id check and the duplicate event id lookup. The
/// log itself is created with `create_new`, so a second import of the same id conflicts
/// even if two processes race.
fn import_same_id(
    deps: &Deps,
    store: &Store,
    bundle: &Bundle,
    target: &ExecutorRef,
    cwd: PathBuf,
    inline: Vec<DecodedBlob>,
) -> Result<SessionId, CoreError> {
    let origin = bundle.session.id.clone();
    if store.session(&origin)?.is_some() || deps.paths.session_dir(&origin).exists() {
        return Err(CoreError::Store(StoreError::Conflict(format!(
            "session {origin} already exists locally; --same-id refuses to overwrite"
        ))));
    }
    check_same_id_events(store, &origin, &bundle.tail)?;

    // The origin row may name a project/job this node has never seen; an absent row
    // becomes NULL exactly as the child path and reindex do.
    let project = match &bundle.session.project {
        Some(name) if store.project(name)?.is_some() => Some(name.clone()),
        _ => None,
    };
    let job_id = match &bundle.session.job_id {
        Some(id) if store.job(id)?.is_some() => Some(id.clone()),
        _ => None,
    };
    let id = SessionService::new(deps.clone()).create_session_with_id(
        origin.clone(),
        NewSession {
            project,
            job_id,
            role: bundle.session.role.unwrap_or(vowp_protocol::SessionRole::User),
            title: bundle.session.title.clone(),
            cwd,
            parent: None,
            executor: Some(target.executor.clone()),
        },
        bundle.session.created_at,
    )?;
    // `create_session_with_id` refuses a present id, so it returned the origin unchanged.
    debug_assert_eq!(id, origin);

    let log = SessionLog::open(&deps.paths, &id, deps.clock.clone())?;
    for event in &bundle.tail {
        // The importer writes its own `session.created` with no parent; the origin's
        // created event is history, not a second setup event.
        if matches!(event.body(), Ok(EventBody::SessionCreated(_))) {
            continue;
        }
        let mut event = event.clone();
        event.session_id = id.clone();
        event.seq = log.last_seq() + 1;
        log.append_envelope(&event)?;
    }

    let mut capsule = bundle.capsule.clone();
    capsule.session_id = id.clone();
    capsule.at_seq = log.last_seq().max(1);
    let path = write_capsule_file(&deps.paths, &capsule)?;
    store.insert_capsule(&capsule, &path.to_string_lossy())?;
    log.append(NewEvent {
        actor: Actor::System,
        source: vowp_source(deps),
        body: EventBody::Capsule(CapsuleEvent {
            capsule_id: capsule.capsule_id.clone(),
            version: capsule.version,
            trigger: capsule.trigger,
            at_seq: capsule.at_seq,
        }),
        raw: None,
        ts: Some(capsule.created_at),
    })?;
    log.sync()?;
    drop(log);

    let blobs = BlobStore::new(deps.paths.blobs_dir(&id));
    for (sha256, bytes) in inline {
        let stored = blobs.put(&bytes)?;
        if stored.sha256 != sha256 {
            return Err(CoreError::Invalid(format!(
                "blob {sha256} hashes to {} on import",
                stored.sha256
            )));
        }
    }

    let report = reindex(store, &deps.paths, Some(&id))?;
    if !report.problems.is_empty() {
        return Err(CoreError::Store(StoreError::Conflict(format!(
            "same-id import of {id} was not indexed cleanly: {}",
            report.problems.join("; ")
        ))));
    }
    store.set_session_state(&id, SessionState::Idle, Some(&target.executor))?;
    if let Some(at) = bundle.compacted_at {
        store.set_session_compacted(&id, at)?;
    }
    Ok(id)
}

/// Rejects a same-id bundle whose tail is invalid or whose event ids are already indexed.
///
/// The body parse and the duplicate checks happen before `SessionLog::create`, so a
/// rejected import leaves no session row, log or directory behind. Event ids are unique
/// across the whole index (`session_events_id`); a collision with any session is a
/// conflict rather than something `reindex` later reports as a warning.
fn check_same_id_events(store: &Store, origin: &SessionId, tail: &[EventEnvelope]) -> Result<(), CoreError> {
    let mut seen = HashSet::new();
    for event in tail {
        event.body().map_err(|error| {
            CoreError::Invalid(format!(
                "bundle event {} for session {origin} is invalid: {error}",
                event.id
            ))
        })?;
        if !seen.insert(event.id.clone()) {
            return Err(CoreError::Store(StoreError::Conflict(format!(
                "bundle for session {origin} carries duplicate event id {}",
                event.id
            ))));
        }
        if let Some(existing) = store.event_row_by_id(&event.id)? {
            return Err(CoreError::Store(StoreError::Conflict(format!(
                "event {} is already indexed for session {}; refusing to import session {origin}",
                event.id, existing.session_id
            ))));
        }
    }
    Ok(())
}

/// Returns the prefix for a seed on the imported session.
///
/// Import deliberately keeps the bundle tail out of the child log, so the first turn
/// on the child uses this text as its seed: capsule plus the exported tail. The capsule
/// is rendered under the **child** session id, not the origin id. The rendered text is
/// redacted again because a bundle may predate the local vault; a redaction failure
/// withholds the seed instead of risking a raw value.
pub fn imported_seed(deps: &Deps, bundle: &Bundle, child: &SessionId, next_input: &str) -> String {
    let mut capsule = bundle.capsule.clone();
    capsule.session_id = child.clone();
    let seed = seed_prompt(&capsule, &bundle.tail, deps.config.capsule.tail_turns, next_input);
    crate::vault::redact_text(deps, &seed)
}

/// Builds the first-turn seed from the bundle sidecar, when the session has one.
///
/// `import` writes the redacted bundle next to the child log; the first `run_turn`
/// consumes it and then removes it (sessions.md §6.3 step 5).
pub(crate) fn pending_import_seed(
    deps: &Deps,
    child: &SessionId,
    next_input: &str,
) -> Result<Option<String>, CoreError> {
    let path = import_sidecar_path(deps, child);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CoreError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let bundle: Bundle = serde_json::from_slice(&bytes).map_err(json_error)?;
    bundle.validate()?;
    Ok(Some(imported_seed(deps, &bundle, child, next_input)))
}

/// Removes the bundle sidecar after the child's first turn was framed.
pub(crate) fn clear_pending_import(deps: &Deps, child: &SessionId) -> Result<(), CoreError> {
    let path = import_sidecar_path(deps, child);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Returns `<session dir>/import.json`.
fn import_sidecar_path(deps: &Deps, child: &SessionId) -> PathBuf {
    deps.paths.session_dir(child).join(IMPORT_SIDECAR)
}

/// Writes the redacted bundle beside the child log.
fn write_import_sidecar(deps: &Deps, child: &SessionId, bundle: &Bundle) -> Result<(), CoreError> {
    let path = import_sidecar_path(deps, child);
    let bytes = serde_json::to_vec_pretty(bundle).map_err(json_error)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Returns a copy of `bundle` with every payload secret value replaced.
///
/// Import uses this before persisting anything: a bundle can carry values from a node
/// that never knew the local secrets, and neither the capsule file/row, the child blob
/// store nor the import sidecar may hold them. Only free-text payloads are redacted;
/// structural fields such as the bundle format, ids, timestamps and typed enums keep
/// their original values, so a secret that happens to match one of them cannot make the
/// sidecar unparseable or change the imported session identity. Inline blobs are redacted
/// when their bytes are valid UTF-8 (text or JSON); invalid UTF-8 is binary and passes
/// through unchanged. Redaction changes a blob's digest, so the inline map is re-keyed and
/// matching `BlobRef`s in the tail are rewritten to the new digest, size and preview. The
/// checksum is recomputed so the sidecar keeps the unchanged bundle format and still
/// passes [`Bundle::validate`].
fn redact_bundle(
    deps: &Deps,
    bundle: &Bundle,
    blobs: Vec<DecodedBlob>,
) -> Result<(Bundle, Vec<DecodedBlob>), CoreError> {
    let redactor = crate::vault::Redactor::load(deps)?;
    if redactor.is_empty() {
        return Ok((bundle.clone(), blobs));
    }
    let mut redacted = bundle.clone();
    redacted.session.title = redactor.redact(&bundle.session.title);
    redacted.session.cwd = redactor.redact(&bundle.session.cwd);
    redact_capsule_payload(&redactor, &mut redacted.capsule);
    for event in &mut redacted.tail {
        redact_json_strings(&redactor, &mut event.payload);
        if let Some(raw) = &mut event.raw {
            redact_json_strings(&redactor, raw);
        }
    }
    for switch in &mut redacted.lineage {
        if let Some(error) = &mut switch.error {
            *error = redactor.redact(error);
        }
    }
    let inline = redact_blobs(&redactor, &mut redacted, blobs)?;
    redacted.checksum = checksum(&redacted)?;
    Ok((redacted, inline))
}

/// Redacts decoded inline blobs and rewrites the bundle's blob references.
///
/// A blob is text when its bytes are valid UTF-8. Text that parses as JSON is redacted
/// through its string values and re-serialized, so both the raw secret and its JSON-escaped
/// form are replaced; other text is redacted as plain text. Invalid UTF-8 is left untouched
/// as binary, the conservative choice for content that cannot be classified safely. Because
/// redaction changes the content digest, each changed blob gets a new `blobs_inline` key and
/// the tail's `BlobRef`s are rewritten; unchanged blobs keep their digest and bytes.
fn redact_blobs(
    redactor: &crate::vault::Redactor,
    bundle: &mut Bundle,
    blobs: Vec<DecodedBlob>,
) -> Result<Vec<DecodedBlob>, CoreError> {
    if blobs.is_empty() {
        return Ok(blobs);
    }
    let mut remapped: BTreeMap<String, BlobRef> = BTreeMap::new();
    let mut redacted_blobs = Vec::with_capacity(blobs.len());
    bundle.blobs_inline.clear();
    for (old_sha256, bytes) in blobs {
        let redacted_bytes = redact_blob_bytes(redactor, &bytes)?;
        let new_sha256 = hex_digest(&redacted_bytes);
        if new_sha256 != old_sha256.to_ascii_lowercase() {
            remapped.insert(
                old_sha256.to_ascii_lowercase(),
                BlobRef {
                    sha256: new_sha256.clone(),
                    bytes: redacted_bytes.len() as u64,
                    preview: blob_preview(&redacted_bytes),
                },
            );
        }
        bundle
            .blobs_inline
            .insert(new_sha256.clone(), base64_encode(&redacted_bytes));
        redacted_blobs.push((new_sha256, redacted_bytes));
    }
    if !remapped.is_empty() {
        for event in &mut bundle.tail {
            remap_event_blob_refs(event, &remapped);
        }
    }
    Ok(redacted_blobs)
}

/// Redacts one inline blob's bytes according to the text/JSON/binary rule.
fn redact_blob_bytes(redactor: &crate::vault::Redactor, bytes: &[u8]) -> Result<Vec<u8>, CoreError> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(bytes.to_vec());
    };
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) {
        let original = value.clone();
        redact_json_strings(redactor, &mut value);
        if value == original {
            return Ok(bytes.to_vec());
        }
        return serde_json::to_vec(&value).map_err(json_error);
    }
    let redacted = redactor.redact(text);
    if redacted == text {
        return Ok(bytes.to_vec());
    }
    Ok(redacted.into_bytes())
}

/// Returns the display preview of redacted blob content, matching the blob store's rule.
fn blob_preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(BLOB_PREVIEW_CHARS)
        .collect()
}

/// Rewrites the `BlobRef`s carried by one event after its blob content changed.
fn remap_event_blob_refs(event: &mut EventEnvelope, remapped: &BTreeMap<String, BlobRef>) {
    let Ok(mut body) = event.body() else {
        return;
    };
    let mut changed = false;
    match &mut body {
        EventBody::ToolResult(payload) => {
            if let Some(reference) = &mut payload.output_ref {
                changed = remap_blob_ref(reference, remapped);
            }
        }
        EventBody::FileChange(payload) => {
            if let Some(reference) = &mut payload.diff_ref {
                changed = remap_blob_ref(reference, remapped);
            }
        }
        EventBody::UserMessage(payload) => {
            for reference in &mut payload.attachments {
                changed |= remap_blob_ref(reference, remapped);
            }
        }
        _ => {}
    }
    if changed {
        event.payload = body.to_value();
    }
}

/// Replaces one reference with its redacted counterpart, when it was re-keyed.
fn remap_blob_ref(reference: &mut BlobRef, remapped: &BTreeMap<String, BlobRef>) -> bool {
    let Some(replacement) = remapped.get(&reference.sha256.to_ascii_lowercase()) else {
        return false;
    };
    *reference = replacement.clone();
    true
}

/// Redacts the free-text fields of one capsule, leaving ids, enums and telemetry alone.
fn redact_capsule_payload(redactor: &crate::vault::Redactor, capsule: &mut Capsule) {
    capsule.intent = redactor.redact(&capsule.intent);
    for text in &mut capsule.constraints {
        *text = redactor.redact(text);
    }
    for decision in &mut capsule.decisions {
        decision.what = redactor.redact(&decision.what);
        decision.why = redactor.redact(&decision.why);
    }
    redact_strings(redactor, &mut capsule.state.done);
    redact_strings(redactor, &mut capsule.state.in_progress);
    redact_strings(redactor, &mut capsule.state.blocked);
    redact_strings(redactor, &mut capsule.state.next_steps);
    for file in &mut capsule.files {
        file.path = redactor.redact(&file.path);
        file.note = redactor.redact(&file.note);
    }
    redact_strings(redactor, &mut capsule.gotchas);
    redact_strings(redactor, &mut capsule.open_questions);
}

/// Redacts every string in one list.
fn redact_strings(redactor: &crate::vault::Redactor, texts: &mut [String]) {
    for text in texts {
        *text = redactor.redact(text);
    }
}

/// Redacts string values inside one decoded JSON value without touching object keys.
fn redact_json_strings(redactor: &crate::vault::Redactor, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact(text),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_strings(redactor, item);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                redact_json_strings(redactor, item);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Returns whether an event belongs in a bundle tail.
fn is_tail_event(event: &EventEnvelope) -> bool {
    event.replayable
        || matches!(
            event.event_type,
            EventType::UserMessage
                | EventType::AssistantMessage
                | EventType::ToolCall
                | EventType::ToolResult
                | EventType::Switch
                | EventType::Capsule
                | EventType::Limit
        )
}

/// Collects the blob references carried by one event.
fn blob_refs(event: &EventEnvelope) -> Vec<BlobRef> {
    let Ok(body) = event.body() else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    match body {
        EventBody::ToolResult(payload) => {
            if let Some(reference) = payload.output_ref {
                refs.push(reference);
            }
        }
        EventBody::FileChange(payload) => {
            if let Some(reference) = payload.diff_ref {
                refs.push(reference);
            }
        }
        EventBody::UserMessage(payload) => refs.extend(payload.attachments),
        _ => {}
    }
    refs
}

/// Decodes and verifies every inline blob before a session is created.
fn decode_blobs(bundle: &Bundle) -> Result<Vec<DecodedBlob>, CoreError> {
    let mut decoded = Vec::with_capacity(bundle.blobs_inline.len());
    for (sha256, encoded) in &bundle.blobs_inline {
        let bytes = base64_decode(encoded)?;
        let actual = hex_digest(&bytes);
        if actual != sha256.to_ascii_lowercase() {
            return Err(CoreError::Invalid(format!(
                "blob digest mismatch: expected {sha256}, content hashes to {actual}"
            )));
        }
        decoded.push((sha256.clone(), bytes));
    }
    Ok(decoded)
}

/// Chooses the executor the imported session continues on.
fn target_executor(deps: &Deps, bundle: &Bundle, executor: Option<ExecutorRef>) -> ExecutorRef {
    match executor {
        Some(mut chosen) => {
            if chosen.node.is_none() {
                chosen.node = Some(deps.node.clone());
            }
            chosen
        }
        None => {
            let executor = bundle
                .session
                .executor
                .clone()
                .unwrap_or_else(|| bundle.capsule.from.executor.clone());
            ExecutorRef {
                executor,
                node: Some(deps.node.clone()),
            }
        }
    }
}

/// Resolves the child working directory: explicit flag, allowed origin cwd, project path.
fn resolve_cwd(deps: &Deps, bundle: &Bundle, explicit: Option<&Path>) -> PathBuf {
    if let Some(cwd) = explicit {
        return cwd.to_path_buf();
    }
    let origin = Path::new(&bundle.session.cwd);
    if allowed_cwd(deps, origin) {
        return origin.to_path_buf();
    }
    deps.config
        .project
        .path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| origin.to_path_buf())
}

/// Returns whether `[node].cwd_allow` admits `cwd` (an empty list admits nothing).
fn allowed_cwd(deps: &Deps, cwd: &Path) -> bool {
    if deps.config.node.cwd_allow.is_empty() {
        return false;
    }
    let path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    deps.config.node.cwd_allow.iter().any(|root| {
        let root = Path::new(root);
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        path.starts_with(root)
    })
}

/// Returns the VOWP system event source used by import framing events.
fn vowp_source(deps: &Deps) -> EventSource {
    EventSource {
        harness: HarnessKind::Vowp,
        model: None,
        node: Some(deps.node.clone()),
        native_session_id: None,
    }
}

/// Maps a serde_json failure into the core protocol error.
fn json_error(error: serde_json::Error) -> CoreError {
    CoreError::Protocol(vowp_protocol::ProtocolError::from(error))
}

/// Computes `"sha256:<hex>"` over the canonical bundle without the checksum field.
fn checksum(bundle: &Bundle) -> Result<String, CoreError> {
    let mut value = serde_json::to_value(bundle).map_err(json_error)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("checksum");
    }
    let canonical = serde_json::to_vec(&value).map_err(json_error)?;
    Ok(format!("sha256:{}", hex_digest(&canonical)))
}

/// Returns the lowercase hex SHA-256 digest of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The standard base64 alphabet.
const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes bytes as padded standard base64.
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(BASE64_ALPHABET[(b[0] >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b[0] & 0b11) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[(((b[1] & 0b1111) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(b[2] & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decodes padded standard base64.
fn base64_decode(text: &str) -> Result<Vec<u8>, CoreError> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(CoreError::Invalid(
            "invalid base64: length is not a multiple of 4".to_owned(),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let a = b64_value(chunk[0])?;
        let b = b64_value(chunk[1])?;
        let c = if chunk[2] == b'=' { 0 } else { b64_value(chunk[2])? };
        let d = if chunk[3] == b'=' { 0 } else { b64_value(chunk[3])? };
        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err(CoreError::Invalid("invalid base64 padding".to_owned()));
        }
        if (chunk[2] == b'=' || chunk[3] == b'=') && !last {
            return Err(CoreError::Invalid("invalid base64 padding".to_owned()));
        }
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push(((b & 0b1111) << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push(((c & 0b11) << 6) | d);
        }
    }
    Ok(out)
}

/// Maps one base64 character to its six-bit value.
fn b64_value(byte: u8) -> Result<u8, CoreError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(CoreError::Invalid(format!(
            "invalid base64 character {:?}",
            byte as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;
    use vowp_protocol::{AssistantMessage, Executor, FakeClock, MemoryLog, SessionCreated, SessionRole, UserMessage};
    use vowp_tmux::fake::FakeWindows;

    use super::*;
    use crate::deps::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};

    fn ts() -> Timestamp {
        Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp")
    }

    fn executor() -> Executor {
        Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor")
    }

    fn test_deps(root: &Path) -> Deps {
        let paths = NodePaths::new(root.join("node"));
        paths.ensure().expect("node paths");
        let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        config.project.path = Some(project.display().to_string());
        Deps {
            paths,
            config: Arc::new(config),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(FakeClock::at(ts())),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        }
    }

    fn new_request(title: &str) -> NewSession {
        NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: title.to_owned(),
            cwd: PathBuf::from("/tmp/project"),
            parent: None,
            executor: Some(executor()),
        }
    }

    fn event_source() -> EventSource {
        EventSource {
            harness: HarnessKind::Opencode,
            model: Some("deepseek/deepseek-flash".to_owned()),
            node: Some(NodeName::parse("source").expect("node")),
            native_session_id: None,
        }
    }

    fn made_event(session: &SessionId, seq: u64, body: EventBody) -> EventEnvelope {
        EventEnvelope::new(session.clone(), seq, ts(), Actor::User, event_source(), &body)
    }

    /// One bundle whose origin row has a parent (the import must not keep it) and three
    /// replayable tail events: created, user message and assistant message.
    fn test_bundle(source: &SessionId) -> (Bundle, EventEnvelope) {
        let created = made_event(
            source,
            1,
            EventBody::SessionCreated(SessionCreated {
                title: "same-id source".to_owned(),
                cwd: "/tmp/source".to_owned(),
                project: None,
                job_id: None,
                role: Some(SessionRole::Worker),
                parent_session_id: Some(SessionId::new()),
            }),
        );
        let user = made_event(
            source,
            2,
            EventBody::UserMessage(UserMessage {
                text: "copy this event".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
        );
        let assistant = made_event(
            source,
            3,
            EventBody::AssistantMessage(AssistantMessage {
                text: "copied".to_owned(),
            }),
        );
        let tail = vec![created, user.clone(), assistant];
        let capsule = Capsule {
            version: vowp_protocol::CAPSULE_VERSION,
            capsule_id: CapsuleId::new(),
            session_id: source.clone(),
            created_at: ts(),
            at_seq: 3,
            trigger: CapsuleTrigger::Export,
            from: ExecutorRef {
                executor: executor(),
                node: Some(NodeName::parse("source").expect("node")),
            },
            intent: "Continue the same session".to_owned(),
            constraints: Vec::new(),
            decisions: Vec::new(),
            state: Default::default(),
            files: Vec::new(),
            gotchas: Vec::new(),
            open_questions: Vec::new(),
            telemetry: Default::default(),
        };
        let session = Session {
            id: source.clone(),
            project: None,
            job_id: None,
            role: Some(SessionRole::Worker),
            title: "same-id source".to_owned(),
            parent_session_id: Some(SessionId::new()),
            node: NodeName::parse("source").expect("node"),
            cwd: "/tmp/source".to_owned(),
            state: SessionState::Idle,
            executor: Some(executor()),
            last_seq: 3,
            last_capsule_id: Some(capsule.capsule_id.clone()),
            log_bytes: 0,
            created_at: ts(),
            updated_at: ts(),
        };
        let mut bundle = Bundle {
            format: BUNDLE_FORMAT.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            exported_at: ts(),
            source_node: NodeName::parse("source").expect("node"),
            session,
            capsule,
            tail,
            lineage: Vec::new(),
            blobs_inline: BTreeMap::new(),
            compacted_at: None,
            checksum: String::new(),
        };
        bundle.checksum = checksum(&bundle).expect("checksum");
        (bundle, user)
    }

    #[test]
    fn base64_roundtrip() {
        for input in [b"".as_slice(), b"a", b"ab", b"abc", b"abcd", &[0_u8, 255, 128, 1, 2, 3]] {
            let encoded = base64_encode(input);
            assert_eq!(base64_decode(&encoded).expect("decode"), input);
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert!(base64_decode("a").is_err());
        assert!(base64_decode("****").is_err());
    }

    #[test]
    fn bundle_head_names_the_source_row_and_last_tail_event() {
        let source = SessionId::new();
        let (bundle, _user) = test_bundle(&source);

        let head = head(&bundle);
        assert_eq!(head.session_id, source);
        assert_eq!(head.node, bundle.source_node);
        assert_eq!(head.last_seq, 3);
        assert_eq!(head.last_event, Some(EventType::AssistantMessage));
        assert_eq!(head.last_event_at, Some(ts()));
        assert_eq!(head.state, Some(SessionState::Idle));
        assert_eq!(head.executor, Some(executor()));
        assert_eq!(head.updated_at, Some(ts()));
    }

    #[test]
    fn bundle_head_of_a_compacted_bundle_has_no_tail_event() {
        let source = SessionId::new();
        let (mut bundle, _user) = test_bundle(&source);
        bundle.tail.clear();
        bundle.compacted_at = Some(ts());
        bundle.checksum = checksum(&bundle).expect("checksum");

        let head = head(&bundle);
        assert_eq!(head.last_seq, 3, "the exported row still names the last seq");
        assert_eq!(head.last_event, None);
        assert_eq!(head.last_event_at, None);
        assert_eq!(head.updated_at, Some(ts()));
    }

    #[test]
    fn same_id_import_keeps_the_origin_id_without_a_parent() {
        let dest_dir = tempdir().expect("dest");
        let source = SessionId::new();
        let (bundle, user) = test_bundle(&source);
        let capsule_id = bundle.capsule.capsule_id.clone();
        let dest = test_deps(dest_dir.path());

        let imported = import_with_mode(&dest, &bundle, None, None, ImportMode::SameId).expect("same-id import");
        assert_eq!(imported, source);

        let store = dest.open_store().expect("store");
        let row = store.require_session(&source).expect("session row");
        assert!(row.parent_session_id.is_none(), "a same-id import has no parent");
        assert_eq!(row.title, "same-id source");
        assert_eq!(row.executor, Some(executor()));
        assert_eq!(
            row.created_at, bundle.session.created_at,
            "the origin creation time is kept"
        );

        let events = SessionLog::open_read(&dest.paths, &source)
            .expect("log")
            .read_all()
            .expect("events");
        assert_eq!(
            events.first().map(|event| event.event_type),
            Some(EventType::SessionCreated)
        );
        assert!(matches!(
            events[0].body().expect("created body"),
            EventBody::SessionCreated(payload) if payload.parent_session_id.is_none()
        ));
        let copied = events
            .iter()
            .find(|event| event.id == user.id)
            .expect("the bundle tail is copied into the log");
        assert_eq!(copied.payload, user.payload);
        assert_eq!(copied.ts, user.ts);
        assert_eq!(events.last().map(|event| event.event_type), Some(EventType::Capsule));

        let indexed = store.event_rows(&source, 0, 100).expect("index rows");
        assert_eq!(
            indexed.len(),
            events.len(),
            "the index mirrors the copied log: {indexed:?} vs {events:?}"
        );
        let capsule = store
            .latest_capsule(&source)
            .expect("capsule query")
            .expect("capsule row");
        assert_eq!(capsule.capsule_id, capsule_id);
        assert_eq!(capsule.session_id, source);
        assert_eq!(
            capsule.at_seq,
            events[events.len() - 2].seq,
            "the capsule covers the copied tail, not its own event"
        );
        assert!(
            !dest.paths.session_dir(&source).join("import.json").exists(),
            "a same-id import copies the tail instead of writing a seed sidecar"
        );
    }

    #[test]
    fn same_id_import_refuses_an_existing_session_and_changes_nothing() {
        let dest_dir = tempdir().expect("dest");
        let source = SessionId::new();
        let (bundle, _) = test_bundle(&source);
        let dest = test_deps(dest_dir.path());
        import_with_mode(&dest, &bundle, None, None, ImportMode::SameId).expect("first import");

        let log_path = dest.paths.log_path(&source);
        let before_log = std::fs::read(&log_path).expect("log bytes");
        let store = dest.open_store().expect("store");
        let before_row = store.require_session(&source).expect("row");
        drop(store);

        let error =
            import_with_mode(&dest, &bundle, None, None, ImportMode::SameId).expect_err("a duplicate id must conflict");
        match error {
            CoreError::Store(StoreError::Conflict(message)) => {
                assert!(message.contains("already exists"), "{message}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }

        assert_eq!(std::fs::read(&log_path).expect("log bytes"), before_log);
        let store = dest.open_store().expect("store");
        let after_row = store.require_session(&source).expect("row");
        assert_eq!(after_row.last_seq, before_row.last_seq);
        assert_eq!(after_row.updated_at, before_row.updated_at);
        assert_eq!(after_row.title, before_row.title);
        assert_eq!(
            store
                .list_sessions(&vowp_store::SessionFilter::default())
                .expect("sessions")
                .len(),
            1,
            "the refused import created no second session"
        );
    }

    #[test]
    fn same_id_import_rejects_a_duplicate_event_id_before_writing() {
        let dest_dir = tempdir().expect("dest");
        let dest = test_deps(dest_dir.path());
        let other = SessionService::new(dest.clone())
            .create_session(new_request("already indexed"))
            .expect("other session");
        let other_events = SessionLog::open_read(&dest.paths, &other)
            .expect("log")
            .read_all()
            .expect("events");
        let existing_id = other_events[0].id.clone();

        let source = SessionId::new();
        let mut duplicated = made_event(
            &source,
            2,
            EventBody::UserMessage(UserMessage {
                text: "collides with another session".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
        );
        duplicated.id = existing_id;
        let mut bundle = test_bundle(&source).0;
        bundle.tail = vec![duplicated];
        bundle.checksum = checksum(&bundle).expect("checksum");

        let error = import_with_mode(&dest, &bundle, None, None, ImportMode::SameId)
            .expect_err("a duplicate event id must conflict");
        match error {
            CoreError::Store(StoreError::Conflict(message)) => {
                assert!(message.contains("already indexed"), "{message}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        let store = dest.open_store().expect("store");
        assert!(
            store.session(&source).expect("lookup").is_none(),
            "the refused import created no session row"
        );
        assert!(
            !dest.paths.session_dir(&source).exists(),
            "the duplicate check runs before the session directory"
        );
    }

    #[test]
    fn same_id_import_rejects_a_corrupt_bundle_before_writing() {
        let dest_dir = tempdir().expect("dest");
        let source = SessionId::new();
        let (mut bundle, _) = test_bundle(&source);
        bundle.session.title = "tampered".to_owned();
        let dest = test_deps(dest_dir.path());

        let error =
            import_with_mode(&dest, &bundle, None, None, ImportMode::SameId).expect_err("a tampered bundle must fail");
        assert!(
            matches!(&error, CoreError::Invalid(message) if message.contains("checksum mismatch")),
            "{error:?}"
        );
        let store = dest.open_store().expect("store");
        assert!(store.session(&source).expect("lookup").is_none());
        assert!(!dest.paths.session_dir(&source).exists());
    }

    #[test]
    fn child_import_still_creates_a_child_with_the_parent() {
        let dest_dir = tempdir().expect("dest");
        let source = SessionId::new();
        let (bundle, _) = test_bundle(&source);
        let dest = test_deps(dest_dir.path());

        let child = import(&dest, &bundle, None, None).expect("child import");
        assert_ne!(child, source);
        let store = dest.open_store().expect("store");
        let row = store.require_session(&child).expect("child row");
        assert_eq!(row.parent_session_id.as_ref(), Some(&source));
        assert_eq!(row.title, "same-id source (imported)");
        let events = SessionLog::open_read(&dest.paths, &child)
            .expect("log")
            .read_all()
            .expect("events");
        assert_eq!(
            events.iter().map(|event| event.event_type).collect::<Vec<_>>(),
            vec![
                EventType::SessionCreated,
                EventType::Note,
                EventType::Switch,
                EventType::Capsule
            ],
            "the default child log keeps its four setup events"
        );
    }
}
