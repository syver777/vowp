# v0.6 — ACP streaming chunks and a permission flow (draft scope)

Status: **draft — for owner acceptance.** This page proposes a v0.6 cycle. It is not accepted,
not implemented, and promises no release, tag, version bump or date. The latest release remains
**0.4.0**; the six v0.5 slices are implemented in the tree and recorded in
[`../v0.5/scope.md`](../v0.5/scope.md). The proposed subject is the first open follow-up in
[`../../roadmap.md`](../../roadmap.md): **ACP streaming chunk events and a permission flow**.

## Why this cycle

The ACP adapter is complete for its first slice (status in [`../adapters.md`](../adapters.md)
§10), but two deliberately deferred behaviours are still missing. Both are visible in the current
tree and both have a direct cost for an operator and for a UI client.

**Chunks are accumulated, not published.** `AcpTurn::on_update` appends every
`agent_message_chunk` to an in-memory string and returns no events
(`crates/vowp-adapters/src/acp/mapping.rs`, the `agent_message_chunk` arm). `AcpTurn::finish`
then emits the single `assistant.message` for the accumulated text (none when the server sent no
text). The unit test
`message_chunks_accumulate_into_one_message` and the integration test
`acp_clean_turn_emits_events_in_order` pin that behaviour. There is no `assistant.chunk` event in
`EventType` (`crates/vowp-protocol/src/event.rs`), and `AdapterCapabilities` has no streaming
flag (`crates/vowp-protocol/src/adapter.rs`).

**Permissions end the turn.** `AcpTurn::on_permission` emits a `note` and an
`error{kind: Refused}` and records the refusal. The driver answers the JSON-RPC request with
`{"outcome":{"outcome":"cancelled"}}` and stops waiting
(`crates/vowp-adapters/src/acp/run.rs`, the `session/request_permission` branch). The tests
`permission_request_is_a_note_and_a_refusal` and `acp_permission_request_is_never_approved` pin
that answer and the absence of an `optionId`. This is safe — it never auto-approves — but there
is no typed permission event and no way for the owner to answer.

The consequences are concrete:

- A text-only ACP answer writes no session event until the turn ends. The home's stall judge uses
  the session row's `updated_at` (`step_stalls` in
  `crates/vowp-core/src/loops/supervisor.rs`; `StallJudge::judge` in
  `crates/vowp-graph/src/guards.rs`), so a healthy turn longer than the 20-minute
  `stall_minutes` bound can be judged stalled while the server is still streaming text. The
  operator sees a static board and then one large message; `vowp chat` prints only
  `assistant.message`/tool/error/note (`crates/vowp-cli/src/chat/mod.rs`, `print_new_events`), and
  the web session timeline reads the same log (`crates/vowp-daemon/src/app.rs`, `events`), so an
  event that is not written is not shown in either place.
- Any ACP tool that asks for permission is refused and the turn ends as `Error`. The owner
  cannot approve; the work is lost. A UI client has only `note`/`error` to render and no typed
  request to turn into a choice. The generic inbox already has an `approval` kind
  (`crates/vowp-protocol/src/inbox.rs`) and a working `vowp inbox answer` path
  (`crates/vowp-cli/src/cmd/inbox.rs`), but ACP never creates an item.

## Goal and non-goals

### Goal

1. **Streaming chunks are canonical events.** A new `assistant.chunk` event carries text as it
   arrives, ordered by the session log's `seq`. The final `assistant.message` remains the
   authoritative complete text. Chunks are non-replayable, excluded from capsules and bundle
   tails, and carry no cost.
2. **Permission requests are typed.** A new `permission.request` event carries the request id,
   tool-call id, title/kind and the server's options. A new `permission.resolved` event records
   the outcome, the selected option, the inbox item and who answered.
3. **The approval path uses the existing inbox and lease model.** For a supervised job turn
   (`TurnRequest.job_id` is `Some`) in a project-bound session, core opens one
   `InboxKind::Approval` item, waits for the existing `vowp inbox answer`/`/inbox` answer path,
   then sends the selected ACP option back to the adapter. The answer is a JSON-RPC result on the
   child's stdin; it is never a keystroke, a tmux `send-keys` call or a terminal scrape.
