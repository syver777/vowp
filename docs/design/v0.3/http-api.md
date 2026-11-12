# v0.3 — daemon HTTP + SSE API

Status: the read API (including session detail, single-capsule reads and the node list), the
per-session SSE stream, the board snapshot SSE stream and the two token-gated writes (inbox
answer and local session switch) are implemented in `crates/vowp-daemon`, and `vowp up`
starts the daemon as a separate child process; the rest of this note is design. The daemon
is not part of v0.2. Nothing here is implemented in v0.1.

## First slice (implemented)

`crates/vowp-daemon` ships a library and the `vowp-daemon` binary:

`vowp-daemon [--home <dir>] [--project <name>] [--port <n>]`

It resolves the home from `--home`, `VOWP_HOME`, then `$HOME/.vowp`; selects the only
non-reserved registered project (or the one named by `--project`, with a clear error when
zero or several are registered); reads the node name from that project's registered
`vowp.toml` config; and opens the home with `Store::open_read_only`. The answer and switch
handlers open the same existing home with `Store::open_writable`, which also never creates or
migrates it. The listener binds `127.0.0.1` only, defaults to port `0` and prints the bound
address.

Served endpoints; every response is JSON except the two SSE streams and the static pages.

- `/api/v1/health` — binary version, node, project, project schema version and the
  supervisor heartbeat from `<home>/heartbeat` (`null` when absent, unreadable or stopped).
- `/api/v1/board` — the exact `vowp board --json` document, computed from
  `BoardSnapshot::collect` against the served project's read-only store and registered
  `VowpConfig`.
- `/api/v1/board/stream` — a `text/event-stream` response. The first frame is
  `event: board` with the exact `Board` JSON from `GET /api/v1/board`; the snapshot is then
  collected once per second and another `event: board` frame is sent only when the document
  changes apart from its wall-clock `at`. An `event: heartbeat` frame with
  `{"at":"<RFC 3339>"}` is sent every 15 seconds. There is no cursor, `after_seq`, resume,
  merge or resync: the board is a self-contained snapshot. Each poll opens and drops its own
  read-only store on `spawn_blocking`, so no handle is held between polls and a slow client
  waits for the next snapshot. A collect error ends the stream cleanly; the page reconnects
  and keeps its five-second polling fallback. The board still has no write or auth surface.
- `/api/v1/jobs?state=&kind=&role=` — a `JobPage`; `state` may repeat or be comma-separated.
- `/api/v1/sessions?project=&job=&state=` — a `SessionPage`; `project` defaults to the
  served project.
- `/api/v1/sessions/{id}` — a `SessionDetail`: the exact `Session`, its latest capsule by
  `at_seq` (or null) and its parent lineage, nearest parent first, bounded at 32 entries and
  stopped when a parent row is missing or a parent session repeats.
- `/api/v1/sessions/{id}/capsules/{capsuleId}` — the exact stored `Capsule` when it belongs
  to that session; an unknown session, unknown capsule or a capsule of another session is
  `404`.
- `/api/v1/sessions/{id}/events?after_seq=<n>&limit=<n>` — an `EventPage` of exact stored
  `EventEnvelope` objects, in sequence order; the default limit is 200 and the cap is 1000.
  A full page's `next` is the cursor for the next `after_seq`.
- `/api/v1/sessions/{id}/events/stream?after_seq=<n>` — a `text/event-stream` response with
  one `event: session` frame per stored `EventEnvelope`, in sequence order and with the exact
  stored JSON as the `data:` line. `after_seq` defaults to 0 and an invalid value is `400`;
  an unknown session is `404` before the stream starts. An `event: heartbeat` frame with
  `{"at":"<RFC 3339>"}` is sent every 15 seconds, including while no events are flowing.
  The log is polled every 200 ms in bounded batches (at most 256 events per poll), so a slow
  client applies HTTP backpressure instead of growing an unbounded queue. When the cursor can
  no longer be served (the log was compacted away, removed or truncated behind it), the stream
  sends one `event: resync` frame with `{"after_seq":<n>}` naming the newest readable cursor,
  or the client's own cursor when nothing is readable, and closes; the client may reconnect
  with that cursor. The stream is read-only and has no token, lease or poll fallback.
- `/api/v1/nodes` — a `NodePage` of the exact stored `Node` objects, ordered by name, from the
  served project's read-only store; `total` is the item count and `next` is `null`. Node
  registration, removal and probing stay CLI-only: this route is read-only and adds no SSH,
  probe or write path.
- `/api/v1/inbox?state=open|answered|dismissed` — an `InboxPage` over the served project;
  `state` defaults to `open`. Token-free.
