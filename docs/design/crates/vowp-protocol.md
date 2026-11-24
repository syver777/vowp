# crate `vowp-protocol`

Depends on nothing in the workspace. Everything else depends on it.

## 1. Purpose

The single definition of every wire/data type (ids, timestamps, events, capsule, job graph rows, gates, reports, config,
adapter trait, service traits) with serde + JSON Schema, plus the small pure behaviours attached to them (parsing, validation,
rendering). **No I/O** except `VowpConfig::from_toml(&str)` taking a string; no filesystem, no process, no clock reads except `SystemClock`.
All type definitions are in `types.md`; this file lists the module layout, the extra items types.md does not define, the behaviour
rules, and the tests.

## 2. Module layout

```
crates/vowp-protocol/
  Cargo.toml
  src/lib.rs          // #![forbid(unsafe_code)] #![warn(missing_docs)]; `pub mod` for each below; re-exports; `pub mod prelude`
  src/ids.rs          // types.md §1
  src/time.rs         // Timestamp, Clock, SystemClock, FakeClock
  src/error.rs        // ErrorKind, ProtocolError
  src/log.rs          // Log, Level, NullLog, MemoryLog
  src/cost.rs         // CostSource, Tokens, Cost
  src/limit.rs        // LimitKind, LimitRowKind, Limit
  src/executor.rs     // HarnessKind, Executor, ExecutorRef, Role, OnQuota, Profile
  src/event.rs        // Actor, EventSource, EventType, EventEnvelope, HarnessEvent, EventBody + payload structs, BlobRef
  src/capsule.rs      // Capsule, CapsuleDraft, ...
  src/job.rs          // JobKind, JobState, EdgeKind, Edge, Job, NewJob, JobPatch, JobLoop, ParkedReason
  src/gate.rs         // Gate, GateKind, Evidence, GateResult, SecretHit, default_gates
  src/report.rs       // Report, ReportFrontMatter, Brief, BriefFrontMatter, Verdict, ReportStatus
  src/run.rs          // Run, RunRole, RunPurpose, RunOutcome, Worker*
  src/offload.rs      // types.md §16.8: Placement, NodeFallback, WaitingReason, AssignmentPayload, RunRejectReason, RemoteRunState, RunResume*, SurfaceResume*
  src/session.rs      // Session, SessionState, SessionRole, EventIndexRow, Switch, SwitchState, Node, Project, LedgerEntry, LedgerKind
  src/inbox.rs        // InboxItem, InboxKind, InboxState
  src/loops.rs        // Loop, LoopKind, LoopBounds, ActiveState, Schedule*, MissedPolicy, ScheduleAction
  src/release.rs      // Release, ReleaseState, GuardResult
  src/config.rs       // VowpConfig and sub-structs
  src/api.rs          // v0.3 loopback API documents: Health, Board, BoardRun, BoardLimit, Page<T>, JobPage, JobAction, JobActionResponse, SessionPage, NodePage, SessionDetail, EventPage, InboxPage, InboxAnswerRequest, SessionSwitchRequest, CostReport, CostRow, CostTotal, ApiError
  src/adapter.rs      // Adapter, AdapterRun, StartInput, RunSummary, AdapterCapabilities, ProbeResult, AdapterError
  src/services.rs     // SessionOps, NewSession, TurnRequest, TurnResult, SwitchRequest, CancelToken, ServiceError
  src/schema.rs       // all_schemas, schema_for_name
  tests/fixtures/     // JSON examples from types.md, vowp.example.toml
  tests/*.rs
```

`src/agent.rs` holds the v0.4 local-node documents (`PathRef`, `AgentRole`,
`AgentCapabilities`, `AgentCapacity`, `AgentPathMap`, `NodeCard`, `NodeKind`, `NodeStatus`,
`NodePresence`, `SurfaceInfo`, `SurfaceScope`, `SurfaceState` and `AgentFrame`); see rule 14.
Its nested `agent::surface` module holds the Phase 3 app-surface frame payloads
(`SurfaceAttach`, `SurfaceDetach`, `SurfaceEvent`, `SurfaceTurn`, `SurfaceTurnState`,
`ToolCall`, `ToolResult` and `ToolError`); see rule 16 and types.md §16.12.
`src/surface.rs` holds the orchestrator card and the v0.4 node-command documents
(`OrchestratorCard`, `OrchestratorNodeState`, `OrchestratorCommand`, `OrchestratorResult` and
`OrchestratorError`); see rule 17.
`src/offload.rs` holds the v0.4 offload wire format (`Placement`, `NodeFallback`,
`WaitingReason`, `AssignmentPayload`, `RunRejectReason`, `RemoteRunState` and the resume
records); see rule 15.

`lib.rs` declares **all** modules and re-exports with `pub use <module>::*;` for every module.

## 3. Extra public items (not in types.md)