4. **No approval, no approval.** When no approval item can exist (an interactive or
   orchestrator turn with no `job_id`, or an adapter that does not advertise permission
   requests), core records `permission.resolved{outcome: unavailable}` plus a typed
   `error{kind: Refused}`, tells the adapter to cancel, and the turn ends. There is no
   auto-approval and no default option.
5. **Capabilities are honest.** A new `AdapterEventCapabilities` (returned by a defaulted
   `Adapter::event_capabilities`) advertises `emits_streaming_chunks` and `requests_permissions`.
   ACP reports both true; every other adapter keeps the empty default.
6. **Compatibility is explicit.** New event types are additive names in the log; the
   `EventEnvelope` shape and every existing payload stay unchanged. The global
   `session_events.type` CHECK and the global schema version move together in one slice, with a
   v7 → v8 migration. New event types are not added to bundle tails, so a v0.6 bundle remains
   readable by an older binary. A v0.6 *log* is not promised readable by a v0.5 binary because
   `EventType` is a closed enum (`EventType::parse` rejects unknown names, pinned by
   `event_type_strings_stable`).

### Non-goals

- A dedicated browser permission UI or new web controls. The generic `/inbox` approval row is
  the surface in this cycle; a dedicated permission UI is separate scope.
- Reading, storing or forwarding ACP authentication tokens. The server owns its own login.
- Native resume / `session/load`; `can_resume_native` remains false.
- General live pause or pause at arbitrary points. The wait is one pending permission request in
  one in-memory turn; a supervisor restart or lease takeover loses it (see risks).
- Changing transcript-mirror or cost accounting. Chunks carry no tokens; the final `cost` event
  and `RunSummary.cost` are unchanged.
- A real ACP server or network smoke test. The hermetic Python fake remains the harness.
- A release, tag, version bump or date.

## Candidate slices

Each slice is intended to be one job with disjoint `owns`. A later slice depends on an earlier
slice's types but never edits the earlier slice's files. Every slice also runs
`cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` before its commit; the targeted gates below are the ones that prove the
slice.

### Slice 1 — protocol types, event index and schema

This is the one repo-wide slice. Adding an `EventType` variant changes the
`session_events.type` CHECK in the embedded global schema
(`crates/vowp-store/src/schema/global.sql`) and the parity test
(`crates/vowp-store/tests/schema_enums.rs`). A protocol-only commit would leave
`cargo test --workspace` red, so the type addition and the index schema must land together. The
file list is broad, but the work is one mechanical version move plus the type definitions.

**Owns:**

```text
crates/vowp-protocol/src/event.rs
crates/vowp-protocol/src/adapter.rs
crates/vowp-protocol/tests/events.rs
crates/vowp-store/src/db.rs
crates/vowp-store/src/lib.rs
crates/vowp-store/src/migrate.rs
crates/vowp-store/src/schema/global.sql
crates/vowp-store/tests/agent.rs
crates/vowp-store/tests/migrate.rs
crates/vowp-store/tests/migration.rs
crates/vowp-store/tests/migration_v8.rs
crates/vowp-store/tests/metrics.rs
crates/vowp-store/tests/misc.rs
crates/vowp-store/tests/offload.rs
crates/vowp-store/tests/schema_enums.rs
crates/vowp-store/tests/split.rs
crates/vowp-cli/tests/up.rs
docs/design/types.md
docs/design/crates/vowp-store.md
docs/design/schema/global.sql
docs/reference/schema/event-envelope.md
```

**What changes:**