- `POST /api/v1/inbox/{id}/answer` — one write. The JSON body is
  `InboxAnswerRequest {"answer":"<text>"}`. It requires
  `Authorization: Bearer <VOWP_API_TOKEN>`; a daemon started without a token answers `503`,
  a missing or wrong token is `401` with `WWW-Authenticate: Bearer`, an invalid id or body
  is `400`, an unknown item is `404`, and an item that is not open is `409`. The answer is
  trimmed, rejected when empty or over 10,000 UTF-8 bytes, and stored with `answered_by =
  "web"`. When the item has a job, the handler queues the same `answer_inbox` request as the
  CLI; the supervisor applies it on its next tick and the endpoint does not touch job state.
  Success is `202 Accepted` with the answered `InboxItem`.
- `POST /api/v1/sessions/{id}/switch` — the local session switch. The JSON body is
  `SessionSwitchRequest {"to":"<executor>","reason":"<reason>","force":false}`; `reason`
  defaults to `user` and `force` to false. `to` uses the `vowp session switch --to` syntax and
  is parsed with `Executor::parse`. The route runs the same `SessionService` switch as the CLI
  over a writable-but-non-creating store, with a five-second drain. It requires
  `Authorization: Bearer <VOWP_API_TOKEN>`: a daemon without a token is `503`, a missing or
  wrong token is `401` with `WWW-Authenticate: Bearer`, an invalid id, body or executor is
  `400`, an unknown session is `404`, a running session without `force` is `409`, a
  `switching` or `closed` session is `409`, and a target provider with an active limit is
  `409`. Success is `200` with the completed `Switch`; a failed switch is an `ApiError` and
  never claims the executor moved. This is local-node only: there is no `node` field and no
  remote or SSH switch.
- `/api/v1/cost?by=day|job|role|provider|model&since=&until=` — the exact `vowp cost --json`
  document over the served project. `by` defaults to `job`; `since` and `until` accept RFC 3339
  or `YYYY-MM-DD` (UTC midnight), and `until` is inclusive.

The new wire documents are exported from `vowp_protocol::api` and `schema::all_schemas()`:
`Health`, `Board`, `BoardRun`, `BoardLimit`, `Page<T>`, `JobPage`, `SessionPage`, `NodePage`,
`SessionDetail`, `EventPage`, `InboxPage`, `InboxAnswerRequest`, `SessionSwitchRequest`,
`CostReport`, `CostRow`, `CostTotal`, `ApiError` and `ApiErrorBody`. Failures use the same
`ApiError` shape as `vowp --json` errors and map to `400` for an invalid filter, id, body or
executor, `401` for a missing or wrong write token, `404` for an unknown session, capsule or
inbox item, `409` for an inbox item that is not open, a switch guard or an aborted switch,
`503` when writes are disabled and `500` for a store, config or I/O failure. Switch error
messages stay path-free.

**Two writes, one token.** Every `GET` stays token-free and read-only. The privileged actions
are `POST /api/v1/inbox/{id}/answer` and `POST /api/v1/sessions/{id}/switch`, and the listener
stays loopback-only. The daemon reads the token from the `VOWP_API_TOKEN` environment
variable at startup; a missing or empty value leaves reads working and disables both writes
with `503`. The value is compared in constant time, is never logged, echoed, put in a URL or
returned in a response, and rotation is a daemon restart with a new value. The pages keep it
in `sessionStorage` and clear it on `401`. Both writes open the existing home with
`Store::open_writable` and never create or migrate it. The switch is local-node only; a
running session needs an explicit `force` before the switch drains and stops it.

Still later: every other write endpoint (job retry/park/cancel, capsule edits), the reconnect
UI on the pages that still poll, the cost dashboard page, node add/remove/probe and the
remaining browser pages. There are no placeholder routes for any of them. The per-session SSE
stream is implemented and used by the session detail page; the board stream is implemented and
used by the board page; the inbox answer form and the session switch form are implemented and
use the two write routes.

Tests are hermetic: a temporary `NodePaths`, a real `127.0.0.1:0` listener, fake clocks,
no real home, harness, tmux server or network beyond loopback. Every returned document is
validated against its exported JSON Schema, not just round-tripped through serde.

## Starting the window from `vowp up` (implemented)

The process-shape question is resolved in favour of the **separate `vowp-daemon` child
process**. An in-supervisor thread would have to share the supervisor's store handles, error
path and shutdown, while the daemon already owns its read-only/read-write split. The separate
process keeps that split intact: the supervisor keeps its `<home>/supervisor.lock`, the daemon
opens the existing home itself, and there is no second home lock and no placeholder token.