```rust
// ---- log.rs : logging without a logging crate (conventions.md §4) ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level { Debug, Info, Warn, Error }
impl Level { pub fn as_str(&self) -> &'static str; }   // "DEBUG" "INFO" "WARN" "ERROR"

/// Sink for supervisor/loop log lines. Implementations must be cheap and never panic.
pub trait Log: Send + Sync {
    fn log(&self, level: Level, target: &str, msg: &str);
}
/// Discards everything.
pub struct NullLog;
/// Collects `(Level, target, msg)` for tests. Clone shares the buffer.
#[derive(Clone, Default)]
pub struct MemoryLog { /* Arc<Mutex<Vec<(Level, String, String)>>> */ }
impl MemoryLog { pub fn new() -> Self; pub fn lines(&self) -> Vec<(Level, String, String)>; pub fn contains(&self, needle: &str) -> bool; }
/// Formats the canonical line: `2026-11-08T19:25:47.123Z INFO l1 job graph-engine: running -> gating`
pub fn format_log_line(ts: Timestamp, level: Level, target: &str, msg: &str) -> String;

// ---- lib.rs ----
/// `use vowp_protocol::prelude::*;` brings in: all id types, Timestamp, Clock, SystemClock, FakeClock, Executor, ExecutorRef,
/// HarnessKind, Role, Cost, CostSource, Tokens, LimitKind, EventEnvelope, EventBody, EventType, HarnessEvent, Actor, EventSource,
/// Capsule, Job, JobState, JobKind, Edge, EdgeKind, Gate, GateResult, Evidence, ErrorKind, ProtocolError, Log, Level.
pub mod prelude { pub use crate::{/* the list above */}; }

pub const BLOB_SPILL_BYTES: usize = 8192;     // tool output strictly larger than this is spilled to a blob (store/sessions.md §1)
pub const BLOB_PREVIEW_CHARS: usize = 512;
pub const PROTOCOL_VERSION: u32 = 1;          // bumped when any wire shape changes incompatibly; written into bundles
```

**Amendment to `types.md` §11 (already applied there):** `NodeConfig` gains `tz_offset_minutes: i32` (default 0) — the node's UTC
offset used by the adapters to read wall-clock reset times like `resets 11pm`. v0.1 has no tz database (decisions.md, open question 2).

## 4. Behaviour rules

1. **Strictness.** Config structs use `deny_unknown_fields`. Event payload structs do NOT (forward compatibility); `EventEnvelope` keeps
   `payload` as `serde_json::Value` so a reader never fails on payload shapes it cannot type. `EventBody::from_parts` is the strict step.
2. **`EventEnvelope::new`**: assigns a fresh `EventId`; `replayable = false` iff body is `AssistantReasoning`; otherwise `true`.
   `EventBody::to_value` then `from_parts` must be the identity for every variant (test `event_body_roundtrip_every_type`).
3. **Timestamp** always serialises as exactly `YYYY-MM-DDTHH:MM:SS.mmmZ`. Parsing accepts RFC 3339 with any offset and 0–9 fractional
   digits; fractional digits beyond 3 are truncated (not rounded). `Timestamp` ordering equals string ordering of `as_rfc3339()`.
4. **Ids.** ULID ids: 26 chars `[0-9A-HJKMNP-TV-Z]`, uppercase on output; parsing accepts lowercase and normalises to uppercase.
   `CapsuleId` = `cap_` + ULID. Slugs: `JobId` = `^[a-z0-9][a-z0-9-]{0,119}$` (max 120 chars); `NodeName` and `ProjectName` = `^[a-z0-9][a-z0-9-]{0,63}$` (max 64 chars); none may end in `-`.
   `new()` is monotonic within one process (shared `Mutex<ulid::Generator>`; on overflow in the same ms, wait 1 ms and retry).
5. **Executor grammar.** `harness ":" model [ "@" effort ] [ "#" variant ]`. `harness` ∈ {claude-code, opencode, cli, acp, vowp}.
   `model` non-empty, chars `[A-Za-z0-9._:/-]`. `effort`/`variant` `[a-z0-9_-]+`. `@effort` is only valid for `claude-code`, `#variant`
   only for `opencode`; both → `InvalidExecutor`. `Display(parse(s)) == s` for all valid `s` with canonical order (`@` before `#`).
   `provider()`: claude-code → `"claude"`; opencode → text before the first `/` (whole model if no `/`); cli/acp → model.
6. **`Profile::validate`**: ladder non-empty; no duplicate executor; every executor `validate`d; `node` slug-valid if set.
   **`Profile::pick(limited, now)`**: iterate rungs in order; skip a rung when `limited[provider]` is `None` (indefinite) or
   `Some(t)` with `t > now`; return the first remaining `(index, &Executor)`; `None` if all are skipped.