- Add three `EventType` names and their payloads in `crates/vowp-protocol/src/event.rs`:
  - `assistant.chunk` → `AssistantChunk { text: String }`.
  - `permission.request` → `PermissionRequest { request_id: String,
    tool_call_id: Option<String>, title: String, kind: Option<String>,
    options: Vec<PermissionOption> }`.
  - `permission.resolved` → `PermissionResolution { request_id: String,
    outcome: PermissionOutcome, option_id: Option<String>, inbox_id: Option<InboxId>,
    answered_by: Option<String> }`.
  - `PermissionOption { option_id: String, name: String, kind: String }`; the ACP `kind`
    remains a string so an unknown future kind is display-only rather than a parse failure.
  - `PermissionOutcome { Approved, Rejected, Cancelled, TimedOut, Unavailable }`, with the
    existing snake_case wire convention (`approved`, `rejected`, `cancelled`, `timed_out`,
    `unavailable`).
  - All optional fields are `#[serde(default)]`; required fields stay required. The payloads
    contain no raw tool input, so a command or path cannot smuggle a secret into the typed
    event. The adapter's display-only `raw` value follows the existing `redact`/cap rules
    (`crates/vowp-adapters/src/acp/mapping.rs`, `raw_value`).
- Update `EventBody::{event_type, to_value, from_parts, summary}` and
  `EventEnvelope::new` for the new types. `assistant.chunk`, `permission.request` and
  `permission.resolved` are non-replayable, like `assistant.reasoning`; the summary strings stay
  under the existing 200-character cap.
- Add `AdapterEventCapabilities { emits_streaming_chunks, requests_permissions }` and a defaulted
  `Adapter::event_capabilities()` in `crates/vowp-protocol/src/adapter.rs`. This is a separate
  type rather than two new fields on `AdapterCapabilities`: adding fields would break every
  full `AdapterCapabilities { … }` literal in the adapters and tests, while a defaulted method
  keeps the existing contract source-compatible.
- Add `AdapterRun::answer_permission(answer: PermissionAnswer) -> Result<(), AdapterError>` with
  a default `Err(AdapterError::Unsupported("permission_requests"))`. `PermissionAnswer` carries
  `outcome` and an optional `option_id`. `send()` keeps its existing `live_input` meaning and is
  not reused for permissions.
- Move the embedded global schema to version 8: update `crates/vowp-store/src/schema/global.sql`
  (`session_events.type` CHECK and `PRAGMA user_version = 8`), `GLOBAL_SCHEMA_VERSION = 8`,
  add `migrate_v07`/`migrate_v07_if_needed` in `crates/vowp-store/src/migrate.rs` and call it from
  `Store::open_global_unbound` in `crates/vowp-store/src/db.rs`. The migration backs up the
  global DB, rebuilds `session_events` with the new CHECK, copies every row and index, verifies
  row counts, integrity and foreign keys, and sets `user_version = 8`. `session_events` is a
  rebuildable index, but the migration must not drop rows; `vowp reindex` remains the recovery
  path.
- Do **not** edit `crates/vowp-store/src/schema/001_init.sql` or `docs/design/schema.sql`. Those
  freeze the v0.1 database and `crates/vowp-store/tests/migrate.rs` asserts they stay
  byte-identical. Retarget the protocol test's SQL parity check at the current embedded global
  schema (or leave the parity proof to `vowp-store/tests/schema_enums.rs`) instead of changing
  the frozen file.
- Update only the assertions that mean "the current global schema" to `GLOBAL_SCHEMA_VERSION`;
  keep the historical v7 targets in the v6 → v7 migration tests as literals. A new
  `migration_v8.rs` test builds a v7 home, runs `migrate_v07`, and asserts every old event row
  survives and a new event type is accepted by the CHECK.
- Regenerate `docs/reference/schema/event-envelope.md` from the binary. Update
  `docs/design/types.md` with the payloads and the compatibility rules, and
  `docs/design/crates/vowp-store.md` with the v8 migration.
- Compatibility rules to write down in `docs/design/types.md`:
  1. `EventEnvelope` and every existing payload keep their shape; new payload fields are
     `#[serde(default)]`; readers ignore unknown fields (pinned by
     `event_body_roundtrip_every_type`).
  2. `EventType` remains a closed enum; `EventType::parse` still rejects unknown names (pinned
     by `event_type_strings_stable`). `PROTOCOL_VERSION` stays 1 because the envelope did not
     change; mixed-version log reads are a separate opaque-event change if the owner wants them.
  3. `is_tail_event` in `crates/vowp-core/src/session/bundle.rs` does not gain the new types, so
     a v0.6 bundle tail stays readable by older binaries.
  4. The three new types are `replayable = false`. The capsule writer already filters
     `replayable` events (`crates/vowp-core/src/session/capsule.rs`), so capsule input does not
     change.
  5. The Rust enum and the index CHECK move together, and `vowp reindex` must index the new
     types.

