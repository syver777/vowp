# types.md — protocol types (crate `vowp-protocol`)

Status: current for v0.2.

Every type here lives in `vowp-protocol` and is the **only** definition: other crates import it, never redefine it.
All types derive `Debug, Clone, PartialEq, Serialize, Deserialize` (and `Eq`/`Hash`/`Copy` where noted as "plain
enum/id") and `schemars::JsonSchema`. JSON is the wire format for logs, bundles, `--json` CLI output and the DB JSON
columns (`schema/global.sql`, `schema/project.sql`). Names are `snake_case` in JSON, except event-type strings
(`session.created`) and executor strings (`opencode:provider/model#max`).

Module → file map:

| Module | Contents |
|---|---|
| `ids`, `time`, `error`, `cost`, `limit`, `executor` | foundations |
| `event`, `capsule` | event log + capsule |
| `job`, `gate`, `report`, `run`, `session`, `inbox`, `loops`, `release` | domain rows |
| `capacity` | v0.2 host samples, run usage, estimates, admission, brakes, targets, advice (§16.1) |
| `surface` | v0.2 surfaces, leases, remote-exec lines, orchestrator cards (§16.2) |
| `snapshot` | v0.2 `snapshot.json` and the project registry view (§16.3) |
| `catalog` | v0.2 lifecycle classes, catalog rows, GC and backup reports (§16.4) |
| `vault` | v0.2 secret names, metadata, modes and the one-time request (§16.5) |
| `config`, `adapter`, `services`, `schema` | `vowp.toml`, adapter trait, service traits, JSON Schema export |

`lib.rs` re-exports everything at the crate root (`vowp_protocol::Job`), plus `vowp_protocol::prelude::*`.

---

## 1. Ids and timestamps (`ids`, `time`)

```rust
/// Declares a transparent String newtype. All id types: Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord,
/// Serialize/Deserialize (transparent), Display, FromStr (validating), AsRef<str>, JsonSchema.
macro_rules! id_type { /* private */ }

id_type!(SessionId);   // 26-char ULID
id_type!(EventId);     // 26-char ULID
id_type!(RunId);       // 26-char ULID
id_type!(SwitchId);    // 26-char ULID
id_type!(InboxId);     // 26-char ULID
id_type!(ReleaseId);   // 26-char ULID
id_type!(ScheduleId);  // 26-char ULID
id_type!(CapsuleId);   // "cap_" + 26-char ULID
id_type!(JobId);       // slug: ^[a-z0-9][a-z0-9-]{0,63}$   (e.g. "graph-engine")
id_type!(NodeName);    // slug, same pattern as JobId
id_type!(ProjectName); // slug, same pattern as JobId

impl SessionId { pub fn new() -> Self; }          // fresh ULID (monotonic within one process)
impl EventId   { pub fn new() -> Self; }          // likewise: RunId, SwitchId, InboxId, ReleaseId, ScheduleId, CapsuleId
impl JobId     { pub fn parse(s: &str) -> Result<Self, ProtocolError>; }   // validating constructor (also FromStr)
impl SessionId { pub fn parse(s: &str) -> Result<Self, ProtocolError>; }   // ULID check; same on every ULID id
impl CapsuleId { pub fn ulid(&self) -> &str; }    // part after "cap_"
```

`ulid` crate generates ids; `new()` uses a process-wide `Mutex<ulid::Generator>` so ids created in the same
millisecond still sort in creation order. Parsing never panics; invalid input → `ProtocolError::InvalidId`.

```rust
/// UTC instant with millisecond precision. Wire/DB form: RFC 3339 `2026-11-08T19:25:47.123Z` (always `Z`, always 3 digits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(time::OffsetDateTime);

impl Timestamp {
    pub fn now() -> Self;
    pub fn from_unix_secs(secs: i64) -> Self;
    pub fn from_unix_millis(ms: i64) -> Self;
    pub fn unix_secs(&self) -> i64;
    pub fn unix_millis(&self) -> i64;
    pub fn parse(s: &str) -> Result<Self, ProtocolError>;   // accepts any RFC 3339 offset, normalises to UTC, truncates to ms
    pub fn as_rfc3339(&self) -> String;                      // canonical form above
    pub fn plus(&self, d: std::time::Duration) -> Self;
    pub fn minus(&self, d: std::time::Duration) -> Self;
    pub fn since(&self, earlier: Timestamp) -> std::time::Duration;   // saturating at zero
}
impl std::fmt::Display for Timestamp {}

/// Clock abstraction so loops/gates are testable. Real: `SystemClock`. Tests: `FakeClock` (in `vowp_protocol::time`,
/// advanceable with `advance(Duration)`; cloneable handle over `Arc<Mutex<Timestamp>>`).
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}
pub struct SystemClock;
#[derive(Clone)] pub struct FakeClock { /* Arc<Mutex<Timestamp>> */ }
impl FakeClock { pub fn at(t: Timestamp) -> Self; pub fn advance(&self, d: std::time::Duration); pub fn set(&self, t: Timestamp); }
```

Example: `"ts": "2026-11-08T19:25:47.123Z"`.

---

## 2. Errors (`error`)

Library crates define their own `thiserror` enums; `vowp-protocol` provides the cross-crate vocabulary:

```rust
/// Stable machine-readable error classes, used in `error` events, `--json` output and CLI exit-code mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput, NotFound, Conflict, Io, Db, Parse, Config,
    AdapterUnavailable, AdapterFailed, Limit, Auth, Timeout,
    Guard, Refused, Remote, Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid id {kind}: {value:?}")]            InvalidId { kind: &'static str, value: String },
    #[error("invalid timestamp: {0}")]                  InvalidTimestamp(String),
    #[error("invalid executor {0:?}: {1}")]             InvalidExecutor(String, String),
    #[error("invalid event payload for {event_type}: {source}")]
                                                         InvalidPayload { event_type: String, source: serde_json::Error },
    #[error("invalid front-matter: {0}")]               FrontMatter(String),
    #[error("invalid gate: {0}")]                       InvalidGate(String),
    #[error("invalid config: {0}")]                     Config(String),
    #[error("json: {0}")]                               Json(#[from] serde_json::Error),
}
impl ProtocolError { pub fn kind(&self) -> ErrorKind; }
```

---

## 3. Executors, profiles, ladders (`executor`, `profile` inside `executor.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum HarnessKind {
    #[serde(rename = "claude-code")] ClaudeCode,
    #[serde(rename = "opencode")]    Opencode,
    #[serde(rename = "cli")]         Cli,       // generic mapping-file adapter
    #[serde(rename = "vowp")]        Vowp,      // events VOWP itself writes (system events, shell/gate runs)
}
impl HarnessKind { pub fn as_str(&self) -> &'static str; pub fn parse(s: &str) -> Result<Self, ProtocolError>; }

/// One concrete way to run a model: `harness:model[@effort][#variant]`.
///   claude-code:model-a@medium                     -> harness=ClaudeCode model="model-a" effort=Some("medium")
///   opencode:provider/model-b#max                  -> harness=Opencode  model="provider/model-b" variant=Some("max")
///   cli:codex                                       -> harness=Cli       model="codex" (name of a [harnesses.<name>] entry)
/// Canonical Display/serde form is exactly that string. `harness:` is mandatory; model may contain '/' ':' '.' '-' '_'
/// but not '@' or '#'; effort and variant are `[a-z0-9_-]+`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct Executor {
    pub harness: HarnessKind,
    pub model: String,
    pub effort: Option<String>,    // claude-code only: low|medium|high|... passed as --effort
    pub variant: Option<String>,   // opencode only: passed as `provider/model#variant` to --model
}
impl Executor {
    pub fn parse(s: &str) -> Result<Self, ProtocolError>;          // FromStr too
    pub fn provider(&self) -> String;   // limits key. claude-code -> "claude"; opencode -> text before first '/' of model
                                        // (e.g. "provider"); cli -> the model/entry name
    pub fn model_arg(&self) -> String;  // opencode: "provider/model-b#max"; claude-code: "model-a"
}

/// Where an executor runs: the executor plus the node. `node == None` means "this node".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutorRef { pub executor: Executor, pub node: Option<NodeName> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role { Planner, Keeper, Worker, Verifier }

/// What to do when the rung's provider reports `quota`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnQuota {
    NextThenProbeBack,   // move to the next rung; probe the earlier rung after resets_at; switch back (default)
    Next,                // move to the next rung and stay
    Park,                // park the jobs, inbox item
}

/// A role's model ladder (profile). Rung 0 is preferred. Order matters; duplicates are rejected on validate().
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    pub ladder: Vec<Executor>,                  // non-empty
    #[serde(default = "default_on_quota")] pub on_quota: OnQuota,
    #[serde(default)] pub node: Option<NodeName>,   // run on that node (default: the project's home node)
}
impl Profile {
    pub fn validate(&self) -> Result<(), ProtocolError>;
    /// First rung whose provider is not limited at `now`, given the set of limited providers (provider -> resets_at).
    /// Returns (rung index, executor). `None` if every rung is limited.
    pub fn pick(&self, limited: &std::collections::HashMap<String, Option<Timestamp>>, now: Timestamp)
        -> Option<(usize, &Executor)>;
}
```

JSON: `"claude-code:model-a@medium"` (an `Executor` is a JSON string).

---

## 4. Cost and limits (`cost`, `limit`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum CostSource {
    #[serde(rename = "receipt")]     Receipt,       // the harness reported it (opencode step_finish.cost, claude result.total_cost_usd)
    #[serde(rename = "price-table")] PriceTable,    // estimated from [providers.*.prices] in vowp.toml
    #[serde(rename = "unknown")]     Unknown,       // never assumed free: usd = None
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tokens {
    #[serde(default)] pub input: u64,        // non-cached input tokens
    #[serde(default)] pub output: u64,
    #[serde(default)] pub reasoning: u64,
    #[serde(default)] pub cache_read: u64,
    #[serde(default)] pub cache_write: u64,
}
impl Tokens { pub fn add(&mut self, other: &Tokens); pub fn total(&self) -> u64; }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Cost {
    pub usd: Option<f64>,                    // None iff source == Unknown
    pub tokens: Tokens,
    pub source: CostSource,
}
impl Cost {
    pub fn unknown() -> Self;
    pub fn receipt(usd: f64, tokens: Tokens) -> Self;
    pub fn add(&mut self, other: &Cost);     // usd: None+Some = Some (partial); source degrades Receipt > PriceTable > Unknown
                                             // only if both sides have usd; the weaker source wins
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind { Quota, Credits, Rate, Context, Auth }

/// `limits` table row. `Unavailable` (used only in the table, never in events) = repeated unknown failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Limit {
    pub provider: String,                    // "claude", "provider"
    pub harness: Option<HarnessKind>,
    pub kind: LimitRowKind,
    pub resets_at: Option<Timestamp>,        // None = until a human resolves it (credits, auth)
    pub detail: String,
    pub since: Timestamp,
    pub last_probe_at: Option<Timestamp>,
    pub probe_fail_streak: u32,
    pub concurrency_cap: Option<u32>,
    pub cap_until: Option<Timestamp>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitRowKind { Quota, Credits, Rate, Context, Auth, Unavailable }
```