7. **`Cost::add`**: tokens always add. `usd`: both `Some` → sum; one `Some` → keep that value (partial) and degrade `source` to `Unknown`
   if the other side is `Unknown` with no usd, else the weaker of the two sources (`Receipt` > `PriceTable` > `Unknown`); both `None` → `None`.
   `Cost::unknown()` has `usd=None`, `source=Unknown`. Constructing `Cost{usd:Some(_), source:Unknown}` or `{usd:None, source != Unknown}` is a bug:
   `Cost::validate()` flags it.
8. **`Gate::from_short(s)`**: trims; `report`→`Report(default fields)`, `owns_respected`→`OwnsRespected(true)`, `review`→`Review(Verifier)`,
   `pushed`→`Pushed(origin, None)`, `secret_scan`→`SecretScan(true)`; empty → `InvalidGate`; anything else → `Cmd{run:s, expect_exit:0, ..defaults}`.
   `short()` is the inverse for gates that equal their defaults; a `Cmd` with non-default fields has no short form and `short()` returns its `run`
   (brief rendering is lossy by design; graph YAML export carries full gates).
9. **`Report::parse`**: input must start with a line `---`; front-matter ends at the next line that is exactly `---`; YAML parsed to
   `serde_json::Value` first (kept as `raw_front_matter`), then typed. Required: `job`, `status`, `branch`. `status` ∈ done|partial|blocked.
   Body = everything after the closing fence (leading newline stripped). `missing_fields` rules are in types.md §8.1.
10. **`Brief::from_job`**: `owns` = job.owns; `needs_done` = job.needs; `gates` = each `Gate::short()`; `must_not_touch` = `["everything else"]`;
    `done_when` = `"report has status: done and every gate passes"`.
11. **`VowpConfig::validate`**: rules listed in `architecture.md` §5.3 (profile names referenced by jobs exist; every ladder valid;
    provider referenced by a ladder executor need not be declared; env guards known names or `script:`/`no_active_jobs:` prefix; cron strings
    parse with the `cron` crate is checked in vowp-core `ScheduleSpec::parse`; here only: `cron`/`every` XOR, exactly 5 whitespace-separated cron fields, `every` pattern, tz format, unique names).
12. **Schema export**: `all_schemas()` keys are the Rust type names. The exported set is
    `"ApiError"`, `"ApiErrorBody"`, `"BackupFile"`, `"BackupReport"`, `"Board"`, `"BoardLimit"`,
    `"BoardRun"`, `"BlockedJobCard"`, `"BrakeState"`,
    `"BriefFrontMatter"`, `"CapacityAdvice"`, `"CapacityReport"`, `"CapacitySnapshot"`, `"CapacityTarget"`,
    `"Capsule"`, `"CapsuleDraft"`, `"CatalogEntry"`, `"Cost"`, `"CostReport"`, `"CostRow"`,
    `"CostTotal"`, `"Edge"`, `"EventEnvelope"`, `"EventPage"`,
    `"Evidence"`, `"FleetQueueRow"`, `"FleetStatus"`, `"Gate"`, `"GateFailureCard"`, `"GateResult"`,
    `"GcClassReport"`, `"GcReport"`, `"Health"`, `"HostSample"`, `"InboxAnswerRequest"`,
    `"InboxItem"`, `"InboxPage"`, `"InboxSummary"`,
    `"IngestAck"`, `"Job"`, `"JobAction"`, `"JobActionResponse"`, `"JobPage"`, `"JobPatch"`,
    `"JoinInstructions"`, `"Lease"`, `"LeaseStatus"`,
    `"LedgerEntry"`, `"Limit"`, `"Loop"`, `"MergeCard"`, `"MigrationCount"`, `"MigrationReport"`, `"NewJob"`,
    `"Node"`, `"NodePage"`, `"OrchestratorCard"`, `"OrchestratorCommand"`,
    `"OrchestratorError"`, `"OrchestratorNodeState"`, `"OrchestratorResult"`, `"Page"`, `"Profile"`, `"Project"`,
    `"ProjectEntry"`,
    `"ProjectSnapshot"`, `"QueueJobCard"`, `"Release"`, `"RemoteEvent"`, `"RemoteLine"`,
    `"ReportFrontMatter"`, `"Run"`, `"RunUsage"`, `"RunningCapacity"`, `"RunningJobCard"`, `"Schedule"`,
    `"SecretInfo"`, `"SecretRequest"`, `"Session"`, `"SessionDetail"`, `"SessionPage"`, `"SessionSwitchRequest"`,
    `"SessionSurfaceCard"`, `"Snapshot"`, `"SupervisorSnapshot"`, `"Surface"`, `"SurfaceRef"`, `"Switch"`, `"TakeoverReport"`,
    `"Tunnel"`, `"TurnGrant"`, `"UsageEstimate"`, `"VowpConfig"`, `"WaitReason"`, `"WaitingJob"`, `"Worker"`.
    The v0.4 local-node set adds `"AgentCapabilities"`, `"AgentCapacity"`, `"AgentFrame"`,
    `"AgentPathMap"`, `"AgentRole"`, `"NodeCard"`, `"NodeKind"`, `"NodePresence"`, `"NodeStatus"`,
    `"PathRef"`, `"SurfaceInfo"`, `"SurfaceScope"`, `"SurfaceState"`, `"LeaseMode"`, `"TurnSummary"`.
    The v0.4 app-surface set adds `"SurfaceAttach"`, `"SurfaceDetach"`, `"SurfaceEvent"`,
    `"SurfaceTurn"`, `"SurfaceTurnState"`, `"ToolCall"`, `"ToolError"`, `"ToolResult"`.
    The v0.4 offload set adds `"AssignmentPayload"`, `"NodeFallback"`, `"Placement"`,
    `"RemoteRunState"`, `"RunRejectReason"`, `"RunResume"`, `"RunResumeResult"`,
    `"SurfaceResume"`, `"SurfaceResumeResult"`, `"WaitingReason"`.
    The v0.5 set adds `"ClusterCapacity"`, `"OrchestratorLocation"`, `"OrchestratorAuth"` and
    `"OrchestratorEndpoint"`. `VaultMode` is a protocol enum but is not in `all_schemas()`;
    `SecretInfo` and `SecretRequest` remain the exported vault documents.
    Output is deterministic (BTreeMap, stable ordering).