**Gates:** `cargo test -p vowp-protocol --test events`, `cargo test -p vowp-store --test
schema_enums`, `cargo test -p vowp-store --test migration_v8`, `python3
docs/reference/check-reference.py`, `bash docs/tests/run.sh`.

**What must fail if the work is absent:** `event_body_roundtrip_every_type` (its sample count)
and `event_type_strings_stable` must fail until the names exist; `schema_matches_enums` must
fail until the CHECK list matches; `migration_v8` must fail if the migration loses a row or the
new type is rejected; `check-reference.py` fails if the regenerated `EventEnvelope` page is
missing or unparsable.

### Slice 2 — ACP adapter emission and the fake server

**Owns:**

```text
crates/vowp-adapters/src/acp/mod.rs
crates/vowp-adapters/src/acp/mapping.rs
crates/vowp-adapters/src/acp/run.rs
crates/vowp-adapters/tests/acp.rs
crates/vowp-adapters/tests/fakes/fake_acp.py
docs/design/adapters.md
docs/design/v0.2/acp-adapter.md
```

**What changes:**

- `AcpAdapter::event_capabilities` reports `emits_streaming_chunks: true` and
  `requests_permissions: true`.
- `on_update` for `agent_message_chunk` buffers text and emits `assistant.chunk` in bounded
  pieces. The proposed default is about 256 characters, flushed before a tool call, a plan, a
  permission request and at `finish`; there are no timers (adapters never sleep or time out on
  their own). Every chunk is also appended to the existing full-message accumulator, so the
  final `assistant.message` is unchanged and no text is lost.
- `on_permission` stops emitting `note`/`error` and emits one `permission.request` carrying the
  JSON-RPC request id (as a string), tool-call id, title/kind and options. The request's `raw`
  value is the existing redacted/capped display value. The adapter does not send an answer yet.
- `AcpRun` shares a locked writer for the child's stdin (the driver already owns it) so
  `answer_permission` can write the JSON-RPC result while the driver continues reading. It
  records one pending request at a time; a second request while one is unanswered is a
  `AdapterError::Protocol`. `answer_permission` validates that an approved/rejected `option_id`
  is one of the request's options; `Approved` with no id selects the single `allow_once` option
  when there is exactly one. `Cancelled`, `TimedOut` and `Unavailable` write
  `{"outcome":{"outcome":"cancelled"}}`.
- A cancelled/timed-out answer caused by VOWP ends the run as `Error`/`StopReason::Error` without
  an adapter-generated `Error` event; core owns the typed `permission.resolved` and `error`
  events. A `selected` answer lets the server continue; a later `stopReason: end_turn` is `Ok`,
  and a later server `cancelled` keeps the existing `Interrupted` mapping.
- `fake_acp.py` permission mode waits for the client's answer and then sends a prompt result that
  reflects it: `selected` `allow-once` → a short final text and `end_turn`; `selected`
  `reject-once` → a "rejected" final text and `end_turn`; `cancelled` → `cancelled`. It logs the
  exact response so the test can prove which option was selected. The existing `ok`, `empty`,
  `no-result`, `malformed`, `unknown-method`, `permission`, `quota` and `hang` modes stay.