JSON examples:

```json
{"usd": 0.0349033, "tokens": {"input": 2, "output": 4, "reasoning": 0, "cache_read": 11873, "cache_write": 8418}, "source": "receipt"}
{"provider": "claude", "harness": "claude-code", "kind": "quota", "resets_at": "2026-11-08T22:20:00.000Z",
 "detail": "five_hour window rejected", "since": "2026-11-08T19:41:02.000Z", "last_probe_at": null,
 "probe_fail_streak": 0, "concurrency_cap": null, "cap_until": null}
```

---

## 5. Events (`event`)

### 5.1 Envelope

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Actor { User, Assistant, System, Tool }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSource {
    pub harness: HarnessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub node: Option<NodeName>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub native_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum EventType {
    #[serde(rename = "session.created")]    SessionCreated,
    #[serde(rename = "session.executor")]   SessionExecutor,
    #[serde(rename = "turn.started")]       TurnStarted,
    #[serde(rename = "turn.ended")]         TurnEnded,
    #[serde(rename = "user.message")]       UserMessage,
    #[serde(rename = "assistant.message")]  AssistantMessage,
    #[serde(rename = "assistant.reasoning")] AssistantReasoning,
    #[serde(rename = "tool.call")]          ToolCall,
    #[serde(rename = "tool.result")]        ToolResult,
    #[serde(rename = "file.change")]        FileChange,
    #[serde(rename = "plan")]               Plan,
    #[serde(rename = "limit")]              Limit,
    #[serde(rename = "cost")]               Cost,
    #[serde(rename = "switch")]             Switch,
    #[serde(rename = "capsule")]            Capsule,
    #[serde(rename = "error")]              Error,
    #[serde(rename = "note")]               Note,
}
impl EventType { pub fn as_str(&self) -> &'static str; pub fn parse(s: &str) -> Result<Self, ProtocolError>; }

/// One line of `log.jsonl`. The payload is kept as raw JSON so a reader never fails on a payload it cannot type;
/// use `body()` for the typed view and `EventEnvelope::new` to build one from a typed `EventBody`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    pub id: EventId,
    pub session_id: SessionId,
    pub seq: u64,                              // 1,2,3,... gaps rejected by the store
    pub ts: Timestamp,
    #[serde(rename = "type")] pub event_type: EventType,
    pub actor: Actor,
    pub source: EventSource,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub raw: Option<serde_json::Value>,  // original harness event; display only
    #[serde(default = "default_true")] pub replayable: bool,                                        // false for reasoning blobs
}
impl EventEnvelope {
    pub fn body(&self) -> Result<EventBody, ProtocolError>;              // typed payload
    pub fn new(session_id: SessionId, seq: u64, ts: Timestamp, actor: Actor, source: EventSource, body: &EventBody)
        -> EventEnvelope;                                                 // fresh EventId; replayable=false for AssistantReasoning
}

/// What an adapter emits: an envelope without id/seq/ts/session_id (the session layer assigns those).
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessEvent {
    pub ts: Option<Timestamp>,                 // harness-reported time, if any
    pub actor: Actor,
    pub body: EventBody,
    pub raw: Option<serde_json::Value>,
}
```

### 5.2 Payloads

```rust
/// Typed payload. `parts()`/`from_parts()` convert to/from `(EventType, serde_json::Value)`.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    SessionCreated(SessionCreated), SessionExecutor(SessionExecutor), TurnStarted(TurnStarted), TurnEnded(TurnEnded),
    UserMessage(UserMessage), AssistantMessage(AssistantMessage), AssistantReasoning(AssistantReasoning),
    ToolCall(ToolCall), ToolResult(ToolResult), FileChange(FileChange), Plan(PlanEvent),
    Limit(LimitEvent), Cost(CostEvent), Switch(SwitchEvent), Capsule(CapsuleEvent), Error(ErrorEvent), Note(Note),
}
impl EventBody {
    pub fn event_type(&self) -> EventType;
    pub fn to_value(&self) -> serde_json::Value;                                   // payload JSON
    pub fn from_parts(t: EventType, v: serde_json::Value) -> Result<Self, ProtocolError>;
    pub fn summary(&self) -> String;                                              // <= 200 chars, one line, for session_events.summary
}

/// Large content reference. Tool output larger than 8 KiB (8192 bytes) is spilled to `blobs/<sha256>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlobRef { pub sha256: String, pub bytes: u64, pub preview: String /* first 512 chars */ }

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SessionCreated {
    pub title: String, pub cwd: String,
    #[serde(default)] pub project: Option<ProjectName>, #[serde(default)] pub job_id: Option<JobId>,
    #[serde(default)] pub role: Option<SessionRole>, #[serde(default)] pub parent_session_id: Option<SessionId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum AttachAction { Attach, Detach }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SessionExecutor {
    pub action: AttachAction, pub executor: Executor, pub node: NodeName,
    #[serde(default)] pub native_session_id: Option<String>,     // known at attach only when resuming (or a pre-chosen claude `--session-id`); otherwise set on the detach event and in `adapters.json`
    #[serde(default)] pub run_id: Option<RunId>,
    #[serde(default)] pub reason: Option<String>,                 // for detach: "turn_done" | "limit" | "crash" | "switch" | ...
}
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct TurnStarted { pub turn: u32 }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum StopReason { EndTurn, MaxTurns, Limit, Error, Interrupted, Crash }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct TurnEnded { pub turn: u32, pub stop_reason: StopReason, #[serde(default)] pub duration_ms: Option<u64> }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct UserMessage { pub text: String, #[serde(default)] pub attachments: Vec<BlobRef>,
                         #[serde(default)] pub note: Option<String> /* "handoff" for capsule seeds */ }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct AssistantMessage { pub text: String }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct AssistantReasoning { pub text: String }                        // envelope.replayable = false
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ToolCall { pub call_id: String, pub name: String, pub input: serde_json::Value }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub call_id: String, pub ok: bool,
    #[serde(default)] pub output: Option<String>,        // inline when <= 8192 bytes
    #[serde(default)] pub output_ref: Option<BlobRef>,   // set instead of `output` when larger (preview inside the ref)
    #[serde(default)] pub exit_code: Option<i32>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum FileOp { Create, Modify, Delete, Rename }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct FileChange { pub path: String, pub op: FileOp, #[serde(default)] pub diff_ref: Option<BlobRef> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum PlanStatus { Pending, InProgress, Done }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct PlanItem { pub text: String, pub status: PlanStatus }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct PlanEvent { pub items: Vec<PlanItem> }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct LimitEvent {
    pub kind: LimitKind, pub provider: String,
    #[serde(default)] pub resets_at: Option<Timestamp>,
    pub message: String,                                   // redacted classifier input excerpt (<= 300 chars)
    #[serde(default)] pub http_status: Option<u16>,
}
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CostEvent { pub cost: Cost, #[serde(default)] pub run_id: Option<RunId> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum SwitchReason { User, Limit, Crash, Context, Failover, ProbeBack }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct SwitchEvent {
    pub switch_id: SwitchId, pub from: Option<ExecutorRef>, pub to: ExecutorRef,
    pub capsule_id: Option<CapsuleId>, pub reason: SwitchReason,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum CapsuleTrigger { Switch, Manual, TurnEnd, Limit, Context, Export, JobEnd }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleEvent { pub capsule_id: CapsuleId, pub version: u32, pub trigger: CapsuleTrigger, pub at_seq: u64 }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct ErrorEvent { pub kind: ErrorKind, pub message: String, #[serde(default)] pub retryable: bool }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct Note { pub text: String }
```

### 5.3 Example log lines

```json
{"id":"01KA1V0Z8Q3M9W6E5T2R4N7B1C","session_id":"01KA1V0Z7A0000000000000000","seq":1,"ts":"2026-11-08T19:30:00.000Z","type":"session.created","actor":"system","source":{"harness":"vowp","node":"server"},"payload":{"title":"graph-engine","cwd":"/home/user/project","project":"vowp","job_id":"graph-engine","role":"worker"},"replayable":true}
{"id":"01KA1V10C3F7H9J2K4M6N8P0Q2","session_id":"01KA1V0Z7A0000000000000000","seq":6,"ts":"2026-11-08T19:30:12.481Z","type":"tool.call","actor":"assistant","source":{"harness":"opencode","model":"provider/model-b","node":"server","native_session_id":"ses_e439617b6ffepWG4UNK521b6ly"},"payload":{"call_id":"call_01_2ncr","name":"shell","input":{"command":"cargo test"}},"replayable":true}
{"id":"01KA1V11D4G8J1K3M5N7P9R0S2","session_id":"01KA1V0Z7A0000000000000000","seq":9,"ts":"2026-11-08T19:41:02.000Z","type":"limit","actor":"system","source":{"harness":"claude-code","model":"model-a","node":"local"},"payload":{"kind":"quota","provider":"claude","resets_at":"2026-11-08T22:20:00.000Z","message":"You've hit your limit · resets 11pm (Europe/Oslo)"},"replayable":true}
```

---

## 6. Capsule (`capsule`)

Unchanged in spirit from the capsule design in the session model; JSON keys are `snake_case`. Stored at
`sessions/<id>/capsules/<at_seq:010>-<ulid>.json` (at_seq zero-padded to 10 digits, e.g. `0000000142-01KA1V2B….json`) and in `capsules.body_json`.

```rust
pub const CAPSULE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct Capsule {
    pub version: u32,                         // CAPSULE_VERSION
    pub capsule_id: CapsuleId,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub at_seq: u64,                          // last session event covered
    pub trigger: CapsuleTrigger,
    pub from: ExecutorRef,                    // executor that was running
    pub intent: String,                       // one paragraph, non-empty
    #[serde(default)] pub constraints: Vec<String>,
    #[serde(default)] pub decisions: Vec<CapsuleDecision>,
    pub state: CapsuleState,
    #[serde(default)] pub files: Vec<CapsuleFile>,
    #[serde(default)] pub gotchas: Vec<String>,
    #[serde(default)] pub open_questions: Vec<String>,
    pub telemetry: CapsuleTelemetry,
}
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleDecision { pub what: String, pub why: String, pub at_seq: Option<u64> }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Default)]
pub struct CapsuleState {
    #[serde(default)] pub done: Vec<String>, #[serde(default)] pub in_progress: Vec<String>,
    #[serde(default)] pub blocked: Vec<String>, #[serde(default)] pub next_steps: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum FileRole { Read, Created, Modified, Deleted, Mentioned }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleFile { pub path: String, pub role: FileRole, #[serde(default)] pub note: String }
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Default)]
pub struct CapsuleTelemetry { pub turns: u32, pub tokens_in: u64, pub tokens_out: u64, #[serde(default)] pub usd: Option<f64> }

impl Capsule {
    pub fn validate(&self) -> Result<(), ProtocolError>;   // intent non-empty; version == CAPSULE_VERSION; at_seq >= 1; each list item non-empty
    pub fn render_markdown(&self) -> String;               // the seed text for a target harness (sessions.md §4)
}

/// What the capsule-writing model must return (no ids/ts: VOWP fills those). Its JSON Schema is embedded in the prompt.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleDraft {
    pub intent: String,
    #[serde(default)] pub constraints: Vec<String>, #[serde(default)] pub decisions: Vec<CapsuleDecision>,
    pub state: CapsuleState, #[serde(default)] pub files: Vec<CapsuleFile>,
    #[serde(default)] pub gotchas: Vec<String>, #[serde(default)] pub open_questions: Vec<String>,
}
```

```json
{"version":1,"capsule_id":"cap_01KA1V2B000000000000000000","session_id":"01KA1V0Z7A0000000000000000","created_at":"2026-11-08T19:41:09.000Z","at_seq":142,"trigger":"limit",
 "from":{"executor":"claude-code:model-a@medium","node":"server"},
 "intent":"Add the graph scheduler and its tests.",
 "constraints":["keep the diff within the owned paths"],"decisions":[{"what":"fix jobs reuse the job branch","why":"keeps one diff to review","at_seq":88}],
 "state":{"done":["protocol types reviewed"],"in_progress":["graph split"],"blocked":[],"next_steps":["dispatch tests"]},
 "files":[{"path":"docs/design/loops.md","role":"modified","note":"added L6"}],
 "gotchas":["the executor reports reasoning tokens separately"],"open_questions":[],
 "telemetry":{"turns":12,"tokens_in":84000,"tokens_out":9100,"usd":null}}
