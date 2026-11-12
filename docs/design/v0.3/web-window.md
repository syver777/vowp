# v0.3 — web / PWA window

Status: moved from `docs/design/v0.2/` by the v0.2 spec; planned for v0.3, not v0.2. The static
board, session, capsule, cost, inbox and nodes pages, the cost chart, the token-gated inbox answer
and local session switch, and the PWA manifest/service-worker shell are implemented. `vowp up`
starts the serving `vowp-daemon` as a separate child process, prints the window address and the
actual-port `vowp tunnel open window` hint, and the rest of this note is design. v0.1 has no
browser UI and no listening socket.

## Goal

A small installable web app served by the local daemon, usable from the same laptop or over an SSH
tunnel. It should answer the questions an operator asks from a browser without opening a terminal:

- **Nodes** — name, SSH target, last seen, version and probe state.
- **Sessions** — filter by project, job, role and state; show executor, last event and last capsule.
- **Session view** — an event timeline with live updates, lineage, a capsule panel, and an **executor
  switcher** that sends the same switch request as `vowp session switch`.
- **Capsule view** — the capsule's metadata and structured fields (`at_seq`, trigger, `from`
  executor, intent, constraints, decisions, state, files, gotchas and open questions).
- **Board** — job counts, running jobs, slot occupancy, limits, today's cost and open inbox count.

The board uses the same JSON shape as `vowp board --json`; the session list and event timeline use
the API from [`http-api.md`](http-api.md). Reads are token-free. This slice has two browser
writes: answering an inbox item uses the existing request queue behind the daemon bearer token,
and the session detail page switches the local executor through the same `SessionService` switch
as the CLI (the queue has no switch variant). The switch is local-node only, requires the token,
and shows the drain and its blocked result.

## Non-goals

- A code editor, a terminal emulator or a replacement for the tmux windows.
- Sending a new chat turn from the browser in the first slice. That needs a run queue, cancellation and
  a streaming path; design it after the read-only timeline works.
- Multi-user sessions, sharing links or a hosted URL.
- A server-side rendering stack or a Node process in production.

## Existing crates it builds on

- `vowp-daemon` and the endpoints in [`http-api.md`](http-api.md).
- `vowp-protocol` — `Session`, `EventEnvelope`, `Capsule`, `Switch`, `Job`, `Limit`, `Cost`.
- `vowp-core::ledger::BoardSnapshot` — the board snapshot and its state names.
- The `vowp session switch` and `vowp inbox answer` command semantics; the web action must produce
  the same switch call or request and the same ledger rows.

## Shape

- TypeScript, built to static assets and served by the daemon; no runtime Node dependency. The
  implemented slices deliberately skip the build step: every page is a hand-written HTML shell
  with inline CSS and vanilla JavaScript.
- Routes: `/` (board; `/board` is its phone alias), `/sessions`, `/sessions/:id`,
  `/sessions/:id/capsules/:capsuleId`, `/nodes`, `/cost`, `/inbox`. All are implemented, with
  `/nodes` a read-only node list and no node mutation or SSH path.
- PWA manifest and a small hand-written service worker; cache only the static shell, never cache
  event data as authoritative.
- The write token is kept in `sessionStorage`, never in a URL. The inbox page and the session
  switch form show a one-field unlock panel when no token is stored; a `401` clears it and shows
  the panel again. Rotating the token means restarting the daemon with a new `VOWP_API_TOKEN`,
  then unlocking again.
- SSE with `after_seq` resume and a visible "reconnecting" state; fall back to polling if SSE fails.
  The session timeline uses the per-session stream with `after_seq` resume. The board uses the
  snapshot `/api/v1/board/stream` with reconnect and polling fallback. The cost, inbox and nodes
  pages still poll every five seconds.

## Open questions

1. **Frontend stack.** Plain TypeScript + a small DOM helper is enough for the first slice and avoids a
   build-tool decision. Pick a framework only when the session timeline needs it.