- Tests:
  - `acp_clean_turn_emits_events_in_order` gains the expected `assistant.chunk` rows before the
    tool/plan events and still asserts one final `assistant.message`.
  - `acp_message_chunks_are_coalesced` sends many small chunks and asserts a bounded number of
    chunk events whose concatenated text equals the final message.
  - `acp_permission_request_emits_a_typed_event` asserts the event type/fields and that no
    `note`/`error` pair is emitted for the request itself.
  - `acp_permission_is_not_answered_until_the_caller_answers` starts the permission mode, waits
    for `permission.request`, and asserts the fake log has no `response` yet. It then calls
    `answer_permission(Approved/allow-once)` and asserts the log has
    `selected`/`allow-once`, the summary is `Ok`, and there is exactly one `turn.ended`.
  - `acp_permission_rejection_selects_the_reject_option` does the same with
    `Rejected/reject-once` and asserts no allow option appears in the log.
  - `acp_permission_unavailable_cancels_without_an_option` asserts the log has `cancelled`, no
    `optionId`, and the summary is `Error`.
  - The existing `acp_permission_request_is_never_approved` is replaced by the typed/no-answer
    tests; its replacement must fail if the adapter ever answers before the caller does.
- When this slice lands, update `docs/design/adapters.md` §10 and the v0.2 ACP note to record
  that chunks and the typed permission request are implemented, that the approval path is
  core-owned, and that a real ACP server, auth, native resume, a dedicated permission UI and live
  pause remain planned.

**Gates:** `cargo test -p vowp-adapters --lib 'acp::'`, `cargo test -p vowp-adapters --test acp`,
`cargo test -p vowp-core --test adapter_contract`.

**What must fail if the work is absent:** the chunk assertions in the clean-turn test have no
events to match; the no-answer test sees an immediate `cancelled` response; the approve/reject
tests see no `selected` option; the coalescing test sees either one event per delta or no chunk
events.

### Slice 3 — core permission routing and the approval wait

**Owns:**

```text
crates/vowp-core/src/session/mod.rs
crates/vowp-core/src/session/service.rs
crates/vowp-core/src/session/permission.rs
crates/vowp-core/tests/permission.rs
```

**What changes:**

- `run_turn`'s event loop gets a `permission.request` branch. It appends the event (with the
  normal redaction), then calls a new `session::permission` module to decide and wait. It does not
  break the loop; the adapter stays alive until the answer is sent.
- `permission::open_or_refuse` opens one `InboxItem` with `kind: Approval`, the session's
  project, the turn's `job_id`, a title such as `ACP permission: <tool title>`, a body that names
  the request id and lists every option's `optionId`, `name` and `kind`, and `options` set to the
  exact `optionId` strings. The body and options are passed through
  `crate::vault::redact_text`; no raw tool input is inserted. The job is running, so this path
  does not park it. It uses `store.insert_inbox`, the same row the existing `vowp inbox ask`
  command writes (`crates/vowp-cli/src/cmd/inbox.rs`); `InboxItem` already has no session/run
  column, so the pending mapping stays in the runner's memory.
- If the turn has no `job_id` or the session has no project, no approval path exists. The module
  appends `permission.resolved{outcome: unavailable}` and
  `error{kind: Refused, retryable: false}`, calls
  `adapter_run.answer_permission(PermissionAnswer { outcome: Cancelled, option_id: None })`, and
  lets the adapter finish as `Error`. The test asserts no inbox row was created. Interactive
  (user/orchestrator) turns are therefore refused in this cycle; an inline prompt for them is a
  later decision.
- If an approval item exists, the module polls `store.inbox_item(&id)` every existing
  `EVENT_POLL` (200 ms) until the item is no longer `Open` or the deadline expires. A `Dismissed`
  item is a rejection, never an approval. An answer is accepted only when it is an exact
  `optionId`; `approve`/`reject` are accepted only when the request has exactly one allow or
  reject option, so `--approve` can never choose between two allows. Anything else is a safe
  rejection and records a typed `error{kind: InvalidInput}`.
- It appends `permission.resolved` with the outcome, option id, inbox id and `answered_by`, then
  calls `adapter_run.answer_permission` with the matching answer:
  - `Approved` → `Approved` + the selected allow option.
  - `Rejected`/dismissed → `Rejected` + the selected reject option, or `Cancelled` when the
    request has no reject option.
  - `TimedOut` → `TimedOut`; `Unavailable` → `Cancelled`.