```

---

## 7. Jobs, edges, states (`job`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum JobKind { Plan, Spec, Build, Test, Verify, Fix, Merge, Docs, Research, Promote, Release, Decision }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending, Ready, Running, Stalled, Gating, Reviewing, Fixing, Merging,
    Done, Failed, Parked, Replanned, Cancelled,
}
impl JobState {
    pub fn is_terminal(&self) -> bool;      // Done | Replanned | Cancelled   (Failed and Parked are NOT terminal)
    pub fn holds_lock(&self) -> bool;       // Running | Stalled | Gating | Reviewing | Fixing | Merging  (ownership + worktree)
    pub fn as_str(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum EdgeKind { Needs, Verifies, Fixes, Escalates, Blocks }
impl EdgeKind { pub fn is_loop_edge(&self) -> bool; }   // true only for Fixes

/// `(src, dst, kind)` reads "src <kind> dst": `src needs dst`, `src verifies dst`, `src fixes dst`, `src escalates dst`, `src blocks dst`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Edge { pub src: JobId, pub dst: JobId, pub kind: EdgeKind }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum LoopUntil { Gates }                                   // v0.1: only "until gates pass"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum OnExhausted { Escalate, Park, Fail }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobLoop { pub until: LoopUntil, pub max_attempts: u32 /* default 3 */, pub on_exhausted: OnExhausted /* default Escalate */ }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub id: JobId,
    pub project: ProjectName,
    pub title: String,
    pub kind: JobKind,
    pub role: Role,
    pub profile: String,
    pub state: JobState,
    pub priority: i32,                           // default 50
    pub needs: Vec<JobId>,                       // convenience: the `needs` edges (store fills it from `edges`)
    pub owns: Vec<String>,                       // glob patterns, repo-relative, '/' separators
    pub reads: Vec<String>,
    pub brief: String,                           // repo-relative path
    pub report: String,
    pub gates: Vec<Gate>,
    #[serde(rename = "loop")] pub loop_: JobLoop,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub attempts: u32, pub continues: u32, pub hang_restarts: u32,
    pub parked_reason: Option<ParkedReason>,
    pub note: String,
    pub created_by: String,
    pub last_progress_at: Option<Timestamp>,
    pub state_changed_at: Timestamp, pub created_at: Timestamp, pub updated_at: Timestamp,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ParkedReason { Credits, Auth, Question, ProviderBlocked, Manual, Exhausted }

/// Input for `vowp job add` / `Store::insert_job`. Everything but `id`, `title`, `kind`, `role` has a default
/// (profile = role name, priority 50, brief/report paths `.vowp/jobs/<id>/{brief,report}.md`, gates = default set for the kind).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NewJob {
    pub id: JobId, pub title: String, pub kind: JobKind, pub role: Role,
    #[serde(default)] pub profile: Option<String>, #[serde(default)] pub needs: Vec<JobId>,
    #[serde(default)] pub owns: Vec<String>, #[serde(default)] pub reads: Vec<String>,
    #[serde(default)] pub gates: Option<Vec<Gate>>, #[serde(default)] pub priority: Option<i32>,
    #[serde(default, rename = "loop")] pub loop_: Option<JobLoop>,
    #[serde(default)] pub brief_text: Option<String>,      // written to brief_path by the caller (core), not by the store
}

/// Partial update (`vowp job update`). `None` = unchanged. State is NOT updatable here (state changes go through the
/// state machine, vowp-graph::transition).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobPatch {
    pub title: Option<String>, pub priority: Option<i32>, pub profile: Option<String>,
    pub owns: Option<Vec<String>>, pub reads: Option<Vec<String>>, pub gates: Option<Vec<Gate>>,
    #[serde(rename = "loop")] pub loop_: Option<JobLoop>,
    #[serde(default)] pub add_needs: Vec<JobId>, #[serde(default)] pub remove_needs: Vec<JobId>, pub note: Option<String>,
}
// Every Option field is `#[serde(default, skip_serializing_if = "Option::is_none")]` so `{}` and `{"loop":{…}}` deserialize. Test `job_patch_json_roundtrip_with_loop_and_defaults` (crates/vowp-protocol.md §6).
```

JSON (`vowp job show graph-engine --json`):

```json
{"id":"graph-engine","project":"vowp","title":"vowp-graph","kind":"build","role":"worker","profile":"worker","state":"running","priority":50,
 "needs":["protocol"],"owns":["crates/vowp-graph/**"],"reads":["docs/design/**"],
 "brief":".vowp/jobs/graph-engine/brief.md","report":".vowp/jobs/graph-engine/report.md",
 "gates":[{"report":{"fields":["status","branch","commits","tests"]}},{"cmd":{"run":"cargo test -p vowp-graph","expect_exit":0}},{"owns_respected":true},{"review":{"by":"verifier"}}],
 "loop":{"until":"gates","max_attempts":3,"on_exhausted":"escalate"},
 "branch":"job/graph-engine","worktree":"/home/user/project","attempts":0,"continues":0,"hang_restarts":0,
 "parked_reason":null,"note":"","created_by":"planner","last_progress_at":"2026-11-08T20:10:00.000Z",
 "state_changed_at":"2026-11-08T20:01:00.000Z","created_at":"2026-11-08T19:50:00.000Z","updated_at":"2026-11-08T20:10:00.000Z"}
