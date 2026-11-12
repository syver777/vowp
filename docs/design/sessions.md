# sessions.md — universal sessions: log, capsule, switch, surfaces, export/import

Status: current for v0.2. File- and protocol-level rules. Consumed by `vowp-store` (files,
`SessionLog`, reindex: `crates/vowp-store.md`), `vowp-core::session` (capsule writer, switch,
remote execution, export/import, `SessionOps` impl), `vowp-core::surface` (leases, takeover, app
hosts, mirror), `vowp-cli` (`session`, `chat`, `mcp`: `cli.md`). Types are in `types.md` (§5 events,
§6 capsule, §9 sessions) and §16 (v0.2 surface types). The normative v0.2 behaviour is
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) §7.

The v0.1 model is unchanged: one append-only log with one `seq`, capsules, the switch protocol and
bundles. v0.2 adds a transport (remote execution), a lease (one writing surface) and app-host
integration. It does not add a second session type or a second transcript store.

## 1. Canonical event log

1. **Location & layout** (all under the node root, `NodePaths`):

```
~/.vowp/
  vowp.db                          global index + surfaces/leases + capsules (schema/global.sql)
  projects/<name>/project.db       project rows, including that project's runs (schema/project.sql)
  sessions/<sessionId>/
    log.jsonl                      AUTHORITATIVE content, append-only, one EventEnvelope per line
    .lock                          single-writer lock (pid)
    blobs/<sha256>                 spilled tool output, diffs, attachments (content addressed)
    capsules/<at_seq:010>-<ulid>.json   authoritative capsule files
    adapters.json                  {"natives":[{harness,native_session_id,node,first_seq,created_at}]} append-only
    manifest.json                  optional; written by `session pull --full`, informational only
  orchestrators/<name>/            orchestrator.json (0600), app config, lease token, mirror.json
  runs/<runId>.jsonl               raw harness stdout mirror on the home (debug only, compressed/rotated)
  logs/supervisor.log  backups/  tmp/
project/.vowp/jobs/<job>/{brief.md,report.md}
```

A remote executor writes its raw harness mirror in its own worktree on the executor machine and
redacts it there; the home keeps the canonical session log. The home is the only writer of
`log.jsonl`; `vowp session ingest` is the one additional append path and it runs on the home.

2. **Append rules.** Only `SessionLog::append` on the home writes `log.jsonl`; the session service
   and `vowp session ingest` are its two callers. `seq` starts at 1, increments by exactly 1; a gap or
   duplicate is rejected (`StoreError::SeqGap`). Ingest is idempotent by event id: a duplicate id is
   acknowledged without a second append; the same id with different content is a conflict. Events are
   immutable; corrections are new events. `ts` = harness-reported time when the adapter provides one,
   else the node clock.
3. **fsync.** The log is flushed per append and fsynced by `sync()` which the session layer calls at: `turn.ended`, `session.executor` (detach), `switch`, `capsule`, `session.created`, and fatal `error`. A crash therefore
   loses at most the in-flight turn's tail; a torn last line is truncated on the next open (vowp-store.md §4.1 rule 5).
4. **Blobs.** `tool.result.output` strictly larger than **8192 bytes** is written to `blobs/<sha256>` and replaced by `output_ref{sha256,bytes,preview(512 chars)}`. `file.change.diff_ref` and attachments always use blobs. Blob content is raw bytes, sha256 lowercase hex.
5. **raw & replayable.** `raw` keeps the harness's original JSON for display (adapters cap it at 16 KiB and replace larger values by a `{"truncated":true,…}` marker, adapters.md §7; the store drops any `raw` still > 64 KiB as a safety net). `replayable=false` for `assistant.reasoning`; reasoning is never fed into a capsule prompt or a seed.
6. **Session ids** are ULIDs created on this node. Harness-side ids live in `session.executor` events (`native_session_id`), `adapters.json` and `session_natives`, never overwritten: a new harness run adds a new mapping.
7. **The DB is an index.** `sessions`, `session_events`, `session_natives` (and cached `capsules`) are rebuilt by `vowp reindex` / `reindex()` (algorithm: vowp-store.md §4.3). Deleting `vowp.db` never loses session content. A session directory copied to another node opens there (`vowp reindex` on that node registers it, `node` of the session row becomes the local node name).
8. **Event ordering in a turn** (adapters + session layer produce this order): `turn.started` → `user.message` (the prompt) → {`assistant.reasoning`, `tool.call`, `tool.result`, `file.change`, `plan`, `assistant.message`}* → `cost` → [`limit` | `error`] → `turn.ended`. Order of the framing events (owned by core, never by adapters): `session.executor{attach}` → `turn.started` → `user.message` → …; `session.executor{detach}` follows the run's `turn.ended`.

## 2. Session states

`idle` (no run) · `running` (a run is attached) · `blocked` (last run ended in a `limit`/`auth`/`credits` outcome; session readable; awaiting a switch or reset) · `switching` (switch protocol in progress; input rejected) · `closed` (job finished; no further turns).

| From | Event | To |
|---|---|---|
| idle | `run_turn` starts | running |
| running | turn ended ok / interrupted | idle |
| running | turn ended with limit/credits/auth | blocked |
| idle, running, blocked | `switch` begins | switching |
| switching | switch completed | idle (new executor) |
| switching | switch aborted | previous state (`blocked` if it was blocked, else `idle`) |
| any | `session close` / job done | closed |

`sessions.state` and `sessions.executor` are updated in the same DB transaction as the event index row (`Store::index_event` + `set_session_state`).

## 3. Capsule writer

Implemented in `vowp-core::session::capsule`. Signature: `CapsuleWriter::write(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, CoreError>`.