- On timeout (proposed default 300 seconds, held in the module or a small policy struct so tests
  can use a short value), it dismisses the inbox item as `acp-timeout`, appends
  `permission.resolved{outcome: timed_out}` and
  `error{kind: Timeout, retryable: true}`, sends `TimedOut`, and lets the adapter finish as
  `Error` (not `Killed`). On cancellation while waiting, it dismisses the item as `acp-cancelled`,
  appends `permission.resolved{outcome: cancelled}`, sends `Cancelled`, and the existing run
  cancellation path finishes `Killed`. If the cancel token fires (a supervisor stop or a
  takeover that cancels the run), it does the same best-effort answer and never resumes the old
  turn.
- The module reuses `TurnRequest.job_id` (`crates/vowp-protocol/src/services.rs`) and the
  existing store methods `insert_inbox`, `inbox_item`, `answer_inbox` and `dismiss_inbox`
  (`crates/vowp-store/src/repo_misc.rs`). No new store column, endpoint or lock is added.
- Tests:
  - `permission_approval_resumes_the_same_turn`: start an ACP fake in permission mode, run
    `run_turn` on a job-bound session in a thread, poll the store for the open item, answer it
    with `allow-once`, and assert the session log has `permission.request` →
    `permission.resolved{approved}` → final `assistant.message` → `cost` → one `turn.ended`
    (`Ok`/`EndTurn`), the item is `Answered`, and the fake log has `selected`/`allow-once`.
  - `permission_without_approval_path_is_a_typed_refusal`: a user session with no `job_id`;
    assert no inbox row, `permission.resolved{unavailable}`, `error{Refused}`, the fake log has
    `cancelled` and no `optionId`, and the turn ends `Error`.
  - `permission_rejection_never_selects_an_allow_option`: answer `reject-once`; assert the fake
    log has `selected`/`reject-once` and no allow id.
  - `permission_dismissal_is_a_rejection`: dismiss the item; assert the same safe rejection
    path.
  - `permission_timeout_is_recorded_and_cancels`: a short policy timeout; assert the item is
    `Dismissed`, the log has `timed_out` + `error{Timeout}`, the fake log has `cancelled`, and
    the turn ends `Error` (not `Killed`).
  - `permission_cancel_answers_the_adapter`: cancel while the item is open; assert the item is
    dismissed, the fake receives `cancelled`, and the run is `Killed`.
  - `permission_request_is_never_self_approved`: with an open item, assert the fake log has no
    `optionId` until the store answer is written.
- `session::permission` stays small and typed; no `unwrap`/`expect` in library code.

**Gates:** `cargo test -p vowp-core --test permission`, `cargo test -p vowp-core --test
session_run`, `cargo test -p vowp-core --test adapter_contract`.

**What must fail if the work is absent:** if core auto-approves or auto-cancels, the approval
test never reaches `end_turn` or the fake log names an option before the store answer; if the
inbox row is not created, the test's `inbox_item` lookup fails; if timeout/cancel is not
answered, the adapter test hangs; if the wrong answer is sent, the fake log has the wrong
option.

### Slice 4 — CLI/inbox surface and docs (conditional)

The minimal approval path needs no CLI code: `vowp inbox answer <id> <text>` already stores
arbitrary text and `vowp inbox show` prints the item's options
(`crates/vowp-cli/src/cmd/inbox.rs`, `answer` and `show`). The approval item's body therefore
names the exact option ids, and the owner can type one. `--approve`/`--reject` are accepted only
when the request has one unambiguous allow or reject option; core enforces that. If the owner
wants a first-class shorthand, this slice adds it:

**Owns:**

```text
crates/vowp-cli/src/cmd/inbox.rs
crates/vowp-cli/src/cli.rs
crates/vowp-cli/tests/inbox.rs
docs/design/cli.md
docs/reference/cli.md
```

**What changes:** add `vowp inbox answer <id> --option <option-id>` (or make `--approve` choose
the single allow option and reject an ambiguous request), teach `inbox show` to label ACP options
with their `optionId`, and document the exact-option answer in `docs/design/cli.md`. Regenerate
`docs/reference/cli.md`. Gates: `cargo test -p vowp-cli --test inbox`, `python3
docs/reference/check-reference.py`.

Do not add a dedicated browser permission page in this cycle; the existing `/inbox` write is the
surface. If the owner chooses a UI, it is a separate cycle and a separate design note.

