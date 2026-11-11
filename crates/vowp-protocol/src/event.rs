//! Event envelopes, the typed payload enum and harness-emitted events.
//! Spec: `docs/design/types.md` §5 and `docs/design/crates/vowp-protocol.md` §4 rules 1–3.
//!
//! [`EventEnvelope::payload`] is raw JSON so a reader never fails on a payload shape it does
//! not know; [`EventEnvelope::body`] is the strict typed view and [`EventBody::from_parts`]
//! is the inverse of [`EventBody::to_value`] for every event type.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::error::{ErrorKind, ProtocolError};
use crate::executor::{Executor, ExecutorRef, HarnessKind};
use crate::ids::{CapsuleId, EventId, JobId, NodeName, ProjectName, RunId, SessionId, SwitchId};
use crate::limit::LimitKind;
use crate::time::Timestamp;

/// Who produced an event line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The human using VOWP.
    User,
    /// The harness's assistant.
    Assistant,
    /// VOWP itself (session framing, limits, cost, switches, capsules).
    System,
    /// A tool invoked by the assistant.
    Tool,
}

/// The role a session serves. User sessions exist for `vowp session new`.
///
/// Defined in the event module because [`SessionCreated`] needs it; the session domain
/// rows use the same role vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    /// Plans the job graph.
    Planner,
    /// Diagnoses stalled or failed jobs.
    Keeper,
    /// Does job work.
    Worker,
    /// Reviews finished work.
    Verifier,
    /// A human-started session.
    User,
    /// A fleet-wide or project-bound orchestrator session.
    Orchestrator,
}

/// Where an event came from: the harness, and optionally its model, node and native session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSource {
    /// Harness that produced the event; [`HarnessKind::Vowp`] for VOWP's own events.
    pub harness: HarnessKind,
    /// Model name as the harness knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Node the harness ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeName>,
    /// Harness-side session id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
}

/// Every canonical event type, named by its dotted wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum EventType {
    /// A session was created.
    #[serde(rename = "session.created")]
    SessionCreated,
    /// An executor was attached to or detached from a session.
    #[serde(rename = "session.executor")]
    SessionExecutor,
    /// A turn started.
    #[serde(rename = "turn.started")]
    TurnStarted,
    /// A turn ended.
    #[serde(rename = "turn.ended")]
    TurnEnded,
    /// The user sent a message.
    #[serde(rename = "user.message")]
    UserMessage,
    /// The assistant sent a message.
    #[serde(rename = "assistant.message")]
    AssistantMessage,
    /// The assistant emitted reasoning (never replayed).
    #[serde(rename = "assistant.reasoning")]
    AssistantReasoning,
    /// The assistant called a tool.
    #[serde(rename = "tool.call")]
    ToolCall,
    /// A tool returned a result.
    #[serde(rename = "tool.result")]
    ToolResult,
    /// A file was created, modified, deleted or renamed.
    #[serde(rename = "file.change")]
    FileChange,
    /// The assistant published or updated a plan.
    #[serde(rename = "plan")]
    Plan,
    /// A provider limit was observed.
    #[serde(rename = "limit")]
    Limit,
    /// A cost was recorded.
    #[serde(rename = "cost")]
    Cost,
    /// A session switched executors.
    #[serde(rename = "switch")]
    Switch,
    /// A capsule was written.
    #[serde(rename = "capsule")]
    Capsule,
    /// An error was recorded.
    #[serde(rename = "error")]
    Error,
    /// A free-form note.
    #[serde(rename = "note")]
    Note,
}

impl EventType {
    /// Returns the dotted wire string, for example `tool.call`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::SessionExecutor => "session.executor",
            Self::TurnStarted => "turn.started",
            Self::TurnEnded => "turn.ended",
            Self::UserMessage => "user.message",
            Self::AssistantMessage => "assistant.message",
            Self::AssistantReasoning => "assistant.reasoning",
            Self::ToolCall => "tool.call",
            Self::ToolResult => "tool.result",
            Self::FileChange => "file.change",
            Self::Plan => "plan",
            Self::Limit => "limit",
            Self::Cost => "cost",
            Self::Switch => "switch",
            Self::Capsule => "capsule",
            Self::Error => "error",
            Self::Note => "note",
        }
    }

    /// Parses a dotted wire string; unknown names are a [`ProtocolError::InvalidPayload`].
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|source| {
            ProtocolError::InvalidPayload {
                event_type: s.to_owned(),
                source,
            }
        })
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns true; serde default for [`EventEnvelope::replayable`].
fn default_true() -> bool {
    true
}