**When:** explicit `switch`, `limit` event, context pressure, `vowp session capsule <id>` (manual), job end (`job_end`), export. Debounce: at most one capsule per session per **120 s**
unless the trigger is `Switch`, `Manual`, `Export` or `Limit` (those never debounce; a debounced call returns the latest existing capsule).

**Executor:** `[capsule].executor` or, by default, the last rung of the `keeper` profile (the cheapest capable executor; no fixed model). The capsule run is a
`run` row with `role=capsule`, `purpose=capsule`, in its **own throwaway session** (never the session being summarised), so it does not pollute the log.

**Input compaction** (all from `log.jsonl`, `replayable` events only):
1. `base` = latest capsule for the session (if any) rendered as JSON; `since` = events with `seq > base.at_seq` (all events if no capsule).
2. Render each event as one text block: `[seq 143 · assistant] text` / `[seq 144 · tool.call] name {input truncated to 300 chars}` / `[seq 145 · tool.result ok] first 400 chars (… N bytes more, blob <sha8>)` / `file.change path op` / `plan` items / `limit`, `error`, `switch`, `note` verbatim (≤ 300 chars each). `assistant.reasoning` and `cost` are omitted.
3. If the concatenated rendering exceeds `max_input_chars` (default 200 000): drop `tool.result` bodies first (keep the one-line header), then middle-truncate `assistant.message` to 1200 chars, then drop the oldest events until it fits, always keeping the first `user.message` and the last 20 events. Insert a line `[… N earlier events omitted …]` where events were dropped.
4. Constants filled by the writer: `session_id`, `capsule_id`, `at_seq` (= last event seq included), `created_at`, `from`, `trigger`, `version=1`, `telemetry` (turns = count of `turn.ended`, tokens/usd = sum of `cost` events). The model only produces a `CapsuleDraft`.

**Prompt** (the exact template; `{…}` placeholders are filled; the JSON Schema is `schema_for_name("CapsuleDraft")` pretty-printed):

```text
You are the handoff writer for a software-engineering session. Another AI agent, possibly on a different harness and model, will continue this work
from your handoff alone. Write a precise, factual capsule. Do not invent facts that are not in the material. Prefer concrete file paths, commands and decisions.

SESSION: {session_title}  (job: {job_id_or_none}, working directory: {cwd})
EXECUTOR THAT WAS RUNNING: {executor}   REASON FOR HANDOFF: {trigger}

PREVIOUS CAPSULE (covers events up to seq {base_at_seq}; update it, do not discard still-valid facts):
{previous_capsule_json_or "none"}

NEW MATERIAL (events seq {first_seq}..{last_seq}):
{rendered_events}

Return ONLY one JSON object, no markdown fence, no commentary, that validates against this JSON Schema:
{capsule_draft_json_schema}

Rules:
- "intent": one paragraph, what the user is trying to achieve (never empty).
- "state.done/in_progress/blocked/next_steps": short imperative or past-tense lines; next_steps ordered by priority.
- "files": only files actually read/created/modified/deleted, with a role and a short note.
- "decisions": what was decided and why; "gotchas": traps the next agent must know; "open_questions": unresolved items.
- Never include secrets, tokens, or key values; refer to them by name only.
```

**Output handling:**
1. Run the capsule executor via `SessionOps::run_turn`; take the final `assistant.message` text; strip an enclosing ```` ```json ```` fence if present; parse as `CapsuleDraft`; assemble the full `Capsule`; `Capsule::validate()`.
2. On parse/validation failure: **one repair attempt** in the same capsule session: prompt = `Your previous answer was invalid: {error}. Return ONLY the corrected JSON object.` plus the original schema.
3. On second failure: keep the previous capsule untouched, append an `error` event (`kind: Parse`, message `capsule invalid after repair: …`) to the **summarised** session, return `Err(CoreError::CapsuleInvalid)`.
4. On success: `write_capsule_file` (atomic) → `Store::insert_capsule` → append `capsule` event (`CapsuleEvent`) to the session and `sync()` → ledger row (`kind=state`, summary "capsule written", cost of the capsule run recorded as its own `cost` ledger row).
5. Limit/credits on the capsule executor itself: try the next rung of the keeper profile; if none works, treat as failure per step 3.

## 4. Seeding a target harness

Used by `switch` and by `run_turn` when a session has no native session on the target harness. Never replays native tool calls; handoff is plain text (`can_inject_assistant_history=false` in v0.1).
`seed_prompt(capsule, tail, tail_turns, next_input) -> String`, where `tail` is the session events of the last complete turns (the caller selects them) and `tail_turns` bounds how many complete turns are rendered =

```text
[VOWP handoff] You are continuing session {session_id}, started on {from_executor}. Prior work summary follows.
Treat it as background; do not restate it. Continue the work and respond to the user's next message below.

{capsule.render_markdown()}

## Last turns (plain text)
{user: …\nassistant: …   for the last N=2 complete turns; tool activity summarised as "(ran: <tool name> …)"; each turn ≤ 3000 chars}