## Tests and gates

The implementation must keep every existing adapter-contract and session test green, and add the
following proofs. A test is only accepted if it fails when the feature is absent.

| Behaviour | Test | What must fail if absent |
|---|---|---|
| Chunks are canonical and ordered | updated `acp_clean_turn_emits_events_in_order`; new `acp_message_chunks_are_coalesced` | no `assistant.chunk` rows, or the concatenated chunk text differs from the final message |
| Chunks do not change the final message or cost | `acp_clean_turn_emits_events_in_order`, `acp_message_chunks_are_coalesced`, the existing `crates/vowp-core/tests/ledger_record.rs` cost tests | final text lost, a second `assistant.message`, or a changed `cost` token total |
| Typed permission request | updated `acp_permission_request_emits_a_typed_event` | the adapter still emits only `note`/`error`, or the fields/options are missing |
| No auto-approval | `acp_permission_is_not_answered_until_the_caller_answers`, core `permission_request_is_never_self_approved` | the fake log has a response/`optionId` before the caller answers |
| Approval resumes the turn | core `permission_approval_resumes_the_same_turn` | no `permission.resolved{approved}`, no `end_turn`, item not `Answered` |
| Rejection is safe | `acp_permission_rejection_selects_the_reject_option`, core `permission_rejection_never_selects_an_allow_option` | an allow option is selected, or the turn auto-approves |
| No approval path | core `permission_without_approval_path_is_a_typed_refusal` | an inbox row is created, or the fake log has an option id |
| Timeout/cancel are recorded | core `permission_timeout_is_recorded_and_cancels`, `permission_cancel_answers_the_adapter` | the wait hangs, or the item is left `Open` with no `timed_out`/`cancelled` event |
| Schema and migration | `event_type_strings_stable`, `schema_matches_enums`, `event_body_roundtrip_every_type`, `migration_v8` | the new type is rejected by the CHECK, a migration row is lost, or a schema page is missing |
| Repo-wide | `cargo test --workspace` | any crate, adapter, store, daemon or CLI suite regresses |