```

---

## 8. Gates, evidence, reports, briefs (`gate`, `report`)

```rust
/// Externally tagged, matches the brief YAML:  `- cmd: { run: "...", expect_exit: 0 }`, `- owns_respected: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    Report(ReportGate),
    Cmd(CmdGate),
    OwnsRespected(bool),                 // only `true` is meaningful; false = gate disabled (kept for readability)
    Pushed(PushedGate),
    Review(ReviewGate),
    Script(CmdGate),                     // identical shape to Cmd; evidence kind distinguishes the two
    Deploy(DeployGate),
    SecretScan(bool),                    // repo-wide scan of the job diff for secret patterns (never records values)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportGate { #[serde(default = "default_report_fields")] pub fields: Vec<String> }   // default [status, branch, commits, tests]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CmdGate {
    pub run: String,                                     // executed with `sh -c` in the job worktree
    #[serde(default)] pub expect_exit: i32,              // default 0
    #[serde(default)] pub expect_pattern: Option<String>, // regex that stdout+stderr must match
    #[serde(default = "default_gate_timeout")] pub timeout_secs: u64,   // 1800
    #[serde(default)] pub cwd: Option<String>,           // relative to the worktree
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PushedGate { #[serde(default = "origin")] pub remote: String, #[serde(default)] pub branch: Option<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewGate { #[serde(default = "verifier_role")] pub by: Role }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeployGate { pub run: String, pub probe: CmdGate, #[serde(default)] pub env: Option<String> }

impl Gate {
    pub fn kind(&self) -> GateKind;
    /// Brief front-matter short form: "report", "owns_respected", "review", "pushed", "secret_scan", or the command string for Cmd with defaults.
    pub fn short(&self) -> String;
    /// Inverse of `short()` for brief parsing: known keywords -> variants; any other string -> `Cmd{run, expect_exit:0,..}`.
    pub fn from_short(s: &str) -> Result<Gate, ProtocolError>;
    pub fn validate(&self) -> Result<(), ProtocolError>;   // run non-empty, regex compiles, timeout > 0
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum GateKind { Report, Cmd, OwnsRespected, Pushed, Review, Script, Deploy, SecretScan }

/// The default gate set for a job kind: build/fix/test -> [report, owns_respected, secret_scan, review(build only)] ;
/// docs/spec -> [report, owns_respected, secret_scan, review]; verify/research/plan -> [report]; merge -> [report];
/// decision/promote/release -> []. Default sets contain no `cmd` gates: crate-specific commands are added per job (`vowp job add --gates …`); the project's `check_cmds` run only in the merge queue (loops.md §4, §6).
pub fn default_gates(kind: JobKind) -> Vec<Gate>;

/// Evidence recorded for one gate evaluation (`gate_results.evidence_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    Report { front_matter: Option<serde_json::Value>, missing_fields: Vec<String>, problems: Vec<String> },
    Cmd { command: String, exit_code: Option<i32>, timed_out: bool, tail: String /* last 200 lines */, duration_ms: u64 },
    Script { command: String, exit_code: Option<i32>, timed_out: bool, tail: String, duration_ms: u64 },
    OwnsRespected { changed: Vec<String>, violations: Vec<String> },
    Pushed { remote: String, branch: String, ls_remote: String, local_sha: Option<String> },
    Review { verify_job: Option<JobId>, verdict: Option<Verdict>, findings: Vec<String> },
    Deploy { command: String, exit_code: Option<i32>, probe: Box<Evidence>, version_before: Option<String>, version_after: Option<String>, rollback_cmd: Option<String> },
    SecretScan { files_scanned: u64, hits: Vec<SecretHit> },
}
/// A secret-scan hit: file, line number and rule NAME only. The matched text is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretHit { pub path: String, pub line: u32, pub rule: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GateResult {
    pub id: Option<i64>,                 // assigned by the store
    pub job_id: JobId, pub run_id: Option<RunId>, pub attempt: u32,
    pub gate_index: i32,                 // -1 for repo-wide merge checks
    pub gate: Gate, pub passed: bool, pub evidence: Evidence,
    pub output_blob: Option<String>, pub duration_ms: u64, pub ts: Timestamp,
}
```

`{"kind":"cmd","command":"cargo test -p vowp-graph","exit_code":0,"timed_out":false,"tail":"test result: ok. 31 passed","duration_ms":41234}`

### 8.1 Report front-matter

A report is a Markdown file starting with a YAML front-matter block delimited by `---` lines.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ReportStatus { Done, Partial, Blocked }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum Verdict { Pass, Fail }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportFrontMatter {
    pub job: JobId,
    pub status: ReportStatus,
    pub branch: String,
    #[serde(default)] pub commits: Vec<String>,
    /// Either the structured form or a free string. Free-string briefs are accepted for compatibility; both are accepted.
    #[serde(default)] pub tests: Option<TestsField>,
    #[serde(default)] pub files_changed: Vec<String>,
    #[serde(default)] pub open_items: Vec<String>,
    #[serde(default)] pub questions_for_owner: Vec<String>,
    // verifier-only
    #[serde(default)] pub verdict: Option<Verdict>,
    #[serde(default)] pub findings: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TestsField { Structured { command: String, #[serde(default)] passed: Option<u32>, #[serde(default)] failed: Option<u32> }, Text(String) }

#[derive(Debug, Clone, PartialEq)]
pub struct Report { pub front_matter: ReportFrontMatter, pub body: String, pub raw_front_matter: serde_json::Value }
impl Report {
    /// Parse `---\n<yaml>\n---\n<body>`. Errors (ProtocolError::FrontMatter): no opening/closing fence, invalid YAML,
    /// missing required field (job, status, branch), invalid enum value. Trailing `# comment`s in YAML are ignored.
    pub fn parse(markdown: &str) -> Result<Report, ProtocolError>;
    pub fn render(&self) -> String;
    /// Required-field check used by the `report` gate: returns the names in `required` absent or empty in `raw_front_matter`
    /// (absent key, null, "" ; an empty list counts as present for `commits`/`open_items`/`questions_for_owner`/`files_changed`).
    pub fn missing_fields(&self, required: &[String]) -> Vec<String>;
}
```

### 8.2 Brief front-matter

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BriefFrontMatter {
    pub job: JobId,
    pub owns: Vec<String>,
    #[serde(default)] pub must_not_touch: Vec<String>,
    #[serde(default)] pub needs_done: Vec<JobId>,
    #[serde(default)] pub gates: Vec<String>,        // short forms: Gate::short()
    #[serde(default)] pub done_when: String,
}
pub struct Brief { pub front_matter: BriefFrontMatter, pub body: String }
impl Brief {
    pub fn parse(markdown: &str) -> Result<Brief, ProtocolError>;
    pub fn render(&self) -> String;
    /// Generated from the graph node + the planner's text: owns/needs/gates come from `job`.
    pub fn from_job(job: &Job, body: &str) -> Brief;
}
```

Example brief:

```markdown
---
job: graph-engine
owns: ["crates/vowp-graph/**"]
must_not_touch: ["everything else"]
needs_done: [protocol]
gates: [report, "cargo test -p vowp-graph", owns_respected, review]
done_when: "report has status: done and every gate passes"
---
# graph-engine …
```

---