## Next message
{next_input}
```

The seed is appended to the log as a `user.message` with `note: "handoff"` (display marks it) so the log shows exactly what the new executor saw. For a job continue after a switch, `next_input` is the standard continue message (loops.md L2).

## 5. Switch protocol

`SessionOps::switch(SwitchRequest{session_id, to, reason, drain_secs=5})`. Triggers: user (`vowp session switch`), `limit` (L6), `crash` (only if configured), `context`, `failover`, `probe_back`, and `takeover` when another surface takes the lease and the session already has an executor (§9).
Each step lists the log events and DB rows it changes. `S` = the source executor, `T` = the target.

| # | Step | Log events | DB rows |
|---|---|---|---|
| 1 | **Freeze**: refuse if state is already `switching` (`Conflict`); set state `switching`; reject new input | — | `sessions.state='switching'` |
| 2 | **Plan record**: insert `switches` row `state='started'` with `from_executor=S`, `to_executor=T`, nodes, `reason`, `at_seq = last_seq` | — | `switches` INSERT |
| 3 | **Probe target**: `adapter(T).probe()` (and for a remote node, `vowp --node <n> doctor --json`). If not ok → abort (row 9) — session stays on S | — | — |
| 4 | **Drain**: if a run is active, wait ≤ `drain_secs` for `turn.ended`; then `interrupt()` and wait 3 s; then `stop()` | `turn.ended{stop_reason: interrupted}` if the adapter did not emit one | `runs` finished (`outcome=killed`) |
| 5 | **Capture capsule**: `write(session, trigger=Switch)` (§3; never debounced). If it fails after repair: if a previous capsule exists use it (and add `note` "stale capsule"), else abort (row 9) | `capsule` | `capsules` INSERT, `sessions.last_capsule_id` |
| 6 | **Detach S**: stop adapter (idempotent) | `session.executor{detach, reason}` → `sync()` | `sessions.executor=NULL` |
| 7 | **Start T**: build seed (§4) and call `run_turn` setup or, for a pure switch without input, start T **idle**: record the attach only (the seed is delivered with the next input) | `session.executor{attach, native_session_id?}` , `user.message{note:"handoff"}` if a turn is started | `sessions.executor=T`, `session_natives` upsert, `runs` INSERT if a turn started |
| 8 | **Record**: append `switch{switch_id, from, to, capsule_id, reason}`; `sync()`; `finish_switch(Completed)` | `switch` | `switches.state='completed'`, `ended_at` |
| 9 | **Unfreeze**: state `idle` (or `running` if a turn was started on T) | — | `sessions.state` |
| ✗ | **Abort** (any failure before step 6 completes): `finish_switch(Aborted, error)`; leave S attached; state returns to the pre-switch state (`blocked` stays `blocked`) | `error{kind, message}` | `switches.state='aborted'` |

Failure handling:

| Failure | Behaviour |
|---|---|
| Target probe fails | Abort at step 3, session unchanged and readable, `error` event, inbox notice if the reason was `limit`/`failover` and no other rung exists |
| Seed rejected / T crashes during start (step 7) | Retry **once** with capsule only (no tail); then abort *after* re-attaching S if S is usable, else leave session `blocked` with a `switch` row `aborted` |
| Capsule invalid twice, no previous capsule | Abort at step 5 |
| Supervisor crash mid-switch | On startup `open_switches()` is scanned. For each `started` switch: if the log contains a `switch` event with its `switch_id` → `finish_switch(Completed)`; else if the log's last `session.executor` is an attach of `T` → append the missing `switch` event, `Completed`; else → `Aborted` ("interrupted by restart") and `sessions.state` recomputed (`blocked` if the last turn ended with a limit, else `idle`). Never both |
| Two switches requested together | The second sees `switching` → `Conflict`; callers retry on the next tick |

The log is never rewritten; a switch only appends events. Lineage = `switches` rows + `switch` events + `parent_session_id` across nodes.

The daemon's `POST /api/v1/sessions/{id}/switch` runs this same protocol through
`SessionService::switch_with_store` and `switch::run_switch_with_store`, over a store the
daemon opened with `Store::open_writable` (never `Store::open`). The HTTP handler applies the
CLI's `switch_to` guards before the service call: an unknown session is `404`, a running
session without `force` is `409`, a `switching` or `closed` session is `409`, and an active
limit on the target provider is `409`; an invalid body or executor is `400`. The switch is
local-node only, uses `drain_secs: 5`, and returns the completed `Switch`; a failed switch is
an error and leaves the previous executor attached under the abort rules above.

## 6. Export / import (cross-node, v0.1)

`vowp session export <id> [--tail N] [--out file | -o file]` writes a **single JSON file** (no archive dependency): `vowp-bundle-v1`.
`vowp session import <file> [--node …]` creates a **child session** on the importing node.

### 6.1 Format

```json
{
  "format": "vowp-bundle-v1",
  "protocol_version": 1,
  "exported_at": "2026-11-08T19:42:00.000Z",
  "source_node": "local",
  "session": { "id": "01KA1V0Z7A0000000000000000", "title": "planner", "role": "planner", "cwd": "/home/user/proj", "project": "vowp", "executor": "claude-code:model-a@medium", "state": "blocked", "last_seq": 142, "parent_session_id": null, "job_id": null,
               "node": "local", "last_capsule_id": "cap_01KA1V2B000000000000000000", "log_bytes": 51234, "created_at": "2026-11-08T18:00:00.000Z", "updated_at": "2026-11-08T19:41:09.000Z" },
  "capsule": { "version": 1, "capsule_id": "cap_01KA1V3C000000000000000000", "session_id": "01KA1V0Z7A0000000000000000", "created_at": "2026-11-08T19:42:00.000Z", "at_seq": 142, "trigger": "export",
               "from": {"executor": "claude-code:model-a@medium", "node": "local"}, "intent": "…", "state": {"done": [], "in_progress": [], "blocked": [], "next_steps": []},
               "telemetry": {"turns": 12, "tokens_in": 84000, "tokens_out": 9100, "usd": null} },
  "tail": [ { "id": "01KA…", "session_id": "01KA1V0Z7A0000000000000000", "seq": 141, "ts": "…", "type": "user.message", "actor": "user", "source": {"harness": "claude-code"}, "payload": {"text": "…"}, "replayable": true } ],
  "lineage": [ { "id": "01KA…", "session_id": "01KA1V0Z7A0000000000000000", "from_executor": null, "to_executor": "claude-code:model-a@medium", "from_node": null, "to_node": "local", "capsule_id": null, "reason": "user", "state": "completed", "error": null, "started_at": "…", "ended_at": "…", "at_seq": 1 } ],
  "blobs_inline": { "<sha256>": "<base64 bytes>" },
  "checksum": "sha256:<64 hex>"
}
```

Rust (in `vowp-core::session::bundle`): `pub struct Bundle { format: String, protocol_version: u32, exported_at: Timestamp, source_node: NodeName, session: Session, capsule: Capsule, tail: Vec<EventEnvelope>, lineage: Vec<Switch>, blobs_inline: BTreeMap<String, String>, checksum: String }`.

### 6.2 Export algorithm

1. Read the session (index) and log (`SessionLogReader`). `tail` = last N events (default **N = 20**, max 500) whose `replayable` is true or which are `user.message`/`assistant.message`/`tool.call`/`tool.result`/`switch`/`capsule`/`limit` — all events in the window are included in order; `assistant.reasoning` events are dropped.
2. Produce a **fresh** capsule (`trigger=Export`, never debounced) so the bundle is self-contained.
3. `blobs_inline`: for every `BlobRef` referenced by the `tail` events whose `bytes <= 65 536`, include base64(bytes) keyed by sha256; larger blobs are **not** included (the `output_ref` stays, marked unavailable on import; its preview survives).
4. `lineage` = all `switches` rows of the session.
5. `checksum` = `"sha256:" + hex(sha256(canonical_json(bundle without "checksum")))` where canonical JSON = `serde_json::to_vec` of a `serde_json::Value` (object keys sorted — `serde_json` `Map` is a BTreeMap unless `preserve_order` is enabled; the crate must not enable it) with no whitespace.
6. Write atomically (tmp + rename); print the path (or write to stdout when `-o -`).

### 6.3 Import algorithm

1. Parse; reject `format != "vowp-bundle-v1"` (`unknown bundle format`), `protocol_version > PROTOCOL_VERSION` (`bundle too new`), checksum mismatch (`checksum mismatch`). No state is changed on rejection.
2. Create a new session `S'` (fresh ULID) on this node: `SessionLog::create`; `parent_session_id = bundle.session.id`; `title = "<title> (imported)"`; `cwd` = `--cwd` (`vowp session import --cwd`) or the bundle's cwd if allowed by `[node].cwd_allow`, else the project path.
3. Append events, in order, each `sync()`ed at the end: `session.created{parent_session_id: <origin>}` → `note{text: "imported from <source_node> session <id> at seq <last_seq>"}` → `switch{from: bundle executor @ source_node, to: none-yet attach target, capsule_id, reason: user}` — the `to` is the importing node with the bundle's executor (so lineage is visible) → `capsule{...}` after writing the bundle capsule into `capsules/` with a **new** `capsule_id` (original kept in `note`) and `session_id = S'`. Tail events are **not** copied into `S'`'s log (they are history of the origin session); they are rendered into the seed as "last turns".
4. Insert index rows (`reindex(Some(S'))`), set `last_capsule_id`; write blobs from `blobs_inline` into `S'/blobs` (verifying each digest) so previews can be opened.
5. The first `run_turn` on `S'` uses `seed_prompt(capsule, tail_from_bundle, tail_turns, next_input)` (§4).
6. Importing the same bundle twice creates two independent children (no dedupe); the origin session is never modified.