/// One line of `log.jsonl`.
///
/// The payload is kept as raw JSON so a reader never fails on a payload it cannot type;
/// use [`EventEnvelope::body`] for the typed view and [`EventEnvelope::new`] to build one
/// from a typed [`EventBody`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    /// Event id, assigned by VOWP.
    pub id: EventId,
    /// Session the event belongs to.
    pub session_id: SessionId,
    /// Sequence number within the session: 1, 2, 3, … (the store rejects gaps).
    pub seq: u64,
    /// Event timestamp.
    pub ts: Timestamp,
    /// Event type.
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Who produced the event.
    pub actor: Actor,
    /// Harness and model that produced the event.
    pub source: EventSource,
    /// Raw payload JSON; decode it with [`EventEnvelope::body`].
    pub payload: serde_json::Value,
    /// Original harness event, kept for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// Whether the event may be replayed to a harness; false for reasoning.
    #[serde(default = "default_true")]
    pub replayable: bool,
}

impl EventEnvelope {
    /// Decodes the raw payload into its typed [`EventBody`].
    pub fn body(&self) -> Result<EventBody, ProtocolError> {
        EventBody::from_parts(self.event_type, self.payload.clone())
    }

    /// Builds an envelope for `body`, assigning a fresh [`EventId`].
    ///
    /// `replayable` is false exactly for [`EventBody::AssistantReasoning`].
    pub fn new(
        session_id: SessionId,
        seq: u64,
        ts: Timestamp,
        actor: Actor,
        source: EventSource,
        body: &EventBody,
    ) -> EventEnvelope {
        EventEnvelope {
            id: EventId::new(),
            session_id,
            seq,
            ts,
            event_type: body.event_type(),
            actor,
            source,
            payload: body.to_value(),
            raw: None,
            replayable: !matches!(body, EventBody::AssistantReasoning(_)),
        }
    }
}

/// What an adapter emits: an envelope without id, seq, timestamp or session id; the
/// session layer assigns those.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessEvent {
    /// Harness-reported time, if any.
    pub ts: Option<Timestamp>,
    /// Who produced the event.
    pub actor: Actor,
    /// Typed payload.
    pub body: EventBody,
    /// Original harness event, kept for display only.
    pub raw: Option<serde_json::Value>,
}

/// Typed event payload.
///
/// [`EventBody::to_value`] and [`EventBody::from_parts`] convert to and from the raw
/// `(EventType, serde_json::Value)` pair stored in an [`EventEnvelope`].
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    /// A session was created.
    SessionCreated(SessionCreated),
    /// An executor was attached to or detached from a session.
    SessionExecutor(SessionExecutor),
    /// A turn started.
    TurnStarted(TurnStarted),
    /// A turn ended.
    TurnEnded(TurnEnded),
    /// The user sent a message.
    UserMessage(UserMessage),
    /// The assistant sent a message.
    AssistantMessage(AssistantMessage),
    /// The assistant emitted reasoning.
    AssistantReasoning(AssistantReasoning),
    /// The assistant called a tool.
    ToolCall(ToolCall),
    /// A tool returned a result.
    ToolResult(ToolResult),
    /// A file changed.
    FileChange(FileChange),
    /// A plan was published or updated.
    Plan(PlanEvent),
    /// A provider limit was observed.
    Limit(LimitEvent),
    /// A cost was recorded.
    Cost(CostEvent),
    /// A session switched executors.
    Switch(SwitchEvent),
    /// A capsule was written.
    Capsule(CapsuleEvent),
    /// An error was recorded.
    Error(ErrorEvent),
    /// A free-form note.
    Note(Note),
}

/// Maximum number of characters in an event summary (`session_events.summary`).
const SUMMARY_MAX_CHARS: usize = 200;

impl EventBody {
    /// Returns the event type of this payload.
    pub fn event_type(&self) -> EventType {
        match self {
            Self::SessionCreated(_) => EventType::SessionCreated,
            Self::SessionExecutor(_) => EventType::SessionExecutor,
            Self::TurnStarted(_) => EventType::TurnStarted,
            Self::TurnEnded(_) => EventType::TurnEnded,
            Self::UserMessage(_) => EventType::UserMessage,
            Self::AssistantMessage(_) => EventType::AssistantMessage,
            Self::AssistantReasoning(_) => EventType::AssistantReasoning,
            Self::ToolCall(_) => EventType::ToolCall,
            Self::ToolResult(_) => EventType::ToolResult,
            Self::FileChange(_) => EventType::FileChange,
            Self::Plan(_) => EventType::Plan,
            Self::Limit(_) => EventType::Limit,
            Self::Cost(_) => EventType::Cost,
            Self::Switch(_) => EventType::Switch,
            Self::Capsule(_) => EventType::Capsule,
            Self::Error(_) => EventType::Error,
            Self::Note(_) => EventType::Note,
        }
    }

