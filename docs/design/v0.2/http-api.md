# v0.2 — daemon HTTP + SSE API

Status: current for the commands shown; this note is design only. Nothing here is implemented in
v0.1; the supervisor opens no socket and there is no HTTP API to call.

## Goal

Serve the data the CLI already reads, over a loopback HTTP API, so the web window, the phone board,
the cost dashboard and third-party tools do not have to shell out to `vowp`:

- `GET /api/v1/health` — version, node name, project, schema version, supervisor heartbeat.
- `GET /api/v1/jobs?state=&kind=&role=` — `Job` objects.
- `GET /api/v1/jobs/{id}` — one `Job`; `/runs` and `/gates` as sub-resources.
- `GET /api/v1/sessions?project=&job=&state=` — `Session` objects.
- `GET /api/v1/sessions/{id}` — metadata, latest capsule and lineage.
- `GET /api/v1/sessions/{id}/events?after_seq=<n>&limit=<n>` — exact `EventEnvelope` lines.
- `GET /api/v1/sessions/{id}/events/stream?after_seq=<n>` — SSE frames, one event per `seq`.
- `GET /api/v1/board` — the JSON shape already printed by `vowp board --json` (`at`, `project`,
  `jobs`, `running`, `slots_busy`, `slots_total`, `planner`, `keeper`, `limits`,
  `cost_today_usd`, `inbox_open`, `last_checkpoint`).
- `GET /api/v1/inbox` and `POST /api/v1/inbox/{id}/answer` — read and answer.
- `GET /api/v1/cost?by=day|job|role|provider|model&since=&until=` — the `vowp cost --json` shape.
- `GET /api/v1/nodes` — **later slice**: node name, SSH target, last seen, version and probe state.
  The web-window `/nodes` page needs it, but the window's first slice has no nodes page.
- `GET /api/v1/sessions/{id}/capsules/{capsuleId}` — **later slice**: one capsule for the window's
  capsule view; the window's first slice has no sessions page.
- `POST /api/v1/jobs/{id}/retry|park|cancel` — enqueue the existing
  `vowp_core::requests::Request`; do not duplicate the state machine.
- `POST /api/v1/sessions/{id}/switch` — run the same `SessionOps::switch` path as
  `vowp session switch`. The v0.1 request queue has no switch variant, so this is the one write that
  needs its own daemon-side call and cancellation story.

The node list and the single-capsule route are **later slices**, not part of the first API slice: the
web-window first slice is a static shell and read-only board page with no sessions page and no nodes
page, so it does not call them. Do not add placeholder routes for them to the first slice.

A write response returns the affected row or the request id; consumers poll or wait for the next
event. A browser action must never apply a state transition directly.

## Wire contract

`EventEnvelope` is already frozen for v0.1 (`docs/design/architecture.md` §8): v0.2 may add optional
fields and new event types, but readers ignore unknown **fields**; unknown event **types** are a hard
error in v0.1 and should be tolerated as opaque in v0.2. The API sends the envelope as-is rather than
wrapping the payload, so a stored log line and an SSE `data:` value are the same object.

`Job`, `Session`, `Run`, `GateResult`, `Switch`, `Capsule`, `Limit`, `Cost` and `LedgerEntry` are
served in their `vowp-protocol` JSON form. The new document and transport types are:

- `Health { version, node, project, schema_version, heartbeat }` — the `/api/v1/health` document.
  `version` is the binary version, `node` and `project` are names, `schema_version` is the integer
  store schema, and `heartbeat` is the RFC 3339 time of the last supervisor tick or null when none
  has run.
- `Board { at, project, jobs, running, slots_busy, slots_total, planner, keeper, limits,
  cost_today_usd, inbox_open, last_checkpoint }` — the exact `vowp board --json` document. `jobs`
  maps a canonical state name to a count; `running` is a `BoardRun[]`; `limits` is a `BoardLimit[]`;
  `planner` and `keeper` are executors or null; `cost_today_usd` is null when today's cost is
  unknown; `last_checkpoint` is an RFC 3339 time or null.
- `BoardRun { job, executor, age_secs, last_event_age_secs }` — one running job in `Board.running`.
  `executor` and `last_event_age_secs` are nullable.
- `BoardLimit { provider, kind, resets_at }` — the reduced limit row in `Board.limits`; `kind` is
  the canonical limit class and `resets_at` is an RFC 3339 time or null.
- `Page<T> { items, next, total }` for lists.
- `ApiError { error: { kind, message, hint? } }` — the same shape as `vowp --json` errors.
- An SSE frame with `event: session` / `event: board` / `event: heartbeat` and the JSON data line.
  This is a `text/event-stream` frame, not a JSON document, so it has no schema of its own: the
  `data:` line is an `EventEnvelope` for `session`, a `Board` for `board`, and `{ at }` for
  `heartbeat`.

`vowp schema` remains the source of truth; add `Health`, `Board`, `BoardRun`, `BoardLimit`, `Page`
and `ApiError` to `all_schemas()` rather than hand-writing JSON examples. Export the concrete page
instantiations the API serves (for example `JobPage`, `SessionPage`, `InboxPage` and `CostPage`)
alongside their item types, and keep the SSE frame as the one explicit schema exception above.

## Existing crates it builds on

- `vowp-store` — `Store`, `JobFilter`, `SessionFilter`, `event_rows`, `cost_totals`, `list_inbox`.
- `vowp-core` — `Deps`, `BoardSnapshot::collect`, `requests::Request` and the ledger query.
- `vowp-protocol` — every served type and `schema::all_schemas`.
- `vowp-cli` — keep as the first client; its `--json` shapes are the reference for the API.
- New `vowp-daemon` crate proposed: `axum` + `tokio` live here, not in `vowp-core`.

## Non-goals

- A public listener, TLS termination, a hosted relay or remote access without SSH.
- Multi-user identity, quotas or per-user permissions (v0.3).
- A second job/session state machine. The request queue and the supervisor remain the writer.
- Replacing `vowp status`, `vowp board`, `vowp session tail` or the tmux windows.

## Open questions

1. **Process shape.** Is the daemon a separate planned `serve` process, or an optional thread inside
   `vowp up`? A separate process keeps the supervisor unchanged but needs a second lock/ownership
   story; an in-supervisor thread reuses the heartbeat and `BoardSnapshot`.
2. **Scope.** One project per daemon (matching one supervisor per project home), or every project in
   the node database? Default to one project and make it explicit in the URL or a `--project` flag.
3. **Cursor semantics.** `session_events.seq` is per session; the ledger has a global autoincrement
   id. A board stream needs a merge policy. Start with per-session SSE and board polling.
4. **Token storage.** A random token in `<home>/api-token` (0600) or an environment variable? How is it
   rotated? Never log the value; never put it in a URL.
5. **Backpressure.** A slow SSE client must not block the supervisor. Bound the per-client queue and
   send a `resync` hint when the cursor falls behind.

## First-issue-sized slice

Add `crates/vowp-daemon` with a binary/entry point that opens an existing node home read-only and
serves `GET /api/v1/health`, `GET /api/v1/jobs`, `GET /api/v1/sessions` and
`GET /api/v1/sessions/{id}/events?after_seq=`. Bind `127.0.0.1:0` in tests, use a temporary store with
two jobs and one session, and assert that the JSON validates against the exported `Job`, `Session` and
`EventEnvelope` schemas. No write endpoints and no SSE yet.

Tests are hermetic: temporary `NodePaths`, a fake clock, no network, no real harness.