The final implementation also runs, from a clean worktree:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash docs/tests/run.sh
python3 docs/reference/check-reference.py
```

`bash docs/tests/run.sh` and `python3 docs/reference/check-reference.py` are also the two
documentation gates for this draft; they must pass before the scope itself is accepted.

## Risks and open questions

1. **Does the owner want a permission approval UI now?** The generic inbox row is a working but
   terminal/page-based surface, and in this proposal it is available only for supervised job
   turns; an interactive `vowp chat` turn is refused. A dedicated web button or an inline
   `vowp chat` prompt is a new UI with its own state, error and lease rules; it is not in this
   scope. The recommended first cycle is the generic inbox for job turns only.
2. **How is a timed-out approval recorded?** Proposed: 300 seconds, then the inbox row is
   `Dismissed` as `acp-timeout`, the session log has
   `permission.resolved{outcome: timed_out}` followed by `error{kind: Timeout, retryable: true}`,
   the adapter receives `cancelled`, and the run ends `Error` (not `Killed`). The run row keeps
   the adapter's existing `detail` fallback; the events are the authoritative record. A
   different duration or a config key is an owner decision.
3. **Do streaming chunks change the transcript mirror or cost accounting?** No cost change:
   chunks carry no tokens, and the final `cost` event still comes from the `session/prompt`
   usage (`crates/vowp-adapters/src/acp/mapping.rs`, `finish`). The app transcript mirror
   (`crates/vowp-core/src/surface/mirror.rs`) mirrors opencode/Claude app transcripts, not ACP
   adapter streams, so it is unaffected. The web session timeline reads the log and will see
   chunk rows; it must treat them as provisional and replace/coalesce them with the final
   `assistant.message`, or text will duplicate. That UI work is not in this cycle. The log and
   `session_events` index grow with chunk events; coalescing, non-replayability, compaction and
   `cargo test --workspace` are the guard rails.
4. **Is ACP revision 1's chunk shape stable enough to pin?** The current adapter consumes only
   `sessionUpdate: "agent_message_chunk"` with a text content block
   (`crates/vowp-adapters/src/acp/mapping.rs`); the fake covers text only. This scope pins that
   minimum and ignores non-text content. If revision 1's field names or chunk granularity are
   not stable, the adapter may need a negotiated version or a per-server compatibility shim;
   that is a design question for the owner before the adapter slice is accepted.
5. **Schema bump cost.** Moving the global schema to v8 touches the version assertions in store
   and CLI tests and needs a v7 → v8 migration. `session_events` is an index and can be rebuilt
   with `vowp reindex`, but the migration must still copy every row. A botched migration is a
   home-wide outage until a backup is restored; the new migration test must use a real v7
   fixture, not a fresh v8 database.
6. **Adapter hang if core does not answer.** The ACP server waits on stdin; a core bug that
   ignores `permission.request` would hang the turn. Core must answer on approval, refusal,
   timeout, cancellation and lease takeover. The core tests above are the guard; the adapter's
   own tests are not enough.
7. **Restart, takeover and orphan items.** `InboxItem` has no session/run column
   (`crates/vowp-protocol/src/inbox.rs`), so the pending request is an in-memory mapping in the
   runner. A supervisor restart or a lease takeover loses the child; the item can remain open.
   The first cycle accepts an orphaned item and records the loss in the session log if possible.
   Persisting pending permissions across a restart is a larger design (a new store column and a
   resume path) and is not proposed here.
8. **Security of the approval text.** Tool titles and option names can contain commands, paths
   and (in the worst case) a pasted secret. The adapter redacts `raw` values; core must pass the
   typed event and the inbox body/options through `vault::redact_text` before writing. The
   approval item never contains raw tool input or secret values. Only `allow_once`/`reject_once`
   are selectable in the first cycle; `allow_always` is display-only. This is an owner decision
   below.
9. **Stall detection both ways.** Indexed chunks keep a healthy text-only turn from looking
   stalled, which is the goal. A server that streams text forever would also look healthy; the
   existing 20-minute bound is measured from real text events, not keepalives, and a stuck
   server sends none. The chunk policy must not emit empty keepalive events.

## Explicit owner decisions

These are choices for the owner; nothing here is decided silently.

1. **Event types and the schema bump.** Recommend: add `assistant.chunk`, `permission.request` and
   `permission.resolved`, and accept global schema v8 with a v7 → v8 migration. Alternative if
   the migration cost is too high: reuse `note`/`assistant.message` with optional typed payload
   fields, keep schema v7, and accept that events are filtered by payload rather than type. The
   recommended path is cleaner for UI clients and `reindex`.
2. **Approval surface.** Recommend: the existing generic inbox for supervised job turns
   (`vowp inbox answer` and the existing `/inbox` form), with exact ACP option ids in the item
   body and options; interactive turns are refused in this cycle. Alternatives: an inline
   `vowp chat` prompt for interactive turns, or a dedicated web permission UI (larger, separate
   cycle).
3. **Timeout.** Recommend: 300 seconds; dismiss the item as `acp-timeout`; record `timed_out` +
   `error{Timeout}`; send `cancelled`; end the run `Error`. Alternatives: 900 seconds, or no
   timeout (the turn waits indefinitely, which the stall rules cannot rescue).
4. **Chunk policy.** Recommend: about 256 characters per `assistant.chunk`, flush before a
   tool/plan/permission event and at `finish`; no timers. Alternatives: one event per ACP delta
   (simple but can flood the log and index), or only the final `assistant.message` (no streaming).
5. **Option safety.** Recommend: only `allow_once` and `reject_once` are selectable; show
   `allow_always`/`reject_always` but never select them automatically, and do not add a
   session-persistent "always" in this cycle. Alternatives: allow `allow_always` for the run or
   for the session (a larger trust decision).

## Status of this page

This is a **draft**. The owner must answer the five decisions above before the scope can be
accepted. Acceptance would turn each slice into one implementation job with the listed `owns`
and gates; this page does not create those jobs, does not claim any behaviour is implemented and
does not promise a release, tag, version or date.