13. **No panics**: all constructors/parsers return `Result`; regex literals inside use `// invariant:` comments.
14. **v0.4 agent protocol.** `PathRef` serialises as one string (`project:vowp`,
    `worktree:vowp/<job>`, `subdir:vowp/src`) and parses strictly: no `..`, leading `/`, `\`, NUL
    or empty project/subdirectory, at most `MAX_PATH_REF_LEN` bytes, with repeated `/` and `.`
    components normalised. `AgentFrame` is the tagged union (`type` is `snake_case`):
    `AgentHello`, `HomeChallenge`, `AgentAuth`, `HomeWelcome`, `Enroll`, `Enrolled`, `Heartbeat`,
    `SurfacePair`, `SurfaceIssued`, `SurfaceRevoked`, `SurfaceAttach`, `SurfaceDetach`,
    `SurfaceEvent`, `SurfaceTurn`, `ToolCall`, `ToolResult`, `LeaseAcquire`, `LeaseHeartbeat`,
    `LeaseRelease`, `LeaseGrant`, `LeaseLost`, `LeaseRevoked`, `TurnBegin`, `TurnGrant`,
    `RemoteLine`, `IngestAck`, `Credit`, `TurnEnd`, `RunLost`, `AssignRun`, `RunAccepted`,
    `RunRejected`, `RunState`, `CancelRun`, `Resume`, `ResumeOk`, `PathMapUpdate`, `Cleanup`,
    `OrchestratorCommand`, `OrchestratorResult`,
    `Ping`, `Pong`, `Revoked`, `Superseded` and `Error`. `SurfacePair.code` and `SurfaceIssued.token` are secret frame values: never logged,
    echoed or put in a URL. `SurfaceScope` is a transparent string (`session.read:<id>`,
    `fleet.read`, `tool:<name>`); `SurfaceState` is `pending|attached|revoked`; `SurfaceInfo` is
    the state-bearing read model and carries no token. `NodeCard` carries no path map and no
    secret name; `NodeStatus`/`NodePresence` are separate so a revoked node is always `revoked`.
    `LeaseMode` is `acquire|takeover`. `LeaseGrant.lease_token`, `LeaseAcquire.surface_token`,
    `LeaseHeartbeat.token` and `LeaseRelease.token` are secret frame values: never logged,
    echoed or written to the database in plaintext; the store keeps only SHA-256 hashes.

15. **v0.4 offload wire format.** `Placement` serialises as one string — `home`, `any`,
    `node:<name>` or `node:<name>+home` — and parses strictly: an unknown selector, an empty
    node, whitespace, `/` or `:` inside the node part and an unknown fallback are rejected with
    `InvalidInput`. The node is a `NodeName` slug; `Default` is `Home`, and `+home` is the only
    explicit fallback (there is no silent one). `WaitingReason` is the §7.5 vocabulary and
    `RunRejectReason` the pre-start refusal vocabulary, both `snake_case`. `AssignmentPayload`
    is the durable, token-free payload carried by `AssignRun`; it contains environment and
    secret names only, a project-relative `PathRef`, the git remote name, the home project's
    expected remote URL (userinfo stripped before storage), and never a lease token, a secret
    value or an absolute local path. The agent compares its push URL (not the fetch URL) with
    `remote_url`, with credentials stripped on both sides, and refuses a mismatch as
    `remote_mismatch`; it never logs the raw URL. `AssignRun.lease_token` is frame-only: the
    home stores only its hash and re-issues a token on resume. `RemoteRunState` is
    `running|disconnected|finishing`.
    `RunResume`/`SurfaceResume` carry `last_acked`; `RunResumeResult`/`SurfaceResumeResult`
    carry `accepted`, an optional `reason` and the `tail_seq` the holder may continue from.

16. **v0.4 local app-surface frames.** The payload types live in the nested `agent::surface`
    module: `SurfaceTurnState` (`started|ended`), `SurfaceAttach { surface, session }`,
    `SurfaceDetach { surface, session }`, `SurfaceEvent { surface, session, native_id, event }`,
    `SurfaceTurn { surface, session, native_id, state, at }`, `ToolCall { request_id, surface,
    session, tool, arguments }`, `ToolResult { request_id, result: Option<Value>, error:
    Option<ToolError> }` and `ToolError { code, message, retryable }`. They sit in a module
    because `event::ToolCall` and `event::ToolResult` already exist; a root re-export of both
    pairs would be ambiguous. `SurfaceEvent.event` serialises as the typed `EventBody` with a
    dotted `type` discriminator and decodes through `EventBody::from_parts`. `AgentFrame` adds
    the agent → home variants `SurfaceAttach`, `SurfaceDetach`, `SurfaceEvent`, `SurfaceTurn`
    and `ToolCall`, and the home → agent variant `ToolResult`; `SurfacePair`, `SurfaceIssued`,
    `SurfaceRevoked`, the lease frames, `RemoteLine`, `IngestAck` and the Phase 2 run frames
    keep their names and shapes. The home assigns the canonical `seq` and deduplicates by
    native id; the agent never sends `seq`, and a native id is stable across retries and
    backfill. `ToolCall.arguments` is normal MCP JSON and by contract never a local path or
    secret value; the home checks the call against the surface token scope and the live lease
    before executing it. `ToolResult` never carries a home secret value or an absolute local
    path, and its failure is the typed `ToolError` rather than a free string.

17. **v0.4 orchestrator node commands.** `OrchestratorNodeState` is
    `starting|running|stopped|degraded|node_offline|failed`; `OrchestratorCard.state` is that
    typed value and `OrchestratorCard.node` is `Option<NodeName>` (`None` on a v0.2 home card).
    `OrchestratorCommand` is the internally tagged (`action`) home → agent payload
    `start|stop|resume`; every variant carries a home-assigned `request_id`, and `start` also
    carries the app and optional project. `OrchestratorResult` is the agent → home reply with
    `request_id`, `accepted`, the resulting state, an optional typed `OrchestratorError`
    (`code`, `message`, `retryable`) and an optional redacted `detail`. `AgentFrame` adds the
    newtype variants `OrchestratorCommand` and `OrchestratorResult`; the inner tagged maps are
    flattened under the outer `type` tag. Neither frame carries a port, password, pair link,
    absolute local path or secret value.

18. **v0.5 cluster and endpoint documents.** `ClusterCapacity` is additive and
    `serde(default)` on `CapacityReport` and `CapacitySnapshot`: `online_worker_nodes` counts
    online, active agent nodes (the home is not counted), `target_workers` and `busy_workers`
    are the home totals plus each online node's `max_slots` and `busy_slots`, and the CPU,
    memory and free-disk values are saturating sums. A document from before the field reads as
    an all-zero card. `OrchestratorLocation` is `home|node`; `OrchestratorAuth` carries
    `header`, `scheme`, `username` and `password`; `OrchestratorEndpoint` carries `name`, `app`,
    `session_id`, `location`, `node`, `state`, `url`, `native_session_id` and `auth`. Every
    field is always present and an unavailable value is `null`. `OrchestratorAuth`'s custom
    `Debug` prints `[REDACTED]` for the password, and no card, snapshot, log or error carries
    the value. The URL is loopback-only. `VaultMode` adds `encrypted` as the wire value for the
    age-keyfile mode; `SecretInfo` still carries only the name, last four characters and
    timestamps.

## 5. Error enum

`ProtocolError` (types.md §2). `kind()` mapping: `InvalidId|InvalidTimestamp|InvalidExecutor|InvalidGate|FrontMatter → InvalidInput`,
`InvalidPayload|Json → Parse`, `Config → Config`.

## 6. Named tests (hermetic: pure functions; fixtures under `crates/vowp-protocol/tests/fixtures/`)

| Test | Asserts |
|---|---|
| `id_parse_roundtrip_all_types` | every id type: `parse(x.to_string()) == x`; invalid length/charset/uppercase-normalisation cases rejected/accepted per rule 4 |
| `slug_ids_reject_bad_input` | `""`, `"-a"`, `"A"`, 65 chars, `"a b"`, `"a-"` → `InvalidId`; `"graph-engine"` ok |
| `ulid_new_is_monotonic` | 10 000 `EventId::new()` strictly increasing as strings, including within one ms |
| `capsule_id_prefix` | `CapsuleId::new()` starts with `cap_`, `.ulid()` is 26 chars |
| `timestamp_roundtrip_and_ms_truncation` | `"2026-11-08T19:25:47.123456Z"` → `as_rfc3339()=="…47.123Z"`; `from_unix_millis(ms).unix_millis()==ms` |
| `timestamp_normalises_offsets` | `"2026-11-08T21:25:47+02:00"` equals `"2026-11-08T19:25:47.000Z"` |
| `timestamp_ordering_matches_strings` | proptest: `a<b ⇔ a.as_rfc3339()<b.as_rfc3339()` |
| `fake_clock_advances` | `FakeClock::at(t)`, `advance(90s)` → `now()==t+90s`; clones share state |
| `executor_parse_table` | valid: the three examples in types.md §3 + `opencode:anthropic/claude-x#high` + `acp:claude-code-acp`; invalid: no colon, `@` on opencode, `#` on claude-code, `@`/`#` on acp, empty model, unknown harness, `claude-code:m@Hi` → `InvalidExecutor`; each valid one `to_string()` back to input |
| `executor_provider_and_model_arg` | `provider()` = `claude` for claude-code, the text before `/` for opencode, the model/entry name for cli and acp; `model_arg()` round-trips the model and optional variant |
| `executor_serde_is_string` | `serde_json::to_string(&e)` is a JSON string equal to Display; deserialising bad string errors |
| `profile_validate_rejects_empty_and_duplicates` | empty ladder, duplicate rung → error |
| `profile_pick_skips_limited_rungs` | 2-rung ladder: claude limited until future → rung 1; limited until past → rung 0; `None` (indefinite) → skipped; both limited → `None` |
| `cost_add_source_degradation` | receipt+receipt=receipt sum; receipt+unknown → source unknown with partial usd; price-table+receipt → price-table |
| `cost_validate_flags_inconsistent` | `{usd:Some, source:Unknown}` fails `validate` |
| `tokens_add_and_total` | field-wise sum; `total()` |
| `session_created_user_role_roundtrips` | `SessionCreated{role: Some(SessionRole::User), ..}` serialises `"role":"user"` and deserialises back (user sessions exist: `vowp session new`) |
| `event_body_roundtrip_every_type` | one sample for each of the 17 types — `session.created`, `session.executor`, `turn.started`, `turn.ended`, `user.message`, `assistant.message`, `assistant.reasoning`, `tool.call`, `tool.result`, `file.change`, `plan`, `limit`, `cost`, `switch`, `capsule`, `error`, `note` — `from_parts(t, body.to_value()) == body` and `event_type()==t` |
| `event_type_strings_stable` | `EventType::as_str()`/serde names equal the dotted strings in schema.sql's `session_events.type` CHECK (parses `include_str!("../../../docs/design/schema.sql")` — path relative to `crates/vowp-protocol/tests/`; the file must exist in the repo layout, there is **no literal fallback**; the test fails if the file or the CHECK list is missing) |
| `envelope_new_sets_replayable` | reasoning → `replayable=false`; others `true`; json omits `raw` when `None` |
| `envelope_forward_compat` | JSON with an extra top-level key and an extra payload key still deserialises; unknown `type` string fails (strict EventType) while the typed `body()` of an `error` payload with extra keys succeeds |
| `envelope_json_matches_fixture` | the three example lines in types.md §5.3 (`fixtures/events.jsonl`) deserialise and re-serialise to equal `serde_json::Value`s |
| `harness_event_summary_len` | `EventBody::summary()` ≤ 200 chars, single line, for a 10 KB multi-line message |
| `capsule_validate_rules` | empty intent, version 2, `at_seq==0`, empty list item each rejected; fixture capsule (types.md §6) valid |
| `capsule_render_markdown_golden` | rendering of the fixture capsule equals `tests/fixtures/capsule.md` byte-for-byte (sections `## Intent`, `## Constraints`, `## Decisions`, `## State` with `### Done/In progress/Blocked/Next`, `## Files`, `## Gotchas`, `## Open questions`; empty sections omitted) |
| `capsule_draft_json_schema_has_required_fields` | schema for `CapsuleDraft` lists `intent` and `state` as required |
| `gate_short_form_table` | the table in rule 8, both directions; `"cargo test -p x"` → Cmd with `expect_exit 0` and timeout 1800 |
| `gate_serde_matches_plan_yaml` | `{"cmd":{"run":"x","expect_exit":0}}`, `{"owns_respected":true}`, `{"review":{"by":"verifier"}}` round-trip; unknown gate key errors |
| `gate_validate_rejects_bad_regex_and_empty_run` | `expect_pattern:"("` → `InvalidGate` |
| `job_patch_json_roundtrip_with_loop_and_defaults` | `{}` → all-default patch; `{"loop":{"until":"gates","max_attempts":2,"on_exhausted":"park"}}` round-trips through the `loop` key; `add_needs` omitted deserializes to empty |
| `default_gates_per_kind` | build ⇒ has report+owns_respected+secret_scan+review; verify ⇒ only report; decision ⇒ empty |
| `evidence_serde_tagged` | each `Evidence` variant round-trips with `"kind"` tag; `SecretHit` has no value field |
| `report_parse_good` | fixture `report_ok.md` (structured and free-string `tests`) parses; body preserved |
| `report_parse_bad_cases` | no fence, unterminated fence, invalid YAML, missing `branch`, `status: finished` → `FrontMatter` error each |
| `report_missing_fields` | required list `[status,branch,commits,tests]` on a report lacking `tests` → `["tests"]`; empty `commits: []` counts as present |
| `brief_roundtrip_and_from_job` | `Brief::parse(render(b)) == b`; `from_job` output matches types.md §8.2 example |
| `config_example_parses` | `VowpConfig::from_toml(include_str!("fixtures/vowp.example.toml"))` is Ok and `validate()` Ok; asserts profile `planner` has 2 rungs, the example provider's `max_concurrent==6`, 2 schedules (`morning-report`, `nightly-backup`), `providers.claude.prices` empty and `env.work.approval == Human` (defaults applied). **`tests/fixtures/vowp.example.toml` must be byte-identical to the TOML block in `architecture.md` §5.2** (the fixture is copied from it; a repo-level test `example_matches_architecture_md` reads `include_str!("../../../docs/design/architecture.md")` (must exist), extracts the first ```toml block of §5.2 and compares) |
| `config_rejects_unknown_fields_and_bad_values` | unknown key in `[node]`, both `cron` and `every` on a schedule, empty ladder, `max_workers=0` → errors |
| `config_defaults_are_documented_values` | `NodeConfig::default()`: name `local`, 4, 2, 60, tz 0; `MergeConfig::default()`: rebase true, 2700, secret_scan true |
| `schema_export_covers_listed_types` | `all_schemas()` has every key of rule 12; each value is an object with `"$schema"` or `"type"`/`"definitions"`; deterministic across two calls |
| `inbox_answer_request_has_one_required_answer_field` | `InboxAnswerRequest { answer }` serialises to `{"answer":"…"}`, round-trips, and its exported schema requires `answer` |
| `session_switch_request_defaults_reason_and_force_and_has_a_schema` | `SessionSwitchRequest { to }` defaults `reason` to `user` and `force` to false, serialises all three fields, round-trips, and its exported schema requires only `to` |
| `job_action_response_has_a_schema_and_stable_action_names` | `JobAction` serialises to `retry`, `park`, `cancel`, `nudge`, `restart`, `replan` and `fail`; the exported `JobActionResponse` schema requires `job`, `action` and `request`, and a response round-trips |
| `types_md_examples_validate` | each JSON example fixture (`job.json`, `capsule.json`, `cost.json`, `limit.json`, `events.jsonl`, `profile.json`, `edge.json`, `run.json`, `session.json`, `inbox_item.json`, `loop.json`, `release.json`, `schedule.json`, `gate_result.json`, `worker.json`, `switch.json`) deserialises into its Rust type and validates against its exported schema (use a tiny structural checker: required properties present, no unknown top-level keys for `deny_unknown_fields` types) |
| `log_format_line` | `format_log_line` golden string; `MemoryLog` records in order |
| `service_cancel_token` | clones share cancellation; `ServiceError` Display strings |
| `path_ref_parses_project_worktree_and_subdir` | the three wire forms parse, display and `FromStr` identically; repeated `/` and `.` normalise |
| `path_ref_rejects_traversal_absolute_backslash_and_nul` | `..`, leading `/`, `\`, NUL, empty project/subdirectory, unknown kind and over-length input all return `InvalidInput` |
| `path_ref_round_trips_through_json` | each form serialises as its Display string and deserialises back; a traversal string fails |
| `agent_frame_round_trips_every_phase_zero_variant` | all 12 Phase 0 variants have their `snake_case` `type` and round-trip through JSON |
| `agent_frame_round_trips_surface_pair_frames` | `surface_pair`, `surface_issued` and `surface_revoked` round-trip with their `snake_case` type tags |
| `agent_frame_round_trips_lease_frames` | `lease_acquire`, `lease_heartbeat`, `lease_release`, `lease_grant`, `lease_lost` and `lease_revoked` round-trip; `LeaseMode` is `acquire`/`takeover` and an unknown mode fails |
| `agent_frame_rejects_unknown_type` | an unknown `type`, an unknown surface variant and a missing required field all fail |
| `surface_types_round_trip_and_have_stable_wire_names` | `SurfaceState` is `pending`/`attached`/`revoked`, `SurfaceScope` is a string and `SurfaceInfo` round-trips |
| `node_card_serializes_without_absolute_paths_or_secret_names` | no absolute string, path-map field or secret name appears in the card JSON |
| `all_schemas_includes_agent_types` | every v0.4 agent type is in `all_schemas()` and `schema_for_name` |
| `surface_kind_agent_wire_name_is_agent` | `SurfaceKind::Agent` serialises as `"agent"` and back |
| `placement_round_trips_home_any_and_node` | `home`, `any` and `node:<name>` display, parse and JSON-round-trip; `Default` is `home` |
| `placement_with_home_fallback_round_trips` | `node:<name>+home` keeps `Some(NodeFallback::Home)` through display, parse and JSON |
| `placement_rejects_an_unknown_or_malformed_selector` | unknown selectors, empty/uppercase/whitespace/slashed/colonned nodes and unknown fallbacks return `InvalidInput` |
| `waiting_reason_matches_the_documented_wire_values` | all 14 §7.5 values round-trip and an unknown value fails |
| `assignment_payload_round_trips_without_a_lease_token` | the payload round-trips, has no `lease_token` and no absolute local path, and keeps `cwd_ref` a `PathRef` string |
| `assignment_payload_carries_the_expected_remote_url` | `remote` stays the git remote name, `remote_url` carries the expected URL with no userinfo, and both round-trip through JSON |
| `assign_run_frame_carries_the_lease_token_but_the_payload_does_not` | `AssignRun.lease_token` is frame-only; the nested payload never contains it |
| `run_reject_reason_covers_the_documented_values` | all 11 refusal values round-trip and an unknown value fails |
| `resume_frame_round_trips_runs_and_surfaces` | `Resume` and `ResumeOk` carry run/surface records and results through JSON with their type tags |
| `path_map_update_round_trips_an_agent_path_map` | `PathMapUpdate` round-trips its owner-only `AgentPathMap` |
| `run_state_reports_the_remote_state_and_head_commit` | every `RemoteRunState` value serialises as its wire name and `RunState` round-trips |
| `agent_frame_round_trips_run_assignment_frames` | `assign_run`, `run_accepted`, `run_rejected`, `run_state`, `cancel_run` and `cleanup` round-trip with their type tags |
| `agent_frame_round_trips_assign_run_with_the_remote_url` | an `assign_run` frame carries `remote_url` next to the remote name and round-trips with it |
| `unknown_frame_type_is_rejected` | an unknown type and a missing required field both fail and name the offending type/field |
| `agent_frame_round_trips_surface_attach_and_detach` | `surface_attach` and `surface_detach` round-trip with their `surface`/`session` fields and carry no lease token |
| `surface_event_round_trips_with_a_native_id` | `surface_event` keeps its `native_id` and typed event body, never carries `seq`, and round-trips |
| `surface_turn_round_trips_started_and_ended` | `SurfaceTurnState` is `started`/`ended` and both states round-trip inside `surface_turn` |
| `tool_call_and_result_round_trip` | `tool_call` and `tool_result` round-trip with JSON arguments/results and no absolute local path |
| `tool_result_error_is_typed` | a failed `tool_result` carries the typed `ToolError` object and rejects a free-string error |
| `unknown_surface_frame_type_is_rejected` | an unknown surface `type`, a missing field and an unknown turn state all fail |
| `agent_frame_round_trips_orchestrator_commands_and_results` | `start`, `stop` and `resume` carry the same `request_id`/`name` fields, and accepted/failed results round-trip with their typed error and no absolute path |
| `orchestrator_node_state_wire_names_are_stable` | all six `OrchestratorNodeState` values serialise to their documented names, display identically and reject an unknown value |
| `orchestrator_card_carries_its_node_and_typed_state` | a card carries `node` and a typed state, round-trips, and its JSON has no path, token or password |
| `orchestrator_card_decodes_a_v0_2_row_without_a_node` | a v0.2 card without `node` decodes to `node: None` and its old `running` state |
| `orchestrator_endpoint_round_trips_home_and_node_documents` | home and node `OrchestratorEndpoint` documents round-trip; `location` is `home`/`node`, the home document carries `url`/`native_session_id`/Basic `auth`, and the node document has all three null |
| `orchestrator_endpoint_schema_is_exported_and_auth_debug_redacts_the_password` | `OrchestratorLocation`, `OrchestratorAuth` and `OrchestratorEndpoint` have schemas; `OrchestratorAuth` requires `header`/`scheme`/`username`/`password`; its `Debug` and the endpoint's `Debug` print `[REDACTED]` and never the password |

## 7. Out of scope / v0.2

ACP event mapping, permission.request/resolve events, IANA timezone support, signed/hash-chained logs (field reserved: none in v0.1).
The v0.3 loopback API documents live in `src/api.rs`; see `docs/design/v0.3/http-api.md`.
The v0.4 local-node protocol lives in `src/agent.rs`; see `docs/design/v0.4/local-orchestrator.md`
§8.3 and §11.3. Surface pairing, lease control and the local-chat stream are defined there.
The Phase 2 offload wire format lives in `src/offload.rs`; the assignment store, dispatcher,
policy evaluation, agent executor and CLI commands that use it are later phases and are not
defined here.