## 9. Runs, workers, sessions, inbox (`run`, `session`, `inbox`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum RunRole { Planner, Keeper, Worker, Verifier, Capsule, Probe, Merge, Gate }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum RunPurpose { Start, Continue, Fix, Verify, Plan, Keep, Capsule, Probe, Merge, Gate }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum RunOutcome { Ok, Limit, Credits, Auth, Context, Rate, Crash, Stalled, Killed, Error }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    pub id: RunId, pub job_id: Option<JobId>, pub session_id: Option<SessionId>,
    pub role: RunRole, pub purpose: RunPurpose, pub profile: Option<String>, pub rung: u32,
    pub executor: Option<Executor>,          // None for shell/gate/merge runs; table columns harness/model/effort/provider derive from it
    pub provider: Option<String>, pub node: NodeName,
    pub native_session_id: Option<String>, pub pid: Option<u32>, pub window: Option<String>, pub cwd: Option<String>,
    pub started_at: Timestamp, pub ended_at: Option<Timestamp>,
    pub exit_code: Option<i32>, pub outcome: Option<RunOutcome>, pub limit_kind: Option<LimitKind>,
    pub cost: Cost, pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum WorkerKind { Worker, Verifier }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum WorkerState { Idle, Busy, Draining }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Worker { pub node: NodeName, pub slot: u32, pub kind: WorkerKind, pub state: WorkerState,
                    pub run_id: Option<RunId>, pub job_id: Option<JobId>, pub window: Option<String>, pub updated_at: Timestamp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum SessionState { Idle, Running, Blocked, Switching, Closed }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub id: SessionId, pub project: Option<ProjectName>, pub job_id: Option<JobId>,
    pub role: Option<SessionRole>,           // Planner|Keeper|Worker|Verifier|User
    pub title: String, pub parent_session_id: Option<SessionId>, pub node: NodeName, pub cwd: String,
    pub state: SessionState, pub executor: Option<Executor>,
    pub last_seq: u64, pub last_capsule_id: Option<CapsuleId>, pub log_bytes: u64,
    pub created_at: Timestamp, pub updated_at: Timestamp,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum SessionRole { Planner, Keeper, Worker, Verifier, User }

/// Row of `session_events` (index of one log line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventIndexRow { pub session_id: SessionId, pub seq: u64, pub id: EventId, pub ts: Timestamp, pub event_type: EventType,
                           pub actor: Actor, pub harness: HarnessKind, pub model: Option<String>,
                           pub byte_offset: u64, pub byte_len: u64, pub summary: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Switch { pub id: SwitchId, pub session_id: SessionId, pub from_executor: Option<Executor>, pub to_executor: Executor,
                    pub from_node: Option<NodeName>, pub to_node: NodeName, pub capsule_id: Option<CapsuleId>,
                    pub reason: SwitchReason, pub state: SwitchState, pub error: Option<String>,
                    pub started_at: Timestamp, pub ended_at: Option<Timestamp>, pub at_seq: u64 }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum SwitchState { Started, Completed, Aborted }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum InboxKind { Question, Approval, Decision, Notice }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum InboxState { Open, Answered, Dismissed }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    pub id: InboxId, pub project: Option<ProjectName>, pub job_id: Option<JobId>, pub kind: InboxKind,
    pub title: String, pub body: String, pub options: Vec<String>, pub state: InboxState,
    pub answer: Option<String>, pub answered_by: Option<String>, pub created_at: Timestamp, pub answered_at: Option<Timestamp>,
}
```

---

## 10. Nodes, projects, loops, schedules, releases, ledger (`loops`, `release`, `session`)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Node { pub name: NodeName, pub ssh_target: Option<String>, pub is_local: bool,
                  pub capabilities: serde_json::Value, pub last_seen_at: Option<Timestamp>, pub created_at: Timestamp }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Project { pub name: ProjectName, pub path: String, pub remote_url: Option<String>, pub default_branch: String,
                     pub config: serde_json::Value, pub created_at: Timestamp, pub updated_at: Timestamp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum LoopKind { L1, L2, L3, L4, L5, L6, L7, L8, L9 }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ActiveState { Active, Paused }
/// Per-loop bounds (stored in loops.bounds_json). Unused fields are omitted. Fix rounds are per job:
/// `JobLoop.max_attempts` is the single fix-round bound, not a loop setting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LoopBounds {
    #[serde(skip_serializing_if = "Option::is_none")] pub max_continues: Option<u32>,      // L2 (default 2)
    #[serde(skip_serializing_if = "Option::is_none")] pub stall_minutes: Option<u32>,      // L1 (default 20)
    #[serde(skip_serializing_if = "Option::is_none")] pub silent_minutes: Option<u32>,     // L1, run printed no output (default 8)
    #[serde(skip_serializing_if = "Option::is_none")] pub max_hang_restarts: Option<u32>,  // L2 (default 3)
    #[serde(skip_serializing_if = "Option::is_none")] pub same_action_max: Option<u32>,    // no-progress repeat bound (default 3)
    #[serde(skip_serializing_if = "Option::is_none")] pub probe_interval_minutes: Option<u32>, // L6 (default 15)
    #[serde(skip_serializing_if = "Option::is_none")] pub planner_min_gap_minutes: Option<u32>, // L4/L5 (default 5)
    #[serde(skip_serializing_if = "Option::is_none")] pub idle_gap_minutes: Option<u32>,   // L4 (default 15)
    #[serde(skip_serializing_if = "Option::is_none")] pub checkpoint_minutes: Option<u32>, // L1 board/reports (default 120)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Loop { pub name: String, pub kind: LoopKind, pub project: Option<ProjectName>, pub interval_secs: u64,
                  pub state: ActiveState, pub bounds: LoopBounds, pub last_run_at: Option<Timestamp>,
                  pub next_run_at: Option<Timestamp>, pub last_status: String, pub fail_streak: u32, pub updated_at: Timestamp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ScheduleSpecKind { Cron, Interval }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum MissedPolicy { RunOnce, Skip }
/// What a schedule does when due.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleAction {
    JobTemplate { template: NewJob },                       // id gets a "-<yyyymmddhhmm>" suffix at creation
    Loop { loop_name: String },                             // trigger one run of a loop now (e.g. "l4-planner")
    Command { argv: Vec<String>, #[serde(default)] cwd: Option<String>, #[serde(default = "gate_timeout")] timeout_secs: u64 },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Schedule { pub id: ScheduleId, pub project: Option<ProjectName>, pub name: String, pub spec_kind: ScheduleSpecKind,
                      pub spec: String, pub tz: String, pub action: ScheduleAction, pub missed_policy: MissedPolicy,
                      pub state: ActiveState, pub last_run_at: Option<Timestamp>, pub next_run_at: Option<Timestamp>,
                      pub last_status: String, pub created_at: Timestamp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum LedgerKind { Dispatch, Gate, Merge, Cost, Switch, Limit, Promote, Rollback, Inbox, Schedule, State, Note }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LedgerEntry { pub id: Option<i64>, pub ts: Timestamp, pub kind: LedgerKind, pub project: Option<ProjectName>,
                         pub job_id: Option<JobId>, pub run_id: Option<RunId>, pub session_id: Option<SessionId>,
                         pub role: Option<RunRole> /* set on cost rows */,
                         pub provider: Option<String>, pub harness: Option<HarnessKind>, pub model: Option<String>,
                         pub cost: Option<Cost>, pub summary: String, pub detail: serde_json::Value }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ReleaseState { Planned, Guarded, Banked, Approved, Deploying, Probing, Live, Failed, RolledBack, Refused }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GuardResult { pub name: String, pub passed: bool, pub detail: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Release { pub id: ReleaseId, pub project: ProjectName, pub env: String, pub change_ref: String,
                     pub from_commit: Option<String>, pub to_commit: Option<String>, pub state: ReleaseState,
                     pub guards: Vec<GuardResult>, pub snapshot_path: Option<String>, pub rollback_cmd: Option<String>,
                     pub probe: serde_json::Value, pub approved_by: Option<String>, pub rolled_back_by: Option<ReleaseId>,
                     pub note: String, pub created_at: Timestamp, pub finished_at: Option<Timestamp> }
```

`ScheduleAction` JSON: `{"kind":"loop","loop_name":"l4-planner"}`; command: `{"kind":"command","argv":["vowp","backup"],"timeout_secs":1800}`. A full `Schedule` example is in §15 (`schedule.json`).

---

## 11. `vowp.toml` types (`config`)

Full annotated schema and example: `architecture.md` §5. Rust:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VowpConfig {
    #[serde(default)] pub node: NodeConfig,
    pub project: ProjectConfig,
    #[serde(default)] pub profiles: BTreeMap<String, Profile>,            // keys: planner keeper worker verifier + custom
    #[serde(default)] pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)] pub harnesses: BTreeMap<String, HarnessConfig>,     // optional per-harness overrides + generic cli entries
    #[serde(default)] pub env: BTreeMap<String, EnvConfig>,               // [env.work] [env.prod]
    #[serde(default)] pub gates: BTreeMap<String, Vec<Gate>>,             // named gate sets: [gates] default_build = [...]
    #[serde(default)] pub loops: BTreeMap<String, LoopConfig>,            // [loops.l4-planner] overrides
    #[serde(default)] pub schedules: Vec<ScheduleConfig>,                 // [[schedules]]
    #[serde(default)] pub capsule: CapsuleConfig,
    #[serde(default)] pub merge: MergeConfig,
}
impl VowpConfig {
    pub fn from_toml(s: &str) -> Result<Self, ProtocolError>;            // + validate()
    pub fn validate(&self) -> Result<(), ProtocolError>;                  // see architecture.md §5.3 rules
    pub fn with_defaults_for_tests() -> Self;
}
pub struct NodeConfig { pub name: NodeName /* default "local" */, pub max_workers: u32 /* 4 */, pub max_verifiers: u32 /* 2 */,
                        pub tick_secs: u64 /* 60 */, pub cwd_allow: Vec<String>, pub home: Option<String> /* VOWP_HOME override */,
                        pub tz_offset_minutes: i32 /* 0; local UTC offset used to read wall-clock reset times like "resets 11pm" */ }
pub struct ProjectConfig { pub name: ProjectName, pub path: Option<String>, pub remote: String /* "origin" */,
                           pub default_branch: String /* "main" */, pub check_cmds: Vec<String>, pub lock_files: Vec<String> /* ["Cargo.lock"] */,
                           pub worktree_root: Option<String>, pub always_allowed: Vec<String> /* paths any job may touch: ["Cargo.lock"] */,
                           pub push: bool /* true */ }