`vowp up` locates the binary from `VOWP_DAEMON_BIN` when set (tests and unusual installs),
then the `vowp-daemon` sibling next to the running `vowp` executable, then `PATH`. It starts
`vowp-daemon --home <home> --project <project> --port 0` as a direct child, inheriting the
environment (including `VOWP_API_TOKEN`) and forwarding the child's stderr with the token
value and home path redacted. The child shares the supervisor's process group, so a terminal
Ctrl-C or a control-group stop reaches it too. No token file, second home lock or
new token storage is added; a missing or empty `VOWP_API_TOKEN` still leaves reads working and
writes disabled.

The window serves one project while the supervisor serves every active project. The checkout
named by `--project` (or the current directory) is used when `up` runs from one; otherwise the
home must have exactly one non-archived project. An ambiguous or empty home prints a warning
and leaves the window off rather than guessing.

The daemon binds `127.0.0.1:0` and prints one `vowp-daemon: listening on http://…` line. The
supervisor reads that line and prints `web window: http://127.0.0.1:<port>/`; it also writes
the URL to `<home>/web-window` so a detached `vowp up` (whose foreground child owns the log)
can print the same line, and removes the file on shutdown. A missing or unstartable binary
prints `warning: web window not started: …` and the supervisor keeps running: the window is
optional, the supervisor is not. `up --once` runs one tick and starts no daemon.

After the daemon reports its address, the human `vowp up` output also carries the
control-machine command for that actual port and the local URL shape:

```text
web window: http://127.0.0.1:54321/
remote window: vowp tunnel open window --remote-port 54321
  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)
```

The command is the existing `vowp tunnel open` managed SSH forward. It runs on the laptop or
phone that will use the window and forwards a local loopback port to the home's loopback daemon
port. The daemon stays bound to `127.0.0.1`, the home's only inbound service remains SSH, and
there is no public bind, TLS terminator, relay or second listener. The hint is printed only when
the daemon reported an address: a missing daemon warns and prints no port or tunnel command.
`vowp up --json` keeps the address in `web_window` and prints no human lines.

The browser opens the tunnel's local loopback URL, not a home URL, so the write token is never
part of a URL. `VOWP_API_TOKEN` is inherited by the daemon from the environment `vowp up` runs
in; the page stores the value it is given in `sessionStorage` and uses it only for the inbox
answer and session switch writes. Reads stay token-free. Rotation is a daemon restart with a new
value followed by an unlock; there is no token file or second store.

On its shutdown path (normal stop file, first SIGINT/SIGTERM, supervisor error or early
startup failure) the foreground supervisor sends SIGTERM to the daemon, waits up to five
seconds, escalates to SIGKILL, reaps the child and removes the address file. No daemon child
is left behind. Tests use `VOWP_DAEMON_BIN` with a fake daemon that records its argv and
token, prints a fake address and traps SIGTERM; the real home, tmux server and harnesses are
never touched.

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
- `GET /api/v1/board/stream` — SSE `board` frames carrying the exact `Board` document when it
  changes, plus `heartbeat` frames; no cursor or resume.
- `GET /api/v1/inbox` and `POST /api/v1/inbox/{id}/answer` — read and answer; both are
  implemented. The answer is one of the two token-gated writes.
- `GET /api/v1/cost?by=day|job|role|provider|model&since=&until=` — the `vowp cost --json` shape;
  implemented in this slice.
- `GET /api/v1/nodes` — a `NodePage` of the exact stored nodes, ordered by name; implemented
  read-only in this slice for the web-window `/nodes` page.
- `GET /api/v1/sessions/{id}/capsules/{capsuleId}` — one capsule belonging to the session;
  implemented in this slice. The window's capsule view remains later.
- `POST /api/v1/jobs/{id}/retry|park|cancel` — enqueue the existing
  `vowp_core::requests::Request`; do not duplicate the state machine.
- `POST /api/v1/sessions/{id}/switch` — run the same `SessionService` switch path as
  `vowp session switch`; implemented in this slice for the local node. The v0.1 request queue
  has no switch variant, so it calls the service directly through the token-gated daemon
  write, with the CLI guards and a five-second drain. It never creates or migrates a home.

The node list is implemented read-only in this slice: it serves the existing `Store::list_nodes`
rows and the web-window `/nodes` page reads them. Do not add a placeholder route for a future
node write; adding, removing or probing nodes and any SSH or exec path remain out of scope. The
single-capsule route is implemented read-only in this slice; the capsule web view is later.