    /// Serialises the payload to its raw JSON value.
    pub fn to_value(&self) -> serde_json::Value {
        let serialised = match self {
            Self::SessionCreated(payload) => serde_json::to_value(payload),
            Self::SessionExecutor(payload) => serde_json::to_value(payload),
            Self::TurnStarted(payload) => serde_json::to_value(payload),
            Self::TurnEnded(payload) => serde_json::to_value(payload),
            Self::UserMessage(payload) => serde_json::to_value(payload),
            Self::AssistantMessage(payload) => serde_json::to_value(payload),
            Self::AssistantReasoning(payload) => serde_json::to_value(payload),
            Self::ToolCall(payload) => serde_json::to_value(payload),
            Self::ToolResult(payload) => serde_json::to_value(payload),
            Self::FileChange(payload) => serde_json::to_value(payload),
            Self::Plan(payload) => serde_json::to_value(payload),
            Self::Limit(payload) => serde_json::to_value(payload),
            Self::Cost(payload) => serde_json::to_value(payload),
            Self::Switch(payload) => serde_json::to_value(payload),
            Self::Capsule(payload) => serde_json::to_value(payload),
            Self::Error(payload) => serde_json::to_value(payload),
            Self::Note(payload) => serde_json::to_value(payload),
        };
        // These payload types contain only strings, numbers, enums and JSON values, so
        // serialising them cannot fail; keep the documented infallible signature.
        serialised.unwrap_or(serde_json::Value::Null)
    }

    /// Decodes `value` as the payload for event type `t`; extra payload keys are ignored.
    pub fn from_parts(t: EventType, value: serde_json::Value) -> Result<Self, ProtocolError> {
        fn decode<T: serde::de::DeserializeOwned>(t: EventType, value: serde_json::Value) -> Result<T, ProtocolError> {
            serde_json::from_value(value).map_err(|source| ProtocolError::InvalidPayload {
                event_type: t.as_str().to_owned(),
                source,
            })
        }

        Ok(match t {
            EventType::SessionCreated => Self::SessionCreated(decode(t, value)?),
            EventType::SessionExecutor => Self::SessionExecutor(decode(t, value)?),
            EventType::TurnStarted => Self::TurnStarted(decode(t, value)?),
            EventType::TurnEnded => Self::TurnEnded(decode(t, value)?),
            EventType::UserMessage => Self::UserMessage(decode(t, value)?),
            EventType::AssistantMessage => Self::AssistantMessage(decode(t, value)?),
            EventType::AssistantReasoning => Self::AssistantReasoning(decode(t, value)?),
            EventType::ToolCall => Self::ToolCall(decode(t, value)?),
            EventType::ToolResult => Self::ToolResult(decode(t, value)?),
            EventType::FileChange => Self::FileChange(decode(t, value)?),
            EventType::Plan => Self::Plan(decode(t, value)?),
            EventType::Limit => Self::Limit(decode(t, value)?),
            EventType::Cost => Self::Cost(decode(t, value)?),
            EventType::Switch => Self::Switch(decode(t, value)?),
            EventType::Capsule => Self::Capsule(decode(t, value)?),
            EventType::Error => Self::Error(decode(t, value)?),
            EventType::Note => Self::Note(decode(t, value)?),
        })
    }