pub struct ProviderConfig { #[serde(default = "four")] pub max_concurrent: u32 /* 4 */,
                            #[serde(default)] pub on_credits_exhausted: OnCreditsExhausted /* park */,
                            #[serde(default)] pub prices: BTreeMap<String, PriceEntry> /* model -> $/M; default empty */ }
#[serde(rename_all = "snake_case")]
pub enum OnCreditsExhausted { #[default] Park }                                      // v0.1: only park (never switch silently)
pub struct PriceEntry { pub input_per_m: f64, pub output_per_m: f64, pub cache_read_per_m: Option<f64>, pub cache_write_per_m: Option<f64> }
pub struct HarnessConfig { pub binary: Option<String>, pub extra_args: Vec<String>, pub env_pass: Vec<String> /* env var NAMES to forward */,
                           pub permission_mode: Option<String>, pub mapping: Option<String> /* cli adapter: path to mapping .toml */ }
pub struct EnvConfig { pub checkout: String /* required */, #[serde(default)] pub deploy: Vec<String>, #[serde(default)] pub guards: Vec<String>,
                       #[serde(default)] pub approval: Approval /* human */,
                       #[serde(default)] pub serving_bank: Option<String>, #[serde(default)] pub probe: Option<String>, #[serde(default)] pub rollback: Option<String> }
#[serde(rename_all = "snake_case")]
pub enum Approval { #[default] Human, Policy }
pub struct LoopConfig { pub interval_secs: Option<u64>, pub state: Option<ActiveState>, #[serde(flatten)] pub bounds: LoopBounds }
                       // NOTE: serde cannot combine `flatten` with `deny_unknown_fields`, so LoopConfig (only) omits the attribute
                       // and VowpConfig::validate() rejects unknown keys by deserialising into toml::Table first.
pub struct ScheduleConfig { pub name: String, pub cron: Option<String>, pub every: Option<String>, pub tz: Option<String>,
                            pub missed: Option<MissedPolicy>, pub action: ScheduleAction }
pub struct CapsuleConfig { pub executor: Option<Executor> /* cheapest capable; default = last rung of the "keeper" profile */,
                           pub min_interval_secs: u64 /* 120 */, pub max_input_chars: u64 /* 200000 */, pub tail_turns: u32 /* 2 */ }
pub struct MergeConfig { pub rebase: bool /* true */, pub check_cmds: Vec<String> /* repo-wide; falls back to project.check_cmds */,
                         pub timeout_secs: u64 /* 2700 */, pub secret_scan: bool /* true */ }
```
All structs except `LoopConfig` use `#[serde(deny_unknown_fields)]`. **Every field that is not marked `required` carries `#[serde(default)]` (or a named
default fn) giving exactly the value in its comment**; `Default` impls match. Config enums (`OnCreditsExhausted`, `Approval`, `MissedPolicy`, `OnQuota`, `ActiveState`)
use `#[serde(rename_all = "snake_case")]`. `JobLoop` (§7) likewise: `#[serde(default = "...")]` on `until` (`gates`), `max_attempts` (3), `on_exhausted` (`escalate`).
The only required keys in the whole file are `[project] name`, each profile's `ladder`, each `[env.*] checkout`, and each `[[schedules]] name` + `action`.

### Node-level file (`~/.vowp/config.toml`)

`vowp.toml` requires `[project]`; the node file does not, so it has its own type with the node-wide subset:

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeFile {
    #[serde(default)] pub node: NodeConfig,
    #[serde(default)] pub profiles: BTreeMap<String, Profile>,
    #[serde(default)] pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)] pub harnesses: BTreeMap<String, HarnessConfig>,
    #[serde(default)] pub capsule: CapsuleConfig,
}
impl NodeFile { pub fn from_toml(s: &str) -> Result<Self, ProtocolError>; }
impl VowpConfig {
    /// Resolution order (architecture.md §5.3): values in `self` (vowp.toml) win; map entries missing in `self` are filled from `node_file`;
    /// scalar `[node]` fields are taken from the node file only where vowp.toml left them at the default (documented per field in tests).
    pub fn with_node_defaults(self, node_file: &NodeFile) -> VowpConfig;
}
```
`validate()` runs on the merged result (so a project may omit the four role profiles if the node file supplies them).

---

## 12. Adapter contract types (`adapter`)

Full behaviour: `adapters.md`. The trait lives in `vowp-protocol` so core and adapters never need to edit each other's files.
Blocking I/O, plain threads, `std::sync::mpsc`.

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdapterCapabilities {
    pub can_resume_native: bool,           // start(resume=Some(id)) continues the harness's own session
    pub can_inject_assistant_history: bool,// false for all v0.1 adapters: handoff = capsule + tail as plain text
    pub can_switch_model_in_session: bool,
    pub live_input: bool,                  // send() works while a run is active (claude --input-format stream-json); false in v0.1
    pub emits_reasoning: bool, pub emits_cost_receipts: bool, pub reports_resets_at: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult { pub ok: bool, pub version: Option<String>, pub path: Option<String>, pub detail: String }

#[derive(Debug, Clone)]
pub struct StartInput {
    pub session_id: SessionId, pub executor: Executor, pub cwd: std::path::PathBuf,
    pub prompt: String,                               // the user/continue/seed message for this run (turn)
    pub resume: Option<String>,                       // native session id to resume (only if can_resume_native)
    pub new_native_id: Option<String>,                // claude: pre-chosen uuid for --session-id
    pub env: Vec<(String, String)>,                   // extra env NAMES+values from [harnesses.*].env_pass, resolved by the caller
    pub permission_mode: Option<String>,
    pub system_note: Option<String>,                  // appended system prompt where supported (claude --append-system-prompt)
    pub event_log_hint: Option<std::path::PathBuf>,   // where the supervisor mirrors raw output (adapters may also tee raw lines here)
    pub timeout: Option<std::time::Duration>,         // overall wall clock cap (None = none; stalls are handled by L1)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub exit_code: Option<i32>, pub outcome: RunOutcome,
    pub native_session_id: Option<String>,
    pub cost: Cost,
    pub limit: Option<LimitEvent>,                    // set when outcome is Limit/Credits/Auth/Rate/Context
    pub result_text: Option<String>,                  // final assistant text if the harness reported one
    pub stop_reason: StopReason,
}

pub trait Adapter: Send + Sync {
    fn kind(&self) -> HarnessKind;
    fn probe(&self) -> ProbeResult;                                    // cheap: `--version`, never calls a model
    fn capabilities(&self) -> AdapterCapabilities;
    /// Spawn the harness in headless JSON mode for ONE run (one prompt, one turn). Never blocks for the run itself.
    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError>;
}
pub trait AdapterRun: Send {
    fn pid(&self) -> Option<u32>;
    /// Normalised events, in order. Ends (channel closes) after the final `HarnessEvent` of the run
    /// (TurnEnded, plus Limit/Error/Cost events as they occur). Adapters emit a terminal TurnEnded on EVERY exit path.
    fn events(&self) -> &std::sync::mpsc::Receiver<HarnessEvent>;
    fn send(&mut self, text: &str) -> Result<(), AdapterError>;        // Err(Unsupported) unless live_input
    fn interrupt(&mut self) -> Result<(), AdapterError>;               // SIGINT to the process group, then stop() after 5 s
    fn stop(&mut self) -> Result<(), AdapterError>;                    // idempotent: SIGTERM, 3 s, SIGKILL of the process group
    /// Block until exit and the reader threads drain; returns the classified summary. Idempotent after first call.
    fn wait(&mut self) -> Result<RunSummary, AdapterError>;
}
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("harness not found or not runnable: {0}")] Unavailable(String),
    #[error("failed to spawn {binary}: {source}")]     Spawn { binary: String, source: std::io::Error },
    #[error("unsupported: {0}")]                       Unsupported(&'static str),
    #[error("invalid mapping file {path}: {message}")] Mapping { path: String, message: String },
    #[error("io: {0}")]                                Io(#[from] std::io::Error),
    #[error("protocol violation from harness: {0}")]   Protocol(String),
}
```

Classification helpers shared by adapters live in `vowp-adapters` (see `adapters.md` §5), not here.

## 13. Service traits (`services`)

These decouple `vowp-core` modules from each other (loops ↔ session). Real implementations are in `vowp-core::session`;
tests of `loops` use fakes written in the test code.

```rust
pub trait SessionOps: Send + Sync {
    /// Create a session (row + dir + `session.created` event). Used for every job run, planner/keeper turn, probe.
    fn create_session(&self, req: NewSession) -> Result<SessionId, ServiceError>;
    /// Run one turn on `exec` (spawn adapter, append events, update runs/ledger rows), blocking until it ends.
    /// `seed` (capsule) is rendered as the prompt prefix when the session has no native session on this harness.
    fn run_turn(&self, req: TurnRequest, cancel: &CancelToken) -> Result<TurnResult, ServiceError>;
    /// Switch protocol (sessions.md §5). Writes capsule, stops, starts the target with the seed when a turn is started;
    /// a pure switch starts the target idle and delivers the seed with the next input. Records the switch.
    fn switch(&self, req: SwitchRequest) -> Result<Switch, ServiceError>;
    fn write_capsule(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, ServiceError>;
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSession { pub project: Option<ProjectName>, pub job_id: Option<JobId>, pub role: SessionRole, pub title: String,
                        pub cwd: std::path::PathBuf, pub parent: Option<SessionId>,
                        #[serde(default)] pub executor: Option<Executor> /* initial executor; `vowp session new --executor` */ }
#[derive(Debug, Clone)]
pub struct TurnRequest { pub session_id: SessionId, pub executor: ExecutorRef, pub purpose: RunPurpose, pub role: RunRole,
                         pub job_id: Option<JobId>, pub prompt: String, pub profile: Option<String>, pub rung: u32,
                         pub window: Option<String>, pub timeout: Option<std::time::Duration> }
pub struct TurnResult { pub run_id: RunId, pub summary: RunSummary }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SwitchRequest { pub session_id: SessionId, pub to: ExecutorRef, pub reason: SwitchReason, pub drain_secs: u64 /* 5 */ }
/// Cooperative cancellation shared between supervisor and runner threads (Arc<AtomicBool> inside).
#[derive(Clone, Default)] pub struct CancelToken(/* Arc<AtomicBool> */);
impl CancelToken { pub fn cancel(&self); pub fn is_cancelled(&self) -> bool; }
#[derive(Debug, thiserror::Error)]
pub enum ServiceError { #[error("{0}")] Failed(String), #[error("not found: {0}")] NotFound(String), #[error("limited: {0:?}")] Limited(LimitEvent),
                        #[error("cancelled")] Cancelled }
```

## 14. JSON Schema export (`schema`)

```rust
/// name -> JSON Schema document for every public wire type (the v0.1 set: EventEnvelope,
/// Capsule, CapsuleDraft, Job, NewJob, Gate, Evidence, ReportFrontMatter, BriefFrontMatter,
/// VowpConfig, Schedule, Release, InboxItem, Run, Session, Limit, Cost, ...; plus the v0.2
/// capacity, surface/lease, snapshot, catalog, vault, fleet, tunnel and migration types in
/// §16).
pub fn all_schemas() -> std::collections::BTreeMap<&'static str, serde_json::Value>;
pub fn schema_for_name(name: &str) -> Option<serde_json::Value>;
```
`vowp schema [name]` prints them. A test checks that every `types.md` example JSON validates against its schema (examples are
copied into `crates/vowp-protocol/tests/fixtures/*.json`).

---

## 15. More JSON examples (fixtures)

Each block is copied to `crates/vowp-protocol/tests/fixtures/<type>.json` and validated against its schema by `types_md_examples_validate`
(round trip: deserialize → serialize → equal `serde_json::Value`).

`Profile` (`profile.json`) and `Edge` / `EdgeKind` (`edge.json`):

```json
{"ladder":["claude-code:model-a@medium","opencode:provider/model-b#max"],"on_quota":"next_then_probe_back"}
{"src":"graph-engine","dst":"protocol","kind":"needs"}
```

`Run` (`run.json`):

```json
{"id":"01KA1V30E5H8K2M4N6P8Q0R2S4","job_id":"graph-engine","session_id":"01KA1V0Z7A0000000000000000","role":"worker","purpose":"start","profile":"worker","rung":0,
 "executor":"opencode:provider/model-b#max","provider":"provider","node":"server","native_session_id":"ses_e439617b6ffepWG4UNK521b6ly","pid":4242,
 "window":"job-graph-engine","cwd":"/home/user/project","started_at":"2026-11-08T20:01:00.000Z","ended_at":"2026-11-08T20:19:30.000Z",
 "exit_code":0,"outcome":"ok","limit_kind":null,"cost":{"usd":0.42,"tokens":{"input":81000,"output":9100,"reasoning":0,"cache_read":240000,"cache_write":0},"source":"receipt"},"detail":""}
```

`Session` (`session.json`):

```json
{"id":"01KA1V0Z7A0000000000000000","project":"vowp","job_id":null,"role":"user","title":"design chat","parent_session_id":null,"node":"local","cwd":"/home/user/project",
 "state":"idle","executor":"claude-code:model-a@medium","last_seq":42,"last_capsule_id":null,"log_bytes":18311,
 "created_at":"2026-11-08T19:30:00.000Z","updated_at":"2026-11-08T19:41:02.000Z"}
```

`InboxItem` (`inbox_item.json`):

```json
{"id":"01KA1V40F6J9M3N5P7Q9R1S3T5","project":"vowp","job_id":"promote-core","kind":"approval","title":"Promote abc1234 to prod?",
 "body":"Guards passed; diff equals the intended change.","options":["approve","reject"],"state":"open","answer":null,"answered_by":null,
 "created_at":"2026-11-08T23:10:00.000Z","answered_at":null}
```

`Loop` (`loop.json`):

```json
{"name":"l4-planner","kind":"L4","project":"vowp","interval_secs":1800,"state":"active","bounds":{"same_action_max":3},
 "last_run_at":"2026-11-08T22:30:00.000Z","next_run_at":"2026-11-08T23:00:00.000Z","last_status":"ok","fail_streak":0,"updated_at":"2026-11-08T22:30:01.000Z"}
```

`Release` (`release.json`):

```json
{"id":"01KA1V50G7K1N4P6Q8R0S2T4V6","project":"vowp","env":"prod","change_ref":"abc1234","from_commit":"def5678","to_commit":"abc1234","state":"live",
 "guards":[{"name":"clean_tree","passed":true,"detail":""},{"name":"head_equals_remote","passed":true,"detail":"abc1234"}],
 "snapshot_path":"/home/user/.vowp/banks/prod-20261108.tar","rollback_cmd":"./scripts/rollback.sh /home/user/.vowp/banks/prod-20261108.tar",
 "probe":{"status":200},"approved_by":"owner","rolled_back_by":null,"note":"","created_at":"2026-11-08T23:12:00.000Z","finished_at":"2026-11-08T23:14:00.000Z"}
```

`Schedule` (`schedule.json`):

```json
{"id":"01KA1V60H8M2P5Q7R9S1T3V5W7","project":"vowp","name":"morning-report","spec_kind":"cron","spec":"0 7 * * 1-5","tz":"+02:00",
 "action":{"kind":"loop","loop_name":"l4-planner"},"missed_policy":"run_once","state":"active","last_run_at":null,
 "next_run_at":"2026-11-09T05:00:00.000Z","last_status":"ok","created_at":"2026-11-08T19:00:00.000Z"}
```

`GateResult` (`gate_result.json`):

```json
{"id":42,"job_id":"graph-engine","run_id":null,"attempt":0,"gate_index":1,
 "gate":{"cmd":{"run":"cargo test -p vowp-graph","expect_exit":0,"expect_pattern":null,"timeout_secs":1800,"cwd":null}},
 "passed":true,"evidence":{"kind":"cmd","command":"cargo test -p vowp-graph","exit_code":0,"timed_out":false,
 "tail":"test result: ok. 31 passed","duration_ms":41234},"output_blob":null,"duration_ms":41234,"ts":"2026-11-08T20:03:00.000Z"}
```

`Worker` (`worker.json`):

```json
{"node":"server","slot":1,"kind":"worker","state":"busy","run_id":"01KA1V30E5H8K2M4N6P8Q0R2S4","job_id":"graph-engine",
 "window":"job-graph-engine","updated_at":"2026-11-08T20:05:00.000Z"}
```

`Switch` (`switch.json`):

```json
{"id":"01KA1V70J9N3Q6R8S0T2V4W6X8","session_id":"01KA1V0Z7A0000000000000000",
 "from_executor":"claude-code:model-a@medium","to_executor":"opencode:provider/model-b#max",
 "from_node":"local","to_node":"server","capsule_id":"cap_01KA1V2B000000000000000000","reason":"limit","state":"completed","error":null,
 "started_at":"2026-11-08T19:41:03.000Z","ended_at":"2026-11-08T19:41:08.000Z","at_seq":142}
```

---

## 16. v0.2 protocol additions

These types are the v0.2 wire/CLI/database shapes. The normative spec is
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) §5–§10 and §13.1; each module is
re-exported at the crate root and every type is exported by `vowp schema`.

### 16.1 Capacity (`capacity`)

```rust
#[serde(rename_all = "snake_case")]
pub enum SampleSource { Proc, Fallback }

pub struct HostSample {
    pub at: Timestamp, pub interval_secs: u64,
    pub mem_total_bytes: u64, pub mem_available_bytes: u64, pub swap_used_bytes: u64,
    pub load1: f64, pub load5: f64, pub load15: f64, pub cpu_count: u32,
    pub cpu_quota_cores: Option<f64>,
    pub disk_free_home_bytes: u64, pub disk_free_worktrees_bytes: Option<u64>,
    pub pressure_cpu_avg10: Option<f64>, pub pressure_mem_avg10: Option<f64>,
    pub pressure_io_avg10: Option<f64>, pub source: SampleSource,
}

#[serde(rename_all = "snake_case")]
pub enum UsageSource { Cgroup, ProcTree, Default }
#[serde(rename_all = "snake_case"), Default]
pub enum BuildMode { Cold, Warm, Incremental, #[default] Unknown }

pub struct RunUsage {
    pub run_id: RunId, pub project: ProjectName, pub job_id: Option<JobId>,
    pub job_kind: Option<JobKind>, pub role: RunRole,
    pub harness: Option<HarnessKind>, pub model: Option<String>, pub provider: Option<String>,
    pub node: NodeName, pub started_at: Timestamp, pub ended_at: Timestamp,
    pub duration_ms: u64, pub peak_rss_bytes: u64,
    pub anon_peak_bytes: Option<u64>, pub file_peak_bytes: Option<u64>,
    pub kernel_peak_bytes: Option<u64>, pub cpu_usec: u64,
    pub mean_cpu_cores: f64, pub max_cpu_cores: f64,
    pub build_mode: BuildMode, pub source: UsageSource,
    pub oom_killed: bool, pub exit_code: Option<i32>,
}

pub struct RemoteUsage {
    pub peak_rss_bytes: u64, pub anon_peak_bytes: Option<u64>,
    pub file_peak_bytes: Option<u64>, pub kernel_peak_bytes: Option<u64>,
    pub cpu_usec: u64, pub mean_cpu_cores: f64, pub max_cpu_cores: f64,
    pub build_mode: BuildMode, pub oom_killed: bool, pub source: UsageSource,
}

#[serde(rename_all = "snake_case")]
pub enum EstimateSource { Measured, Pooled, Default }

pub struct UsageEstimate {
    pub project: Option<ProjectName>, pub job_kind: Option<JobKind>,
    pub harness: Option<HarnessKind>, pub model: Option<String>,
    pub build_mode: Option<BuildMode>, pub samples: u32,
    pub ram_p50_bytes: u64, pub ram_p90_bytes: u64,
    pub cpu_p50_cores: f64, pub cpu_p90_cores: f64,
    pub duration_p50_ms: u64, pub duration_p90_ms: u64, pub source: EstimateSource,
}

pub struct AdmitRequest {
    pub project: ProjectName, pub job: Option<JobId>, pub kind: JobKind,
    pub role: RunRole, pub harness: Option<HarnessKind>, pub model: Option<String>,
    pub provider: Option<String>, pub node: NodeName,
}
pub enum AdmitDecision { Admit, Wait(WaitReason) }

#[serde(rename_all = "snake_case")]
pub enum WaitCode { Ram, Cpu, Disk, Provider, Brake, Target }
pub struct WaitReason { pub code: WaitCode, pub detail: String }

#[derive(Default)]
pub struct BrakeState { pub active: bool, pub reasons: Vec<BrakeReason>, pub since: Option<Timestamp> }
#[serde(rename_all = "snake_case")]
pub enum BrakeCode { MemoryPressure, SwapGrowth, DiskLow, Provider }
pub struct BrakeReason { pub code: BrakeCode, pub detail: String }

#[serde(rename_all = "snake_case")]
pub enum TargetMode { Auto, Fixed }
pub struct CapacityTarget {
    pub mode: TargetMode, pub target_workers: u32, pub advised_workers: Option<u32>,
    pub is_override: bool, pub set_by: String, pub set_at: Timestamp, pub reason: String,
}
impl Default for CapacityTarget { /* auto, 1 worker, set_by "supervisor" */ }

#[serde(rename_all = "snake_case")]
pub enum AdviceState { Open, Applied, Dismissed }
pub struct CapacityAdvice {
    pub id: String, pub created_at: Timestamp,
    pub current_target: u32, pub proposed_target: u32, pub mode: TargetMode,
    pub reasons: Vec<String>, pub expected_effect: String,
    pub inbox_id: Option<InboxId>, pub state: AdviceState,
    pub applied_at: Option<Timestamp>, pub applied_by: Option<String>,
}

#[derive(Default)]
pub struct RunningCapacity { pub runs: u32, pub ram_p90_bytes: u64, pub cpu_p90_cores: f64 }
pub struct WaitingJob { pub project: ProjectName, pub job: JobId, pub reason: WaitReason, pub since: Timestamp }

#[derive(Default)]
pub struct CapacitySnapshot {
    pub measured: Option<HostSample>, pub running: RunningCapacity, pub target: CapacityTarget,
    pub advised_target: Option<u32>, pub brake: BrakeState,
    pub reserve_ram_bytes: u64, pub min_disk_bytes: u64,
}
pub struct CapacityReport {
    pub generated_at: Timestamp, pub host: Option<HostSample>, pub target: CapacityTarget,
    pub running: RunningCapacity, pub brakes: BrakeState,
    pub reserve_ram_bytes: u64, pub min_disk_bytes: u64,
    pub estimates: Vec<UsageEstimate>, pub waiting: Vec<WaitingJob>, pub limits: Vec<Limit>,
}
```

### 16.2 Surfaces, leases, remote execution and orchestrators (`surface`)

```rust
#[serde(transparent)] pub struct SurfaceId(pub String);   // fresh ULID
pub fn new_lease_token() -> String;                       // two ULIDs, 160 random bits

#[serde(rename_all = "snake_case")]
pub enum SurfaceKind { Chat, Opencode, ClaudeCode, Mcp, Mirror, Api }

pub struct Surface {
    pub id: SurfaceId, pub kind: SurfaceKind, pub node: NodeName, pub holder: String,
    pub app: Option<String>, pub cwd: Option<String>, pub metadata: serde_json::Value,
    pub created_at: Timestamp, pub last_seen_at: Timestamp,
}

pub struct Lease {
    pub session_id: SessionId, pub surface_id: Option<SurfaceId>,
    pub surface_kind: SurfaceKind, pub node: NodeName, pub holder: String,
    pub token: String, pub acquired_at: Timestamp, pub heartbeat_at: Timestamp,
}
pub struct SurfaceRef {
    pub surface_id: SurfaceId, pub kind: SurfaceKind, pub holder: String,
    pub node: NodeName, pub acquired_at: Timestamp, pub heartbeat_at: Timestamp,
}
pub struct LeaseStatus {
    pub held: bool, pub holder: Option<String>, pub kind: Option<SurfaceKind>,
    pub node: Option<NodeName>, pub acquired_at: Option<Timestamp>,
    pub heartbeat_at: Option<Timestamp>, pub token: Option<String>,  // only on --acquire
}
pub struct TakeoverReport {
    pub previous: Option<String>, pub drained: bool, pub capsule_id: Option<CapsuleId>,
}

pub struct RemoteEvent {
    pub id: EventId, pub ts: Timestamp, #[serde(rename = "type")] pub event_type: EventType,
    pub actor: Actor, pub source: EventSource, pub payload: serde_json::Value,
    pub raw: Option<serde_json::Value>, pub replayable: bool,
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLine {
    Hello { session_id: SessionId, run_id: RunId, executor: String, node: NodeName,
            spool_id: String, lease: String },
    Event { event: RemoteEvent },
    Heartbeat { at: Timestamp },
    Summary { exit_code: Option<i32>, outcome: RunOutcome, native_session_id: Option<String>,
              cost: Cost, limit: Option<LimitEvent>, result_text: Option<String>,
              stop_reason: StopReason, usage: Option<RemoteUsage> },
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum IngestAck {
    Ack { event_id: EventId, seq: u64 },
    Duplicate { event_id: EventId, seq: u64 },
    Summary { run_id: RunId, done: bool },
    LeaseRevoked { reason: String, by: String, at: Timestamp, last_seq: u64 },
    Error { kind: ErrorKind, message: String },
}

pub struct TurnGrant {
    pub run_id: RunId, pub session_id: SessionId, pub executor: String,
    pub purpose: RunPurpose, pub node: NodeName, pub cwd: String, pub prompt: String,
    pub resume_native: Option<String>, pub native_hint: Option<String>,
    pub env_names: Vec<String>, pub lease_token: String, pub tail_seq: u64,
}

#[serde(rename_all = "snake_case")]
pub enum OrchestratorApp { Opencode, ClaudeCode, Chat }
pub struct JoinInstructions { pub desktop: String, pub phone: String, pub chat: String }
pub struct OrchestratorCard {
    pub name: String, pub session_id: SessionId, pub project: Option<ProjectName>,
    pub app: OrchestratorApp, pub state: String, pub join: JoinInstructions,
}

// Changed enums:
#[serde(rename_all = "snake_case")]
pub enum SessionRole { User, Worker, Planner, Verifier, Keeper, Orchestrator }
#[serde(rename_all = "snake_case")]
pub enum SwitchReason { User, Limit, Crash, Context, Failover, ProbeBack, Takeover }
```

### 16.3 Snapshot and project registry (`snapshot`)

```rust
pub const SNAPSHOT_FORMAT: &str = "vowp-snapshot/1";
pub const SNAPSHOT_VERSION: u32 = 1;

#[serde(rename_all = "snake_case")]
pub enum ProjectState { Active, Paused, Archived }

pub struct ProjectEntry {
    pub name: ProjectName, pub path: String, pub remote_url: Option<String>,
    pub default_branch: String, pub state: ProjectState, pub weight: u8,
    pub paused_reason: String, pub archived_at: Option<Timestamp>,
    pub created_at: Timestamp, pub updated_at: Timestamp,
}

pub struct SupervisorSnapshot {
    pub running: bool, pub pid: Option<u32>, pub started_at: Option<Timestamp>,
    pub last_tick: Option<Timestamp>, pub tick_secs: u64, pub paused: bool,
    pub fail_streak: u32, pub version: String,
}
pub struct RunningJobCard {
    pub job: JobId, pub kind: JobKind, pub role: Role, pub executor: Option<String>,
    pub node: NodeName, pub age_secs: u64, pub last_event_age_secs: Option<u64>,
    pub session_id: Option<SessionId>,
}
pub struct BlockedJobCard {
    pub job: JobId, pub state: JobState, pub reason: String,
    pub detail: String, pub since: Timestamp,
}
pub struct QueueJobCard {
    pub job: JobId, pub title: String, pub kind: JobKind, pub role: Role,
    pub priority: i32, pub state: JobState, pub age_secs: u64,
    pub waiting_reason: Option<String>,
}
pub struct MergeCard { pub job: JobId, pub at: Timestamp, pub commit: String, pub branch: String }
pub struct GateFailureCard { pub job: JobId, pub gate: String, pub at: Timestamp, pub evidence: String }
pub struct InboxSummary {
    pub id: InboxId, pub project: Option<ProjectName>, pub job_id: Option<JobId>,
    pub kind: InboxKind, pub title: String, pub created_at: Timestamp, pub age_secs: u64,
}
pub struct SessionSurfaceCard {
    pub id: SessionId, pub project: Option<ProjectName>, pub job_id: Option<JobId>,
    pub role: Option<SessionRole>, pub title: String, pub state: SessionState,
    pub executor: Option<String>, pub node: NodeName,
    pub active_surface: Option<SurfaceRef>, pub last_event_at: Timestamp,
}
pub struct ProjectSnapshot {
    pub name: ProjectName, pub path: String, pub state: ProjectState, pub weight: u8,
    pub paused_reason: String, pub jobs_by_state: BTreeMap<String, u32>,
    pub running: Vec<RunningJobCard>, pub blocked: Vec<BlockedJobCard>,
    pub queue: Vec<QueueJobCard>, pub last_merge: Option<MergeCard>,
    pub gate_failures: Vec<GateFailureCard>, pub cost_today_usd: Option<f64>,
}
pub struct Snapshot {
    pub schema: String, pub version: u32, pub sequence: u64, pub generated_at: Timestamp,
    pub home: NodeName, pub supervisor: SupervisorSnapshot,
    pub projects: Vec<ProjectSnapshot>, pub capacity: CapacitySnapshot,
    pub limits: Vec<Limit>, pub inbox: Vec<InboxSummary>,
    pub sessions: Vec<SessionSurfaceCard>, pub orchestrators: Vec<OrchestratorCard>,
}
```

The snapshot is derived. In this build the capacity card's `measured` is `null` and
`running.runs` is populated from the open runs, while its p90 RAM/CPU fields stay zero; a session
card's `active_surface` is `null`. `vowp capacity show` is the measured report and the lease is live
in `vowp session lease`.

### 16.4 Lifecycle catalog (`catalog`)

```rust
#[serde(rename_all = "snake_case")]
pub enum CatalogClass {
    Production, Truth, OpenWork, Secrets, ClosedSession,
    RunOutput, Worktree, Sample, Cache,
}
pub struct CatalogEntry {
    pub id: String, pub path: String, pub class: CatalogClass,
    pub project: Option<ProjectName>, pub environment: String,
    pub job_id: Option<JobId>, pub session_id: Option<SessionId>,
    pub bytes: Option<u64>, pub expires_at: Option<Timestamp>, pub note: String,
    pub created_at: Timestamp, pub updated_at: Timestamp, pub deleted_at: Option<Timestamp>,
}
pub struct GcClassReport { pub class: CatalogClass, pub files: u64, pub bytes: u64, pub sample_paths: Vec<String> }
pub struct GcReport {
    pub generated_at: Timestamp, pub dry_run: bool, pub classes: Vec<GcClassReport>,
    pub deleted_files: u64, pub freed_bytes: u64, pub compacted_sessions: u64,
    pub errors: Vec<String>,
}
pub struct BackupFile { pub path: String, pub bytes: u64, pub kind: String }
pub struct BackupReport {
    pub at: Timestamp, pub directory: String, pub files: Vec<BackupFile>,
    pub bytes: u64, pub offsite: Option<String>, pub encrypted: bool, pub skipped: Vec<String>,
}
pub struct CatalogFilter {
    pub class: Option<CatalogClass>, pub project: Option<ProjectName>,
    pub expired_before: Option<Timestamp>, pub limit: Option<u32>,
}
```

### 16.5 Vault (`vault`)

```rust
#[serde(transparent)] pub struct SecretName(pub String);  // ^[A-Z][A-Z0-9_]{0,63}$
pub struct SecretInfo {
    pub name: SecretName, pub last4: String,
    pub created_at: Timestamp, pub updated_at: Timestamp,
}
#[serde(rename_all = "snake_case")]
pub enum VaultMode { VaultUser, Basic }
pub struct SecretRequest {
    pub name: SecretName, pub url: String, pub expires_at: Timestamp, pub single_use: bool,
}
```

Values never appear in these types. The vault stores files, keeps only the last four characters
for display, and `secret request` returns a loopback URL and expiry, never the value.

### 16.6 CLI-local result types

Some `--json` results are defined next to their command because they are not protocol
storage types: `FleetStatus {snapshot, source, fetched_at, stale}`, `FleetQueueRow`, `Tunnel`,
`MigrationReport`, `MigrationCount` and the CLI status summaries. Those are exported by
`vowp schema` and have pages under [`docs/reference/schema/`](../reference/schema/README.md).
`ControlConfig` and the setup plan/report types are CLI-local `Serialize` shapes (not protocol
storage types); their fields are described in `v0.2/home-and-surfaces.md` §8 and §11.