2. **Asset embedding.** The first slice embeds one hand-written `web/index.html` with
   `include_str!`, so the binary carries the page and `cargo build` needs no asset step. Decide
   later whether a built `web/dist` is embedded at release time or served from disk in development.
3. **Write scope.** Resolved for the local switch: it is allowed only behind the daemon write
   token and an explicit `force` before it drains and stops a running turn. The form shows the
   drain state and the returned `blocked` result. A run queue and live input remain later.
4. **Mobile vs desktop.** The phone board is a separate note; decide whether it is the same app at a
   narrow width or a dedicated route with less data.

## First slice (implemented)

`vowp-daemon` serves a static, read-only board page at `GET /`. The page is
`crates/vowp-daemon/web/index.html`, embedded in the binary with `include_str!` and returned as
`text/html`. It fetches `GET /api/v1/board` on load and renders the project, job counts by
state, running jobs (job, executor, age, last event age), slots busy/total, planner and keeper,
active limits with reset times, today's cost (`unknown` when the API reports null), the open
inbox count, the last checkpoint and a visible "last updated" time. After that initial fetch it
opens `EventSource('/api/v1/board/stream')`. A `board` frame replaces the whole board document
and updates "last updated"; a `heartbeat` frame updates only the live line. On a stream error
or close the page shows a visible "Reconnecting..." state, retries with bounded exponential
backoff and keeps the five-second polling fallback until a stream open succeeds. If
`EventSource` is unavailable the page polls only. A failed fetch or stream keeps the last board
and its timestamp visible. The page uses text labels as well as colour and carries a
`<noscript>` message. It links to `/sessions`, `/cost` and `/inbox`.