    /// Returns a one-line summary of at most 200 characters for `session_events.summary`.
    pub fn summary(&self) -> String {
        let raw = match self {
            Self::SessionCreated(payload) => {
                format!("session created: {} ({})", payload.title, payload.cwd)
            }
            Self::SessionExecutor(payload) => {
                let action = wire_name(&payload.action);
                format!("executor {action}: {} on {}", payload.executor, payload.node)
            }
            Self::TurnStarted(payload) => format!("turn {} started", payload.turn),
            Self::TurnEnded(payload) => format!("turn {} ended: {}", payload.turn, wire_name(&payload.stop_reason)),
            Self::UserMessage(payload) => format!("user: {}", payload.text),
            Self::AssistantMessage(payload) => format!("assistant: {}", payload.text),
            Self::AssistantReasoning(payload) => format!("reasoning: {}", payload.text),
            Self::ToolCall(payload) => format!(
                "tool.call {}: {}",
                payload.name,
                serde_json::to_string(&payload.input).unwrap_or_default()
            ),
            Self::ToolResult(payload) => format!(
                "tool.result {}: {}",
                payload.call_id,
                if payload.ok { "ok" } else { "failed" }
            ),
            Self::FileChange(payload) => {
                format!("file.change {}: {}", wire_name(&payload.op), payload.path)
            }
            Self::Plan(payload) => {
                let items = payload
                    .items
                    .iter()
                    .map(|item| format!("{}: {}", wire_name(&item.status), item.text))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("plan: {items}")
            }
            Self::Limit(payload) => format!(
                "limit {} {}: {}",
                wire_name(&payload.kind),
                payload.provider,
                payload.message
            ),
            Self::Cost(payload) => match payload.cost.usd {
                Some(usd) => format!("cost ${usd:.4} ({})", wire_name(&payload.cost.source)),
                None => format!("cost unknown ({})", wire_name(&payload.cost.source)),
            },
            Self::Switch(payload) => {
                let from = payload
                    .from
                    .as_ref()
                    .map(executor_ref_text)
                    .unwrap_or_else(|| "none".to_owned());
                format!(
                    "switch {from} -> {} ({})",
                    executor_ref_text(&payload.to),
                    wire_name(&payload.reason)
                )
            }
            Self::Capsule(payload) => format!(
                "capsule {} v{} ({})",
                payload.capsule_id,
                payload.version,
                wire_name(&payload.trigger)
            ),
            Self::Error(payload) => {
                format!("error {}: {}", wire_name(&payload.kind), payload.message)
            }
            Self::Note(payload) => format!("note: {}", payload.text),
        };
        clip_summary(&raw)
    }
}

/// Returns the serde wire name of an enum value (all callers pass a string-like enum).
fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|json| json.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Formats an executor reference for a one-line summary.
fn executor_ref_text(value: &ExecutorRef) -> String {
    match &value.node {
        Some(node) => format!("{} on {node}", value.executor),
        None => value.executor.to_string(),
    }
}

/// Collapses whitespace to single spaces and truncates to [`SUMMARY_MAX_CHARS`] characters.
fn clip_summary(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SUMMARY_MAX_CHARS {
        return one_line;
    }
    let mut clipped: String = one_line.chars().take(SUMMARY_MAX_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

/// Large content reference. Tool output larger than 8 KiB is spilled to `blobs/<sha256>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlobRef {
    /// Lowercase hex SHA-256 of the blob content.
    pub sha256: String,
    /// Blob size in bytes.
    pub bytes: u64,
    /// First 512 characters of the content, for display.
    pub preview: String,
}

/// Payload of `session.created`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SessionCreated {
    /// Human-readable session title.
    pub title: String,
    /// Working directory the session runs in.
    pub cwd: String,
    /// Project the session belongs to.
    #[serde(default)]
    pub project: Option<ProjectName>,
    /// Job the session works on, if any.
    #[serde(default)]
    pub job_id: Option<JobId>,
    /// Role the session serves.
    #[serde(default)]
    pub role: Option<SessionRole>,
    /// Parent session for sessions created by a switch or import.
    #[serde(default)]
    pub parent_session_id: Option<SessionId>,
}

/// Whether an executor was attached or detached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachAction {
    /// The executor started running the session.
    Attach,
    /// The executor stopped running the session.
    Detach,
}

/// Payload of `session.executor`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SessionExecutor {
    /// Whether the executor was attached or detached.
    pub action: AttachAction,
    /// The executor that was attached or detached.
    pub executor: Executor,
    /// Node the executor runs on.
    pub node: NodeName,
    /// Harness-side session id; known at attach when resuming.
    #[serde(default)]
    pub native_session_id: Option<String>,
    /// Run that used this executor attachment, if any.
    #[serde(default)]
    pub run_id: Option<RunId>,
    /// Why the executor was detached, for example `turn_done`, `limit` or `switch`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Payload of `turn.started`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct TurnStarted {
    /// Turn number within the session.
    pub turn: u32,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn normally.
    EndTurn,
    /// The harness hit its maximum turn count.
    MaxTurns,
    /// A provider limit stopped the turn.
    Limit,
    /// The harness reported an error.
    Error,
    /// A human or the supervisor interrupted the turn.
    Interrupted,
    /// The harness process crashed.
    Crash,
}

