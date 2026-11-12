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
  src/session.rs      // Session, SessionState, SessionRole, EventIndexRow, Switch, SwitchState, Node, Project, LedgerEntry, LedgerKind
  src/inbox.rs        // InboxItem, InboxKind, InboxState
  src/loops.rs        // Loop, LoopKind, LoopBounds, ActiveState, Schedule*, MissedPolicy, ScheduleAction
  src/release.rs      // Release, ReleaseState, GuardResult
  src/config.rs       // VowpConfig and sub-structs
  src/api.rs          // v0.3 loopback API documents: Health, Board, BoardRun, BoardLimit, Page<T>, JobPage, SessionPage, NodePage, SessionDetail, EventPage, InboxPage, InboxAnswerRequest, SessionSwitchRequest, CostReport, CostRow, CostTotal, ApiError
  src/adapter.rs      // Adapter, AdapterRun, StartInput, RunSummary, AdapterCapabilities, ProbeResult, AdapterError
  src/services.rs     // SessionOps, NewSession, TurnRequest, TurnResult, SwitchRequest, CancelToken, ServiceError
  src/schema.rs       // all_schemas, schema_for_name
  tests/fixtures/     // JSON examples from types.md, vowp.example.toml
  tests/*.rs
```

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
   `CapsuleId` = `cap_` + ULID. Slugs (`JobId`, `NodeName`, `ProjectName`): `^[a-z0-9][a-z0-9-]{0,63}$`, no trailing `-`.
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
    `"IngestAck"`, `"Job"`, `"JobPage"`, `"JobPatch"`, `"JoinInstructions"`, `"Lease"`, `"LeaseStatus"`,
    `"LedgerEntry"`, `"Limit"`, `"Loop"`, `"MergeCard"`, `"MigrationCount"`, `"MigrationReport"`, `"NewJob"`,
    `"Node"`, `"NodePage"`, `"OrchestratorCard"`, `"Page"`, `"Profile"`, `"Project"`, `"ProjectEntry"`,
    `"ProjectSnapshot"`, `"QueueJobCard"`, `"Release"`, `"RemoteEvent"`, `"RemoteLine"`,
    `"ReportFrontMatter"`, `"Run"`, `"RunUsage"`, `"RunningCapacity"`, `"RunningJobCard"`, `"Schedule"`,
    `"SecretInfo"`, `"SecretRequest"`, `"Session"`, `"SessionDetail"`, `"SessionPage"`, `"SessionSwitchRequest"`,
    `"SessionSurfaceCard"`, `"Snapshot"`, `"SupervisorSnapshot"`, `"Surface"`, `"SurfaceRef"`, `"Switch"`, `"TakeoverReport"`,
    `"Tunnel"`, `"TurnGrant"`, `"UsageEstimate"`, `"VowpConfig"`, `"WaitReason"`, `"WaitingJob"`, `"Worker"`.
    Output is deterministic (BTreeMap, stable ordering).
13. **No panics**: all constructors/parsers return `Result`; regex literals inside use `// invariant:` comments.

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
| `types_md_examples_validate` | each JSON example fixture (`job.json`, `capsule.json`, `cost.json`, `limit.json`, `events.jsonl`, `profile.json`, `edge.json`, `run.json`, `session.json`, `inbox_item.json`, `loop.json`, `release.json`, `schedule.json`, `gate_result.json`, `worker.json`, `switch.json`) deserialises into its Rust type and validates against its exported schema (use a tiny structural checker: required properties present, no unknown top-level keys for `deny_unknown_fields` types) |
| `log_format_line` | `format_log_line` golden string; `MemoryLog` records in order |
| `service_cancel_token` | clones share cancellation; `ServiceError` Display strings |

## 7. Out of scope / v0.2

ACP event mapping, permission.request/resolve events, IANA timezone support, signed/hash-chained logs (field reserved: none in v0.1).
The v0.3 loopback API documents live in `src/api.rs`; see `docs/design/v0.3/http-api.md`.