A write response returns the affected row or the request id; consumers poll or wait for the next
event. The inbox answer write returns the answered `InboxItem` with `202 Accepted`; the
supervisor applies the queued request on its next tick. A browser action must never apply a
state transition directly.

## Wire contract

`EventEnvelope` is already frozen for v0.1 (`docs/design/architecture.md` §8): v0.3 may add optional
fields and new event types, but readers ignore unknown **fields**; unknown event **types** are a hard
error in v0.1 and should be tolerated as opaque in v0.3. The API sends the envelope as-is rather than
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
  the canonical limit class (`quota`, `credits`, `rate`, `context`, `auth` or `unavailable`) and
  `resets_at` is an RFC 3339 time or null.
- `Page<T> { items, next, total }` for lists.
- `NodePage` = `Page<Node>` — the `/api/v1/nodes` document. Each item is the exact stored
  `Node` (`name`, `ssh_target`, `is_local`, `capabilities`, `last_seen_at`, `created_at`),
  ordered by name; `next` is null and `Node` keeps its existing shape and schema.
- `SessionDetail { session, latest_capsule, lineage }` — the `/api/v1/sessions/{id}` document.
  `session` is the exact `Session`, `latest_capsule` is the capsule with the highest `at_seq`
  (or null when the session has none), and `lineage` is the parent-session chain, nearest
  parent first, at most 32 entries, stopped when a parent row is missing or a parent session
  repeats, or when the bound is hit.
- `InboxPage` = `Page<InboxItem>` — one inbox list page.
- `InboxAnswerRequest { answer }` — the body of `POST /api/v1/inbox/{id}/answer`. The
  response reuses `InboxItem`; there is no separate answer response type.
- `SessionSwitchRequest { to, reason, force }` — the body of
  `POST /api/v1/sessions/{id}/switch`. `to` is the CLI's executor string, `reason` is a
  `SwitchReason` (default `user`) and `force` defaults to false. The response reuses
  `Switch`; there is no separate switch response type.
- `CostReport { by, rows, total }` — the exact `vowp cost --json` document. `by` echoes the
  grouping; `rows` and `total` split known USD, token usage, source and unknown-entry counts.
- `CostRow { key, runs, usd, tokens, source, unknown_entries }` — one grouped cost row.
  `usd` is null when every entry is unknown, and `source` is `receipt`, `price-table`,
  `unknown` or `mixed`.
- `CostTotal { runs, usd, tokens, source, unknown_entries }` — the totals row.
- `ApiError { error: { kind, message, hint? } }` — the same shape as `vowp --json` errors.
- An SSE frame with `event: session` / `event: board` / `event: heartbeat` / `event: resync`
  and the JSON data line. This is a `text/event-stream` frame, not a JSON document, so it has
  no schema of its own: the `data:` line is an exact `EventEnvelope` for `session`, the exact
  `Board` document for `board`, `{ at }` for `heartbeat`, and `{ after_seq }` for the one
  recovery `resync` frame. The board stream uses `board` and `heartbeat` only; it has no
  `resync`.

`vowp schema` remains the source of truth; add `Health`, `Board`, `BoardRun`, `BoardLimit`, `Page`,
`SessionDetail`, `NodePage`, `InboxAnswerRequest`, `SessionSwitchRequest` and `ApiError` to
`all_schemas()` rather than hand-writing JSON examples. Export the concrete page instantiations
the API serves (`JobPage`, `SessionPage`, `NodePage`, `EventPage` and `InboxPage`) and
`CostReport` alongside their item types, and keep the SSE frame as the one explicit schema
exception above.

## Existing crates it builds on

- `vowp-store` — `Store`, `JobFilter`, `SessionFilter`, `event_rows`, `cost_totals`, `list_inbox`.
- `vowp-core` — `Deps`, `BoardSnapshot::collect`, `SessionService` (the store-injected switch),
  `requests::Request` and the ledger query.
- `vowp-protocol` — every served type and `schema::all_schemas`.
- `vowp-cli` — keep as the first client; its `--json` shapes are the reference for the API.
- New `vowp-daemon` crate proposed: `axum` + `tokio` live here, not in `vowp-core`.

## Non-goals

- A public listener, TLS termination, a hosted relay or remote access without SSH.
- Multi-user identity, quotas or per-user permissions (v0.3).
- A second job/session state machine. The request queue and the supervisor remain the writer.
- Cross-node or SSH session switching from the daemon; the switch route is local-node only.
- Replacing `vowp status`, `vowp board`, `vowp session tail` or the tmux windows.

## Open questions