### 6.4 Pull from another node (v0.3 first slice)

`vowp session pull <node> <session>` runs on the local machine. It looks up `node` in the local
registry and runs `vowp session export <session> --out -` on that node with the same `--project`
argument as the other remote session commands. The stdout bundle is staged in a private temporary
directory, parsed and verified by `bundle::from_json` (format, protocol version and sha256
checksum), and then imported through the same path as `session import`: a fresh child whose
`parent_session_id` is the source id. The command prints the source and child ids; `--json` prints
`{"node", "source_session", "child_session"}`. The staged file and its directory are removed on
every path.

A failed remote export returns the remote exit code and output. A malformed, truncated or
checksum-mismatched bundle returns a local invalid-input error. In both cases nothing is imported.
The source must be a remote node; a local node is refused with a hint to use export/import. This
slice deliberately has no same-id import (see §6.5) or directory copy (see §6.6); push,
bidirectional sync/merge, lease interaction and daemon/offline transport remain out of scope.

### 6.5 Same-id import (v0.3 second slice)

`vowp session import <file> --same-id` and `vowp session pull <node> <session> --same-id` keep the
origin session id instead of creating a child. The default import and pull paths are unchanged.

1. Verify the bundle exactly as in §6.3 step 1 before looking at local state. A checksum, format,
   protocol-version or event-body failure creates nothing.
2. Refuse with exit 4 when the origin id already exists locally as a `sessions` row or a
   `sessions/<id>/` directory. The existing row, log and index are not touched. Never overwrite and
   never merge.
3. Check every event id in the bundle tail: an id repeated inside the bundle or already present in
   `session_events` is a conflict. This check runs before the log is created, so a duplicate id
   leaves no session directory.
4. Create the session with the origin id, no parent, the bundle's title, role, project/job (when
   those rows exist locally) and the same cwd rule as §6.3 step 2. The log is created with
   `create_new`, so two racing imports cannot both succeed.