`vowp up` starts this daemon for the window as a separate `vowp-daemon` child process, prints
`web window: http://127.0.0.1:<port>/` together with the exact `vowp tunnel open window
--remote-port <port>` hint for that address, and stops the child on `vowp down` or
SIGINT/SIGTERM. The child inherits `VOWP_API_TOKEN`, so the page's token unlock and the two
writes work without a second token store. The window is optional: a missing daemon binary warns
and the supervisor keeps running. The window serves one project (the checkout `up` runs from,
or the only active project); an ambiguous home leaves it off with a warning. The process shape,
address file and shutdown details are in
[`http-api.md`](http-api.md#starting-the-window-from-vowp-up-implemented).

### Reaching the window from a laptop or phone (implemented)

The daemon stays bound to `127.0.0.1` on the home. On the home, `vowp up` prints:

```text
web window: http://127.0.0.1:54321/
remote window: vowp tunnel open window --remote-port 54321
  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)
```

On the laptop or phone, configure the home once as a control machine (`vowp control init --name
home <home-ssh-target>`) and run the printed command:

```sh
vowp tunnel open window --remote-port 54321
```

That starts the existing managed SSH forward and prints the chosen local loopback port, for
example `tunnel window open: 127.0.0.1:41023 -> 127.0.0.1:54321 (pid …)`. Open
`http://127.0.0.1:41023/` in the browser. Pass `--local-port <n>` when a fixed local port is
wanted. The tunnel is opened from the machine that will use it; it adds no listener on the home
and no public port. The home's only inbound service remains SSH, and the home's firewall needs no
rule for the daemon.

The same page is the PWA at any width. On a phone, use `/board` for the narrow card board (the
same shell is also at `/`) and add the tunnel's local URL to the home screen. Reads need no
token. `VOWP_API_TOKEN` is set in the environment that runs `vowp up` (for example the systemd
unit), inherited by the daemon, and typed into a page's unlock panel; the page keeps it in
`sessionStorage` and only the inbox answer and session switch writes use it. The value is never
put in a URL, logged or returned by the API. Rotating the token means restarting the daemon with
a new value and unlocking again.

The session pages are the same kind of embedded static shell,
`crates/vowp-daemon/web/sessions.html`, served at `GET /sessions` and `GET /sessions/{id}`. The
shell reads its own path. On `/sessions` it fetches `GET /api/v1/sessions` and renders a card
list with id, title, project, job, role, state, executor, last seq and updated time; each row
links to `/sessions/{id}`. On `/sessions/{id}` it fetches the session list to find the metadata
and `GET /api/v1/sessions/{id}/events?after_seq=<n>&limit=200`, renders the metadata and the
event timeline of seq, time, type, actor and a compact summary, then opens
`EventSource('/api/v1/sessions/{id}/events/stream?after_seq=<last rendered seq>')`. It appends
`session` frames in sequence order and ignores sequences already on screen, so a reconnect does
not duplicate a row. `heartbeat` frames update the visible "live updates / last updated" line
and are never rendered as timeline rows. On a stream error or close the page shows a visible
"Reconnecting..." state, starts its five-second polling fallback at `after_seq=<last>`, and
retries the stream with bounded exponential backoff; a successful stream open stops the
fallback. A `resync` frame moves the resume cursor to the readable cursor it names. If
`EventSource` is unavailable the page says so and polls only. A failed fetch or stream keeps the
last rendered events and timestamps visible, and a 404 or failed fetch shows a visible error or
not-found line. On `/sessions/{id}` the page also fetches `GET /api/v1/sessions/{id}` for the
session-detail document. When the session has a capsule, it renders the capsule id, `at_seq`,
trigger, `from` executor, intent, constraints, decisions (`what`, `why`, `at_seq`), state (done,
in progress, blocked, next steps), files (path and action), gotchas and open questions in a
read-only panel with no edit control, and the panel's "View capsule" link opens
`/sessions/{id}/capsules/{capsule_id}`. When the latest capsule is null the panel says "No capsule
yet." The page renders the parent lineage returned by the API as a compact chain of links to
`/sessions/{id}`, using the session-list titles when they are available and the id otherwise, and
says "No parent sessions." when the chain is empty. The detail page also carries the executor
switcher: the current executor, a target input in `vowp session switch --to` syntax, a reason
select, a force checkbox with a visible drain warning, the shared token unlock panel and a
result line. Submitting posts `{to, reason, force}` to
`POST /api/v1/sessions/{id}/switch` with `Authorization: Bearer` and shows "Switching…" while
the current turn may drain for up to five seconds. A `200` shows the returned `Switch` state
and `error` and refreshes the session; a failed request shows the `ApiError` message and never
claims the executor moved. The token stays in `sessionStorage`, a `401` clears it and a `503`
shows the disabled line; it is never put in the URL. The page carries
`<noscript>The session view requires JavaScript.</noscript>`, links back to the board and to
cost and inbox, and never prints the token value, the home path or a provider secret.

The capsule page is `crates/vowp-daemon/web/capsule.html`, served at
`GET /sessions/{id}/capsules/{capsule_id}`. The shell reads its own path and fetches
`GET /api/v1/sessions/{id}/capsules/{capsuleId}`. It renders the capsule id, session id (linked to
`/sessions/{id}`), created time, `at_seq`, trigger, the `from` executor with its harness, model and
profile when present, intent, constraints, decisions (`what`, `why`, `at_seq`), state (done, in
progress, blocked, next steps), files (path and action), gotchas, open questions and one compact
telemetry line. The page is read-only: it has no Markdown parser, capsule editor or write control.
An unknown session or capsule is the API's `404`, which the page shows as a visible "Capsule not
found." line while keeping its back link; any other failed fetch shows an error line and still
leaves the static shell loaded. The page links to the board, the session, cost and inbox, carries
`<noscript>The capsule page requires JavaScript.</noscript>`, and never prints a home path, auth
token or provider secret. Markdown rendering, capsule editing and capsule writes remain out of
scope.

The cost page is `crates/vowp-daemon/web/cost.html`, served at `GET /cost`. It fetches
`GET /api/v1/cost?by=<group>` on load and every five seconds. The selector offers job, day, role,
provider and model (default job), and the URL keeps the chosen value. The page renders the grouped
rows and the total with key, runs, USD, token usage, source and unknown-entry count. Above the
table, a responsive inline CSS bar chart shows the known USD for the selected grouping as
horizontal bars labelled with the row keys and scaled to the largest known row; the figure carries
a visible caption and a `role="img"` label/summary for assistive technology. A row whose USD is
null is never drawn as zero or included in the scale: the chart lists those keys under "Unknown USD
(not charted)" and the table keeps showing `?`. The chart is rebuilt from the same report when the
selector changes and on the five-second refresh, while the table remains the full accessible data
view. A null USD total is shown as `?` and never as zero, and the page states that unknown costs
stay unknown. A failed fetch shows an error line and keeps the last report, chart and timestamp
visible; an empty report shows "No cost entries." in the chart and the table. Price-table editing
remains out of scope. The page links to the board, sessions and inbox, carries
`<noscript>The cost page requires JavaScript.</noscript>`, and never prints a home path, token or
provider secret.

The inbox page is `crates/vowp-daemon/web/inbox.html`, served at `GET /inbox`. It fetches
`GET /api/v1/inbox?state=open` on load and every five seconds. The state selector adds all,
answered and dismissed; "all" fetches the three API states and merges their lists. The page
renders id, kind, job, title, age, state, options, a short body preview and, for an open item, an
answer form. The form offers a select of the item's options plus a free-text input, or only a
text input when the item has no options; the Answer button posts the chosen or typed text to
`POST /api/v1/inbox/{id}/answer` as JSON. A token unlock panel stores the `VOWP_API_TOKEN` value
in `sessionStorage` (never the URL) and hides once stored. A `401` clears the stored value,
shows the unlock panel and leaves the list visible; `503` shows the writes-disabled message;
`202` shows "Answer queued; the supervisor applies it on the next tick" and refreshes the list;
any other error shows a line and keeps the list. Reads continue without a token. A failed fetch
shows an error line and keeps the last list and timestamp visible; an empty list shows "No inbox
items." There is no dismiss or other write action. The page links to the other pages, carries
`<noscript>The inbox page requires JavaScript.</noscript>`, and never prints a home path, token
value or provider secret.

The nodes page is `crates/vowp-daemon/web/nodes.html`, served at `GET /nodes`. It fetches
`GET /api/v1/nodes` on load and every five seconds and renders a table of name, SSH target (or
`-`), local/remote, last seen, version from `capabilities.vowp` and a compact capabilities/probe
summary built only from the stored fields. It shows "No nodes." for an empty list; a failed fetch
shows an error line and keeps the last list and timestamp visible. The page is read-only: it has
no add, remove or probe control and no SSH, exec or write path. It links to the board, sessions,
cost and inbox, carries `<noscript>The nodes page requires JavaScript.</noscript>`, and never
prints a home path, token or provider secret.

## PWA shell (implemented)

`vowp-daemon` serves the installable shell assets, embedded with `include_str!` like the pages:

- `GET /manifest.webmanifest` (`application/manifest+json`) names the app, sets
  `start_url` and `scope` to `/`, uses `display: standalone`, theme colour `#16855b`, background
  colour `#f6f5f1` and one `image/svg+xml` icon at `/icon.svg`.
- `GET /icon.svg` (`image/svg+xml`) is a static SVG mark.
- `GET /sw.js` (`text/javascript`) is sent with `Cache-Control: no-cache` and
  `Service-Worker-Allowed: /`, so it can control the root scope and updates are revalidated.

The service worker is hand-written with no build step. On install it opens cache
`vowp-shell-v2` and caches a bounded `SHELL` list: `/`, `/board`, `/sessions`, `/cost`,
`/inbox`, `/nodes`, `/manifest.webmanifest` and `/icon.svg`. The version was bumped from v1 when
the cost page gained its chart, so existing clients replace the older shell; on activate the worker
deletes every other cache version. The fetch handler bypasses `/api/` and any `/stream` URL (and
any request whose `Accept` names `text/event-stream`) entirely: those requests go to the network
only and are never cached. Navigations are network-first with a cached shell fallback, and other same-origin GET
shell/assets are cache-first with a background network update. Dynamic session and capsule
navigations are therefore read live and only fall back to the cached session or board shell when
the daemon is unreachable. API responses, SSE frames and event data are never cached as
authoritative data; the pages keep their existing error states offline. Offline data entry is out
of scope.

Every page shell links `/manifest.webmanifest`, carries
`<meta name="theme-color" content="#16855b">` and registers `/sw.js` at load time inside
`if ('serviceWorker' in navigator)`. There is no push-notification code.

There is no Node or TypeScript build step and no browser test:
`crates/vowp-daemon/tests/board_page.rs`, `session_page.rs`, `capsule_page.rs`,
`cost_inbox_pages.rs`, `nodes_page.rs` and `pwa.rs` are HTTP-only tests over a temporary home and
a real `127.0.0.1:0` listener, and `tests/sse.rs` reads real chunked stream bodies. The cost-page
test pins the inline chart container and its accessible label, the selector and five-second
refresh wiring, the unknown-row and empty-report paths, and the no-external-library rule, then
reads `/api/v1/cost` in the same test to prove the chart's data source includes known rows and a
null USD row. The board-page test pins the stream path, the `board` and `heartbeat` handlers, the
"Reconnecting..." text and the polling fallback without running a browser. The session-page test pins the stream path, the `after_seq` resume, the
`session` and `heartbeat` handlers, the reconnect text and the polling fallback. It also pins the
session-detail path, the capsule field labels, the lineage links, the "View capsule" link and the
no-capsule branch, and fetches `/api/v1/sessions/{id}` in the same test to prove the panel's data
source. It also pins the switch route, the `Authorization: Bearer` header, the shared
`vowp-api-token` storage key, the force warning and the visible state/error line, and proves the
page and its API response never contain the token value or home path. The capsule-page test pins
the capsule API path, the metadata and section labels, the
session back link, the `<noscript>` fallback and the absence of paths or secrets, fetches
`/api/v1/sessions/{id}/capsules/{capsuleId}` in the same test to prove the page's data source, and
checks that an unknown session or capsule is the API's `404` behind the page's not-found text. The
inbox-page test pins the answer form, the `Authorization: Bearer` header, the `sessionStorage`
unlock, the queued message, the `401` and `503` lines, and the absence of a token in the URL; the
answer API test proves the write itself. The nodes-page test pins the `/api/v1/nodes` fetch, the
node column labels, the empty and failed-fetch messages and the `<noscript>` fallback, and reads
the same endpoint to prove the page's data source is a schema-valid `NodePage` of the stored
local and remote nodes. The PWA test parses the manifest, pins the three asset content types and
the service-worker headers, checks the bounded `SHELL` list and the API/SSE bypass, and asserts
every page shell links and registers the service worker without printing a home path or token.
The CLI `up` tests pin the printed window URL and the actual-port
`vowp tunnel open window --remote-port <port>` hint, and prove a missing daemon warns without
printing a hint.

Still later: the reconnect UI on the pages that still poll, cross-node switching, dismiss and the
other write actions, and a token rotation service.
Offline data entry, offline event data and push notifications remain out of scope. Markdown
rendering, capsule editing and capsule writes remain out of scope. The API routes and their
fallback are otherwise unchanged; unknown non-API paths still return the API's JSON 404.

The app must never print a token value, a home path or a provider secret; the board only contains the
fields already shown by `vowp board`, and the session pages only the fields already shown by
`vowp session show` and `vowp session tail`.
