# vowp-daemon

The loopback HTTP API for one VOWP project.

`vowp up` starts this binary as a separate child process for the web window, prints its
loopback address as `web window: http://127.0.0.1:<port>/`, inherits `VOWP_API_TOKEN`, and
stops it on `vowp down` or SIGINT/SIGTERM. Running the binary directly remains supported.

`vowp-daemon [--home <dir>] [--project <name>] [--port <n>]` opens an existing VOWP home
read-only for its GET endpoints (and writable-but-non-creating for the two writes),
serves the static board page at `GET /` and its phone alias `GET /board`, the
static session list at `GET /sessions`, session detail at `GET /sessions/{id}`, the
individual capsule page at `GET /sessions/{id}/capsules/{capsule_id}`, the read-only nodes
page at `GET /nodes`, and the API endpoints `GET /api/v1/health`, `/api/v1/board`,
`/api/v1/board/stream`, `/api/v1/jobs`, `/api/v1/sessions`,
`/api/v1/sessions/{id}`, `/api/v1/sessions/{id}/capsules/{capsuleId}`,
`/api/v1/sessions/{id}/events`, `/api/v1/sessions/{id}/events/stream`,
`POST /api/v1/sessions/{id}/switch`, `/api/v1/inbox`,
`/api/v1/nodes`, `POST /api/v1/inbox/{id}/answer` and `/api/v1/cost`, and binds
`127.0.0.1` only. The default
port is `0`, so the operating system
picks an ephemeral port and the binary prints the bound address. Session detail returns the
exact `Session`, its latest capsule (or null) and at most 32 parent links, nearest parent
first; the capsule route returns only a capsule that belongs to the path session.

The board and session pages are plain HTML, CSS and vanilla JavaScript embedded in the
binary with `include_str!`; the board fetches `/api/v1/board` on load, then follows
`/api/v1/board/stream` and keeps a five-second polling fallback, and its card layout is
usable at a 360 px viewport. A `board` frame replaces the board and updates the last-updated
time; a `heartbeat` frame updates only the live line. A stream error or close shows a
visible "Reconnecting..." state, retries with bounded backoff and keeps polling until a
stream opens; a browser without `EventSource` polls only. `/sessions` fetches
`/api/v1/sessions`; `/sessions/{id}` shows one session's metadata and a read-only event
timeline. The timeline first fetches `/api/v1/sessions/{id}/events?after_seq=`, then opens
`/api/v1/sessions/{id}/events/stream?after_seq=` and appends `session` frames while using
`heartbeat` frames only to refresh the live line. A stream error or close shows a visible
reconnecting state, starts the five-second polling fallback at the same cursor and retries
the stream with bounded backoff; a browser without `EventSource` polls only. The session
detail panel links to `/sessions/{id}/capsules/{capsule_id}`, which fetches
`/api/v1/sessions/{id}/capsules/{capsuleId}` and renders the capsule's metadata and
structured fields read-only, with a visible not-found line when the API answers 404. The
session detail page also carries the token-gated executor switch form. No Node or TypeScript
build step is involved.

The per-session stream at `/api/v1/sessions/{id}/events/stream?after_seq=` sends one
`event: session` frame per stored `EventEnvelope`, a periodic `event: heartbeat` frame and a
single `event: resync` frame if the log was compacted away or truncated behind the cursor.
The board stream at `/api/v1/board/stream` sends the exact `Board` document as an
`event: board` frame on open and whenever the snapshot changes apart from its `at`, plus
periodic `event: heartbeat` frames; it has no cursor, resume or resync. The board and session
detail pages use their streams. The session-list, cost and inbox pages still poll their JSON
endpoints; their reconnect UI is later.

The nodes page fetches `/api/v1/nodes` on load and every five seconds and renders each stored
node's name, SSH target, local/remote flag, last-seen time, `capabilities.vowp` version and a
compact capabilities/probe summary. It is read-only: no add, remove or probe control and no SSH
path; an empty list shows "No nodes." and a failed fetch keeps the last list and timestamp with
an error line.

The session detail page also carries the executor switch form: the current executor, a target
executor input in `vowp session switch --to` syntax, a reason select, a force checkbox with a
visible drain warning and a result line. It posts `{"to","reason","force"}` to
`/api/v1/sessions/{id}/switch` with the same bearer token, shows "Switching…" while the switch
may drain for up to five seconds, then shows the returned `Switch` state/error and refreshes
the session. The server applies the CLI guards: an unknown session is `404`, a running session
without `force`, a switching or closed session, and a limited target provider are `409`, and an
invalid body or executor is `400`. A failed switch never claims the executor moved.

The cost page renders grouped ledger rows and totals with a `by` selector. The inbox page
renders open, answered and dismissed items with a state selector. Its open items carry the
other browser write: a select of the item's options plus a free-text input (or only a text input
when there are no options) and an Answer button. A token unlock panel on both pages keeps
`VOWP_API_TOKEN` in `sessionStorage`, never in the URL; `401` clears it, `503` shows the
writes-disabled message and `202` shows "Answer queued; the supervisor applies it on the next
tick" and refreshes the list. Reads work without a token.

Reads are token-free. `POST /api/v1/inbox/{id}/answer` and
`POST /api/v1/sessions/{id}/switch` require `Authorization: Bearer <VOWP_API_TOKEN>`. The binary
reads the variable at startup; missing or empty disables writes with `503` and leaves reads
available. The token is compared in constant time and never logged, echoed or put in a URL;
rotate it by restarting the daemon. Both handlers open the existing home with
`Store::open_writable` (never `Store::open`): the answer writes the same item update and
`answer_inbox` request row as `vowp inbox answer` and leaves job state to the supervisor; the
switch runs the same `SessionService` switch as `vowp session switch` and never creates or
migrates a home. The listener is loopback-only; this is not TLS or a multi-user login.

The API serves the same `vowp-protocol` objects the CLI prints. Spec:
[`docs/design/v0.3/http-api.md`](../../docs/design/v0.3/http-api.md).