5. Append a fresh `session.created` (parent `NULL`) followed by every other replayable event in the
   bundle tail. Event ids and timestamps are preserved; `seq` is reassigned contiguously from 1
   because the bundle carries only a tail, not the full origin log. A bundle that includes its own
   `session.created` contributes its metadata, not a second created event.
6. Write the bundle capsule with its original capsule id under the origin session id, set its
   `at_seq` to the last copied tail seq, append the `capsule` event, write inline blobs with digest
   verification and run `reindex(Some(id))` in one transaction. A `reindex` collision (duplicate
   event id) is a conflict, never a warning.
7. Do not write the child `import.json` sidecar: the tail events are already in the log, and the
   first turn uses the normal capsule + complete-turn seed. A compacted bundle keeps its capsule,
   empty tail and `compacted_at`.
8. The output keeps the default shape in child mode. Same-id human output is
   `pulled session <source> from <node> with same id <id>`; JSON is
   `{"node","source_session","same_id_session"}`.

Out of scope for this slice: directory copy of `sessions/<id>/` (see §6.6), divergence reporting
(two heads, see §6.7), merge, bidirectional sync, `session push`, lease interaction and an offline
retry queue.

### 6.6 Full directory pull (v0.3 third slice)

`vowp session pull <node> <session> --full` copies the raw `sessions/<id>/` directory instead of a
bundle. It always keeps the origin id and has no `--same-id` combination; the default bundle pull
and its child session are unchanged.

1. The local side runs the source's hidden `session dir-export <session>` over the existing SSH
   argv path. Its stdout is one JSON manifest line followed by the file bytes in manifest order.
   The command is read-only: it takes no session lock, writes nothing and moves no lease.
2. The manifest is `vowp-session-dir-v1`:
   `{"format":"vowp-session-dir-v1","origin":"<id>","files":[{"path":"log.jsonl","bytes":N,
   "sha256":"<64 lowercase hex>"}]}`. Paths are relative, `/`-separated and limited to `log.jsonl`,
   `adapters.json`, `blobs/<sha256>` and `capsules/<name>.json`; traversal, duplicates,
   `.lock`, `import.json` and `manifest.json` are refused.
3. The cap is 1 GiB (`SESSION_DIR_MAX_BYTES` = 1 073 741 824 bytes). The source checks the total
   from file sizes before hashing and the local side checks it again from the manifest before
   staging; an over-cap pull fails with the limit named and copies nothing. A missing or oversized
   blob is never replaced by a placeholder in this slice: the pull refuses instead.
4. The origin id must be absent locally as a `sessions` row and as `sessions/<id>/`. A present id
   is exit 4 and the existing row, log, index and directory are untouched. There is no child, no
   parent and no bundle rewrite.
5. Files are written into a private staging directory under `<home>/tmp` containing
   `sessions/<id>/`. Every size and sha256 is checked, the staged log must verify cleanly, and
   `adapters.json` and every capsule file must parse and belong to the id. A failure removes the
   staging directory and leaves no final `sessions/<id>/`.
6. Only after verification is the manifest written into the staged directory as `manifest.json`
   and the directory renamed to `sessions/<id>/`. The pull then runs `reindex(Some(id))`. A reindex
   failure removes the new directory and fails; expected warnings (for example an absent local
   project or job row) leave the session indexed with a null foreign key and are printed on stderr.
7. A compacted source has no `log.jsonl` and is refused with a hint to use the bundle pull, which
   carries its capsule. A torn or corrupt source log is also refused rather than copied.
8. The source lease must be released or quiescent: this is a one-way snapshot, not a lease move,
   and no live lease is migrated or cut off. Both nodes must run a build that has the hidden
   export command.
9. Human output is `pulled session <id> from <node> as a full directory snapshot (<n> files,
   <bytes> bytes)`; JSON is
   `{"node","source_session","full_session","files","bytes"}`.

Named tests: `session_pull_full_imports_the_origin_id_with_the_full_log`,
`session_pull_full_reports_divergence_without_mutating_it`,
`session_pull_full_refuses_a_corrupt_snapshot_without_leaving_a_directory`,
`session_pull_full_refuses_an_over_cap_snapshot_before_copying`,
`session_pull_full_reports_a_failed_remote_export_and_imports_nothing` and the store tests
`session_dir_manifest_verifies_and_detects_corruption`,
`session_dir_manifest_refuses_a_directory_over_the_size_cap` and
`session_dir_manifest_rejects_traversal_and_reserved_paths`.

Still out of scope: divergence reporting (two heads, see §6.7), merge, bidirectional sync,
`session push`, live lease migration, a daemon transport, offline queueing, a progress display,
blob deduplication and a CLI size-limit flag.

### 6.7 Divergence report (v0.3 fourth slice)

`vowp session pull --same-id` and `vowp session pull --full` report a divergence when the origin
id is present locally instead of overwriting, silently refusing or merging. The default child pull
and the empty-local same-id/full success paths are unchanged.

1. The check runs after the bundle or directory manifest has been verified but before any local
   session file, row or index entry is written. The full path aborts the snapshot stream at that
   point; it does not stage, copy, rename or reindex anything.
2. The report names both heads. The local head is read from the local index row and `log.jsonl`:
   last seq, last event type and timestamp, state and executor when the row exists. A compacted
   local session reports the row's `last_seq` with no last event. The remote head comes from the
   bundle tail (seq, event type, timestamp) or, for a full pull, from the source's read-only
   `session show --json --events 1` because a `vowp-session-dir-v1` manifest carries no log
   position. `vowp-store::session_log::SessionLogReader::head` supplies the local log position.