1. **Process shape.** Resolved: the daemon is the separate `vowp-daemon` child process described
   above. The supervisor keeps its lock, the daemon opens the home itself and there is no second
   lock or ownership story. An in-supervisor thread was rejected because it would share the
   supervisor's store handles and shutdown path.
2. **Scope.** One project per daemon, matching one supervisor per project home. `vowp up` passes
   the checkout's project, or the only non-archived project; an ambiguous home leaves the window
   off with a warning. Starting `vowp-daemon` directly still takes `--project`.
3. **Cursor semantics.** Resolved for the board stream: the board is a self-contained
   snapshot, so `GET /api/v1/board/stream` sends the exact `Board` document as a `board`
   frame whenever its content changes apart from `at`, with no cursor, `after_seq`, event
   merge or `resync`. `session_events.seq` remains per session for the timeline stream and
   the ledger has a global autoincrement id; no global event log is introduced.
4. **Token storage.** Resolved for this slice: the daemon reads `VOWP_API_TOKEN` from its
   environment at startup. Missing or empty disables writes; rotating the token means restarting
   the daemon with a new value. There is no token file, rotation service or multi-user account.
   The value is never logged or put in a URL; the page keeps it in `sessionStorage` and clears it
   on `401`.
5. **Backpressure.** Answered for both streams. Each session poll reads at most 256 events,
   the response stream yields one frame at a time and lets HTTP backpressure slow the client,
   and a cursor that falls behind a truncated log gets one `resync` frame and a clean close.
   The board stream collects one snapshot per poll, holds no store between polls and sends a
   frame only when the snapshot changes, so a slow client only ever waits for the next
   snapshot.

## First-issue-sized slice

Implemented: `crates/vowp-daemon` opens an existing node home read-only for reads
(and writable-but-non-creating for the two writes) and serves
`GET /api/v1/health`, `GET /api/v1/board`, `GET /api/v1/board/stream`, `GET /api/v1/jobs`,
`GET /api/v1/sessions`,
`GET /api/v1/sessions/{id}`, `GET /api/v1/sessions/{id}/capsules/{capsuleId}`,
`GET /api/v1/sessions/{id}/events?after_seq=`,
`GET /api/v1/sessions/{id}/events/stream?after_seq=`,
`POST /api/v1/sessions/{id}/switch`, `GET /api/v1/inbox?state=`,
`POST /api/v1/inbox/{id}/answer` and `GET /api/v1/cost?by=&since=&until=`. It binds
`127.0.0.1:0` in tests, uses a temporary store with jobs in several states, one running job
and a parent/child session chain with a capsule and a bounded long chain, a limit,
open/answered inbox items, cost ledger rows from every source and a checkpoint, and asserts
that every JSON document validates against its exported schema — including `SessionDetail`
for the session detail, `Capsule` for the capsule read, `Board`, `BoardRun` and `BoardLimit`
for the board, `InboxPage` for the inbox and `CostReport` for cost. The SSE tests use a real
loopback listener and short injected intervals: the session stream compares exact
`EventEnvelope` frames and appends live through `SessionLog::open`; the board stream validates
its `board` frames against the `Board` schema, compares them with `GET /api/v1/board` apart
from `at`, changes a job row through a writable store to receive the next snapshot, and
observes heartbeats while the snapshot is stable. The answer tests pin `503` without a token,
`401` for a missing or wrong Bearer value (before the body is parsed), `202` with the answered
`InboxItem` and exactly one `answer_inbox` request row for a job item, `404`, `409`, `400` for
empty/oversized/malformed answers, unchanged token-free `GET`s, and the store never creating a
home. The switch tests pin `503` without a token, `401` for a missing or wrong Bearer value
(before the body is parsed), `200` with a schema-valid completed `Switch` and the moved session,
`404`, the running/switching/closed/limit `409` guards and `400` for malformed bodies and
executors. The page tests pin the switch form, `Authorization: Bearer` use, `sessionStorage`,
the force warning, the state/error line and the absence of a token in the URL. The inbox page
test pins the answer form and its queued message. The `vowp up` tests pin the daemon child's
`--home`/`--project`/`--port` arguments, inherited `VOWP_API_TOKEN`, the printed address, the
exact `vowp tunnel open window --remote-port <port>` hint with the daemon's actual port, the
detached address file and hint, SIGTERM/`down` shutdown without an orphan, and the
warning-and-continue path when the binary is missing (including no tunnel hint). Every other
write endpoint remains later.

Tests are hermetic: temporary `NodePaths`, a fake clock, no network beyond loopback, no real
harness and no tmux.