/// Payload of `turn.ended`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct TurnEnded {
    /// Turn number within the session.
    pub turn: u32,
    /// Why the turn ended.
    pub stop_reason: StopReason,
    /// Wall-clock duration of the turn.
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Payload of `user.message`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct UserMessage {
    /// Message text.
    pub text: String,
    /// Attached blobs.
    #[serde(default)]
    pub attachments: Vec<BlobRef>,
    /// Why VOWP sent the message, for example `handoff` for capsule seeds.
    #[serde(default)]
    pub note: Option<String>,
}

/// Payload of `assistant.message`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    /// Message text.
    pub text: String,
}

/// Payload of `assistant.reasoning`; such envelopes are never replayable.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct AssistantReasoning {
    /// Reasoning text.
    pub text: String,
}

/// Payload of `tool.call`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Harness-side call id, echoed by the matching [`ToolResult`].
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Tool input as the harness reported it.
    pub input: serde_json::Value,
}

/// Payload of `tool.result`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// Harness-side call id.
    pub call_id: String,
    /// Whether the tool call succeeded.
    pub ok: bool,
    /// Inline output when it is at most 8192 bytes.
    #[serde(default)]
    pub output: Option<String>,
    /// Blob reference used instead of `output` when the output is larger.
    #[serde(default)]
    pub output_ref: Option<BlobRef>,
    /// Process exit code, for shell-like tools.
    #[serde(default)]
    pub exit_code: Option<i32>,
}

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    /// The file was created.
    Create,
    /// The file was modified.
    Modify,
    /// The file was deleted.
    Delete,
    /// The file was renamed.
    Rename,
}

/// Payload of `file.change`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct FileChange {
    /// Path relative to the session working directory.
    pub path: String,
    /// What happened to the file.
    pub op: FileOp,
    /// Blob holding the diff, when available.
    #[serde(default)]
    pub diff_ref: Option<BlobRef>,
}

/// Status of one plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Done,
}

/// One item of a plan.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct PlanItem {
    /// Item text.
    pub text: String,
    /// Item status.
    pub status: PlanStatus,
}

/// Payload of `plan`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct PlanEvent {
    /// Plan items in display order.
    pub items: Vec<PlanItem>,
}

/// Payload of `limit`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct LimitEvent {
    /// Kind of limit.
    pub kind: LimitKind,
    /// Provider key, for example `claude` or `deepseek`.
    pub provider: String,
    /// When the limit lifts, when the harness reports it.
    #[serde(default)]
    pub resets_at: Option<Timestamp>,
    /// Redacted classifier input excerpt of at most 300 characters.
    pub message: String,
    /// HTTP status that carried the limit, when known.
    #[serde(default)]
    pub http_status: Option<u16>,
}

/// Payload of `cost`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CostEvent {
    /// Cost of the run or turn.
    pub cost: Cost,
    /// Run the cost belongs to, if any.
    #[serde(default)]
    pub run_id: Option<RunId>,
}

/// Why a session switched executors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    /// A human requested the switch.
    User,
    /// A provider limit forced the switch.
    Limit,
    /// The running harness crashed.
    Crash,
    /// The context window was exhausted.
    Context,
    /// Failover moved the session to another executor.
    Failover,
    /// The preferred rung became available again.
    ProbeBack,
    /// Another surface took the session over.
    Takeover,
}

/// Payload of `switch`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SwitchEvent {
    /// Switch row id.
    pub switch_id: SwitchId,
    /// Executor the session came from; `None` for the first attach.
    pub from: Option<ExecutorRef>,
    /// Executor the session moved to.
    pub to: ExecutorRef,
    /// Capsule captured for the handoff.
    pub capsule_id: Option<CapsuleId>,
    /// Why the session switched.
    pub reason: SwitchReason,
}

/// What caused a capsule to be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleTrigger {
    /// A switch between executors.
    Switch,
    /// A human asked for a capsule.
    Manual,
    /// The end of a turn.
    TurnEnd,
    /// A provider limit.
    Limit,
    /// Context pressure.
    Context,
    /// Export or import.
    Export,
    /// The job finished.
    JobEnd,
}

/// Payload of `capsule`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleEvent {
    /// Capsule id.
    pub capsule_id: CapsuleId,
    /// Capsule format version.
    pub version: u32,
    /// What caused the capsule.
    pub trigger: CapsuleTrigger,
    /// Last session event sequence covered by the capsule.
    pub at_seq: u64,
}

/// Payload of `error`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ErrorEvent {
    /// Machine-readable error class.
    pub kind: ErrorKind,
    /// Human-readable message (no secret values).
    pub message: String,
    /// Whether retrying the same operation may succeed.
    #[serde(default)]
    pub retryable: bool,
}

/// Payload of `note`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct Note {
    /// Note text.
    pub text: String,
}