3. Human output writes a `local:` and a `remote:` line plus the note
   `choose which side advances; no merge performed`, and exits 4 (conflict). `--json` writes one
   stable document: `{"error":{"kind":"conflict","code":"session_divergence","message","hint",
   "node","source_session","local","remote","note"}}`, where each head carries `session`, `node`,
   `last_seq`, `last_event`, `last_event_at`, `state`, `executor` and `updated_at` (nullable).
   Neither form prints a token, cwd or home path.
4. The explicit choice is manual and outside this slice: delete or export one side, then pull
   again. There is no force/overwrite flag and no merge. A bundle export still writes its usual
   export capsule on the source; the directory export and the head read take no lock and write
   nothing.

Tests: `session_pull_same_id_reports_divergence_without_mutating_it` and
`session_pull_full_reports_divergence_without_mutating_it` (both sides advanced, both heads,
human and JSON shape, unchanged local log/manifest/row, clean temp directory),
`session_pull_full_prefix_of_an_existing_session_keeps_the_local_conflict`, plus the existing
empty-local success tests `session_pull_same_id_imports_the_origin_id_without_a_parent`,
`session_pull_full_imports_the_origin_id_with_the_full_log` and the default child test
`session_pull_imports_a_child_and_prints_both_ids`. Store tests are
`session_log_head_reports_the_last_complete_event` and
`session_log_head_is_empty_for_an_empty_log`; the core test is
`bundle_head_names_the_source_row_and_last_tail_event` (and its compacted-bundle case).

Still out of scope: automatic merge, a force/overwrite flag, bidirectional sync, `session push`,
live lease migration, a daemon transport, offline queueing, a progress display and a configurable
size limit.

## 7. `vowp session watch` importer

Records an interactive Claude Code session into the canonical store without running it. **Read-only** on Claude's files.

1. Locate transcripts: `~/.claude/projects/<mangled-cwd>/<claude-session-uuid>.jsonl` (read from `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`; the path is a flag/config input, never assumed in tests). `vowp session watch [--claude-dir <dir>] [--once] [--follow]` (the flag names the transcripts root, default `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`; cli.md §4).
2. Map each Claude transcript line to events (`harness=claude-code`, `native_session_id` = uuid): user text → `user.message`; assistant text blocks → `assistant.message`; `thinking` blocks → `assistant.reasoning` (`replayable=false`); `tool_use` → `tool.call`; `tool_result` → `tool.result` (spill > 8192 bytes); usage → `cost` (`source: unknown`, tokens only — subscription sessions have no receipt); unknown line types are skipped and counted.
3. **Incremental by byte offset.** The importer keeps an `ImportCursor { offset, last_uuid }`
   in the node database under `watch:<native session id>`, as JSON. A pass opens the transcript
   read-only, seeks to `offset` and consumes complete lines only: a final line without its `\n`
   stays unread until the newline arrives. `last_uuid` names the last complete line that carried an
   app-side id. A file that shrank below the cursor restarts at offset 0. The old v0.1 marker was a
   bare processed-line count; it is converted to the byte offset after that many complete lines, so
   upgrading does not re-import the prefix. Each imported entry also records its app-side id in
   `native_events`, which makes a re-read after a crash idempotent.
4. **One canonical session per Claude uuid.** The first pass with new lines looks up the canonical
   session mapped to `(claude-code, uuid)` in `session_natives`, creates it when missing
   (`role=user`, title = first user message ≤ 60 chars) and records the mapping; later passes append
   only the new events to that same session. `--follow` follows: it polls every two seconds,
   imports newly completed lines, and exits cleanly on Ctrl-C. A plain TTY run without `--once` or
   `--follow` follows the same way. `--once`, and a non-TTY run without `--follow`, make one
   incremental pass so scripts and tests do not hang. The importer never writes to Claude's
   transcript tree.
5. Coverage lives in `crates/vowp-cli/tests/session.rs` (growing fixture, partial final line,
   follow loop, legacy marker and one-session rule) and `crates/vowp-core/src/session/importer.rs`
   (complete-line and truncation cursor behaviour).

## 8. Surfaces, leases and takeover

A **surface** is where a person talks to a session: `vowp chat`, an opencode app, a Claude app, or
the `vowp mcp` server such an app calls. A **lease** is the right of one surface to write. The
global DB holds `surfaces` and `leases`; rows are live state and are not rebuilt from the logs.

- One lease row per session (`leases.session_id` is the key). It carries `surface_id`,
  `surface_kind`, `node`, `holder`, a random `token`, `acquired_at` and `heartbeat_at`.
- A surface registers once and heartbeats every 5 s. A lease without a heartbeat for 60 s is stale
  and another surface may take it; the node tick sweeps stale leases.
- A surface without the current token reads in **follow mode** and cannot write. A clean exit
  releases the lease.
- Taking over replaces the lease row atomically, pushes `IngestAck::LeaseRevoked` on the old
  surface's ingest stream (measured 99.6–265 ms over live SSH, bound < 2 s), drains a running turn
  for up to `drain_secs` (5 s) then interrupts it, writes a capsule, records a switch with reason
  `takeover` when the session already has an executor, appends a `note` after the switch releases
  the log lock, and leaves the old surface in follow mode. A pre-attach session gets the note only.
- The old surface prints:
  `Taken over by phone at 14:05. This surface is now read-only (follow mode).`
- `vowp session takeover <session> --kind <kind> --holder <h> --node <n>` performs the same path;
  `vowp session lease` shows the status, and only an acquire prints the token.
- A surface cannot close another vendor's window. The cut-off is enforced in every write path: the
  chat loop, the MCP write tools and `session ingest`.

Types: `Surface`, `Lease`, `SurfaceRef`, `LeaseStatus`, `TakeoverReport` (types.md §16.2).

## 9. Remote execution (`turn-begin`, `__exec`, `ingest`, spool)

A turn may run on the surface's own machine (for example a laptop) while the session stays on the
home and the home remains the only writer.

1. The surface calls the hidden
   `vowp session turn-begin <session> --executor <exec> --purpose <p> --node <n> --request <file|->`.
   The request document carries the lease token, prompt, executor, purpose and node; it is read from
   stdin/file, never argv. The home validates the lease, writes
   `session.executor{attach}`, `turn.started`, `user.message` (seeded from the capsule when the
   native session is absent), inserts the project `runs` row with `node=<executor-node>`, and prints
   a `TurnGrant { run_id, session_id, executor, purpose, node, cwd, prompt, resume_native,
   native_hint, env_names, lease_token, tail_seq }`.
2. The client writes the grant to a mode-0600 file and runs `vowp __exec --grant <file>` on the
   executor machine. `__exec` starts the adapter with the same `StartInput`, resolves `env_names`
   from the executor's own environment/vault (a missing name is a clear error, never an empty
   value), and appends every adapter event to `<spool>/<session>/<run>/events.jsonl` with fsync
   **before** sending it. The executor owns its harness binary and any injected value; the home
   never receives a secret value over the wire.
3. `__exec` prints `RemoteLine` JSON objects on stdout; the first is `RemoteLine::Hello`, which
   carries the lease token. The client pipes them over SSH into
   `vowp session ingest <session> --run <run-id>`. The token is never in argv or `ps`.
4. `ingest` validates the token, assigns `seq`, appends with the supplied event id and timestamp,
   updates the index, heartbeats the lease, and prints `IngestAck { event_id, seq }`. It keeps a
   bounded in-flight window (64 events) so a WAN round trip is not paid per event.
5. Acks are written to `acked.json` by event id. On reconnect, `ingest --replay` resends from the
   first unacked event; a duplicate id is acknowledged, not appended; the same id with different
   content is `duplicate-conflict`. A killed client, not only a dead SSH, resumes from the spool. A
   torn final line is truncated; corruption before the last complete line is an error.
6. `RemoteLine::Summary` carries the harness summary and usage. `ingest` writes `cost`, `limit` or
   `error`, `turn.ended`, `session.executor{detach}`, finishes the `runs` row, writes ledger rows
   and `run_usage` (a default row with `source='default'` when the executor could not measure), and
   acks `{run_id, done:true}`.
7. A laptop that dials the home is registered once with `vowp node add <name> -`; the home never
   dials it. `control_master` multiplexing is an optimisation for reconnects, never a correctness
   requirement.

Wire types: `RemoteEvent`, `RemoteLine`, `IngestAck`, `TurnGrant` (types.md §16.2). The
`vowp-adapters` `proc` scope and `run_usage` measurement are in `capacity`/`types.md` §16.1.

## 10. App hosts, transcript mirror and resume

The primary surfaces are the apps people already use. VOWP hosts the session and gives the app a
project-local configuration.

- **opencode host**: runs `opencode serve` for an orchestrator on the home with a private
  `XDG_STATE_HOME`/`XDG_DATA_HOME` under `~/.vowp/orchestrators/<name>/`, binds `127.0.0.1` only,
  and prints join instructions for the desktop (`ssh -L` + `opencode pair`, a one-time link that
  expires after five minutes) and the phone (SSH + `opencode --server …`).
- **Claude Remote Control**: starts only when a claude.ai login already exists on the home. VOWP
  reads only `claude auth status` (`loggedIn`/`authMethod`), never login or credential files. The
  project-local `.mcp.json` needs one owner approval; without a login the orchestrator is recorded
  `degraded` and the command exits non-zero.
- **mirror**: each app transcript is mirrored into the canonical log when the orchestrator is
  created, resumed, or resumed by the supervisor at startup. A pass reads entries after the cursor,
  appends each at most once (the native-id map skips duplicates), and writes
  `~/.vowp/orchestrators/<name>/mirror.json` with the last mirrored app id and update time. This is
  not a continuous watcher; the next create/resume/startup pass catches up. A read or cursor failure
  is logged and does not stop the host.
- **resume**: `session_resume` (MCP) builds the capsule and the last turns for a new app from the
  log; `surface::resume_context` is implemented and tested in core. The new app continues the same
  session id and log.

## 11. MCP tools and what a surface may write

`vowp mcp --session <id> --surface <id> --token-file <path>` is a stdio MCP server using plain
threads. Read tools: `fleet_status`, `fleet_queue`, `inbox_list`, `capacity_report`,
`session_tail`. Write tools: `inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_set`,
`tunnel_open`, `tunnel_close`, `secret_request`, `session_handoff`, `session_resume`.

Every write reads the token file on each call, checks the lease and records the actor
`session:<id>/surface:<surface-id>`. Read tools work in follow mode; write tools refuse after a
takeover. `tunnel_open`/`tunnel_close` exist only on a control-machine surface and refuse on the
home.

| Surface | Read | Write | Lease |
|---|---|---|---|
| `vowp chat` | yes | turn input and slash commands | required |
| opencode app via MCP | yes | MCP write tools only | required; checked on every call |
| Claude app via MCP | yes | MCP write tools only | same |
| fleet/status/capacity/project CLI | yes | writes go to the home | no session lease |
| `vowp session ingest` | yes | event append for one run | token in the grant |
| `vowp session tail --follow` | yes | no | not required |

## 12. Named tests (consumers)

The tests live in `crates/vowp-store.md` §6 (log, blob, reindex) and `crates/vowp-core.md` §3 (session module); those files are the **only** place test names are defined. The table maps each requirement of this document to the exact test name that must prove it:

| Requirement (this document) | Exact test name | Crate |
|---|---|---|
| §3 prompt contains schema, previous capsule and in-budget events | `capsule_prompt_golden` | core |
| §3 compaction keeps the first user message and the last 20 events, drops tool results first | `capsule_compaction_keeps_first_user_and_last_20` | core |
| §3 one repair attempt then success (exactly 2 capsule runs) | `capsule_write_repairs_once_then_ok` | core |
| §3 second invalid output keeps the previous capsule and appends an `error` event | `capsule_write_twice_invalid_keeps_previous_and_logs_error_event` | core |
| §3 debounce 120 s, exempt triggers | `capsule_debounce_120s_except_manual_switch_limit_export` | core |
| §5 happy-path event order and rows | `switch_happy_path_events_rows_and_lineage` | core |
| §5 target probe fails → session unchanged, switch aborted | `switch_target_probe_fails_stays_on_source` | core |
| §5 crash recovery (never attached → aborted; attached → completed) | `recover_started_switch_never_attached_is_aborted`, `recover_started_switch_attached_is_completed` | core |
| §6 export → import round trip (child session, 4 setup events, seed text) | `bundle_export_import_roundtrip_child_session` | core |
| §6 tampered bundle / unknown format rejected, nothing created | `bundle_checksum_mismatch_rejected`, `bundle_unknown_format_rejected` | core |
| §6 only blobs ≤ 64 KiB inlined (60 KB yes, 70 KB no) | `bundle_inlines_small_blobs_only` | core |
| §6.5 same-id absent → origin id, no parent, log/index read back | `same_id_import_keeps_the_origin_id_without_a_parent` | core |
| §6.5 same-id present → conflict, existing row/log unchanged | `same_id_import_refuses_an_existing_session_and_changes_nothing` | core |
| §6.5 duplicate event id → conflict before any write | `same_id_import_rejects_a_duplicate_event_id_before_writing` | core |
| §6.5 default import still creates a child with the parent | `child_import_still_creates_a_child_with_the_parent` | core |
| §6.5 fake-ssh pull in both modes | `session_pull_same_id_imports_the_origin_id_without_a_parent`, `session_pull_imports_a_child_and_prints_both_ids` | cli |
| §6.5 corrupted bundle fails before any write | `same_id_import_rejects_a_corrupt_bundle_before_writing`, `session_pull_same_id_refuses_a_corrupt_bundle_before_importing` | core, cli |
| §6.6 full pull keeps the origin id and the full log, with manifest checksums | `session_pull_full_imports_the_origin_id_with_the_full_log` | cli |
| §6.6 full pull refuses a present id without touching it | `session_pull_full_reports_divergence_without_mutating_it` | cli |
| §6.6 corrupt or over-cap full pull leaves no final directory | `session_pull_full_refuses_a_corrupt_snapshot_without_leaving_a_directory`, `session_pull_full_refuses_an_over_cap_snapshot_before_copying` | cli |
| §6.6 manifest validation, size cap and digest verification | `session_dir_manifest_verifies_and_detects_corruption`, `session_dir_manifest_refuses_a_directory_over_the_size_cap`, `session_dir_manifest_rejects_traversal_and_reserved_paths` | store |
| §1 log, blob and reindex rules | the `log_*`, `blob_*`, `reindex_*` tests of vowp-store.md §6 | store |
| §8 lease acquire, heartbeat, stale sweep, exact holder | `lease_acquire_is_one_holder`, `heartbeat_keeps_lease_live`, `stale_lease_free_after_sixty_seconds` | core |
| §8 takeover cuts off the old surface within 2 s | `takeover_cuts_old_surface_within_two_seconds`, `lease_revoked_is_pushed_within_2s` | core |
| §8 takeover drains, writes a capsule and leaves follow mode | `takeover_drains_and_writes_capsule`, `follow_mode_prints_new_events_and_refuses_writes` | core |
| §8 a pre-attach session gets the note only; a wrong token is rejected | `takeover_before_first_executor_writes_note_only`, `lease_check_rejects_wrong_token` | core |
| §9 framing, seq assignment and idempotent ingest | `turn_begin_writes_framing_events_and_returns_grant`, `remote_exec_streams_events_and_home_assigns_seq`, `remote_ingest_is_idempotent_by_event_id` | core |
| §9 spool before send; replay after SSH drop or killed client | `exec_spools_and_fsyncs_before_send`, `ssh_drop_spools_and_replays_without_gap_or_duplicate`, `client_crash_resumes_from_spool` | core |
| §9 bad lease, summary and usage | `remote_ingest_rejects_bad_lease`, `remote_summary_finishes_run_and_terminal_events`, `remote_summary_writes_run_usage` | core |
| §9 token never in argv or logs; remote probe before a switch | `lease_token_never_in_argv_or_logs`, `remote_probe_uses_remote_node_and_switch` | core |
| §10 opencode host and transcript mirror | `opencode_host_starts_and_stops_with_fake_server`, `mirror_is_incremental_and_idempotent`, `orchestrator_state_survives_supervisor_restart` | core |
| §10 Claude Remote Control refuses without a login; resume context | `claude_remote_refuses_without_login`, `resume_context_builds_capsule_and_tail` | core |
| §11 MCP write checks the lease and records the actor | `mcp_write_tool_checks_lease_and_records_actor`, `mcp_old_app_write_gets_read_only_notice` | cli |
| §8/§9 chat fallback lease rules | `chat_takes_over_and_prints_notice`, `chat_follow_mode_is_read_only` | cli |
