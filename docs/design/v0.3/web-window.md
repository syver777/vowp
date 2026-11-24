# v0.3 — web / PWA window

Status: moved from `docs/design/v0.2/` by the v0.2 spec; planned for v0.3, not v0.2. The static
board, session, capsule, cost, inbox, nodes, jobs and job-detail pages, the cost chart, the
token-gated inbox answer, local session switch, job retry/park/cancel and the four keeper
actions, the board, jobs-list, job-detail, cost, inbox and nodes live streams, and the PWA
manifest/service-worker
shell are implemented. `vowp up` starts the serving `vowp-daemon` as a separate child process,
prints the window address and the actual-port `vowp tunnel open window` hint, and the rest of this
note is design. v0.1 has no browser UI and no listening socket.

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
- **Jobs** — every job in the served project with its state and the fields an operator needs,
  plus token-gated retry, park and cancel controls, and a detail view with the job's runs and
  gate results and the token-gated keeper actions (nudge, restart, replan, fail).

The board uses the same JSON shape as `vowp board --json`; the session list and event timeline use
the API from [`http-api.md`](http-api.md). Reads are token-free. The browser writes are answering an
inbox item through the existing request queue, switching the local executor on the session detail
page through the same `SessionService` switch as the CLI (the queue has no switch variant),
retrying, parking or cancelling a job from the jobs page through the queued job-action routes, and
nudging, restarting, replanning or failing a job from the detail page through the queued keeper
routes. They all sit behind the daemon bearer token. The switch is local-node only, requires the
token, and shows the drain and its blocked result.

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
  `/sessions/:id/capsules/:capsuleId`, `/nodes`, `/cost`, `/inbox`, `/jobs` and `/jobs/:id`.
  All are implemented, with `/nodes` a read-only node list and no node mutation or SSH path;
  `/jobs/:id` is the job detail page with the four token-gated keeper controls.
- PWA manifest and a small hand-written service worker; cache only the static shell, never cache
  event data as authoritative.
- The write token is kept in `sessionStorage`, never in a URL. The inbox, session switch, jobs and
  job detail pages show a one-field unlock panel when no token is stored; a `401` clears it and shows
  the panel again. Rotating the token means restarting the daemon with a new `VOWP_API_TOKEN`, then
  unlocking again.
- SSE with `after_seq` resume and a visible "reconnecting" state; fall back to polling if SSE fails.
  The session timeline uses the per-session stream with `after_seq` resume. The board uses the
  snapshot `/api/v1/board/stream` with reconnect and polling fallback. The jobs list and job
  detail use the snapshot `/api/v1/jobs/stream` and `/api/v1/jobs/{id}/stream` streams with the
  same reconnect and polling fallback, and reconcile immediately after a keeper action. The
  cost, inbox and nodes pages use `/api/v1/cost/stream`, `/api/v1/inbox/stream` and
  `/api/v1/nodes/stream` with the same reconnect and polling fallback. The whole live-window
  contract is in [`v0.5/live-streams.md`](../v0.5/live-streams.md).

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
`<noscript>` message. It links to `/jobs`, `/sessions`, `/cost` and `/inbox`.

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
`sessionStorage` and only the inbox answer, session switch, ASIF import, jobs action and keeper
action writes use it. The
value is never put in a URL, logged or returned by the API. Rotating the token means restarting
the daemon with a new value and unlocking again.

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
shows the disabled line; it is never put in the URL. The sessions nav also links the separate
`/import` page. The detail page carries an "Export ASIF" card with a plain
`<a download>` link to `/api/v1/sessions/{id}/export/asif` and a "Preview declared losses"
button that fetches `?report=1` and shows either `N losses: kind, kind…` or
"No declared losses.". The card says host paths are scrubbed but exact secrets are not
detected, points to the CLI `vowp session export --format asif --redact-file <file>` for a
specific redaction list, and says the export is semantic interchange with declared losses, not
lossless. A failed preview leaves the download link usable. The page carries
`<noscript>The session view requires JavaScript.</noscript>`, links back to the board and to
cost and inbox, and never prints the token value, the home path or a provider secret.

The ASIF import page is `crates/vowp-daemon/web/import.html`, served at `GET /import`. It uses
the same token unlock and `sessionStorage` key as the session page, a file input accepting
`.asif.zip,application/zip`, an optional branch text field and a submit button. The page
checks the 272 MiB archive size in the browser, posts the raw file with an explicit
`Content-Type: application/zip`, `Authorization: Bearer` and no multipart form or filename,
and shows the new session link, the event and loss counts, the optional warnings and the
"not continued automatically" note. A `401` clears the token, a `413` names the limit, a
`400` shows the API hint and a `503` shows the writes-disabled line. It loads no external
script, font or image, and it links back to the sessions page.

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
`GET /api/v1/cost?by=<group>` on load, then follows
`EventSource('/api/v1/cost/stream?by=<group>')`; each `cost` frame is the exact `CostReport`
and replaces the table and chart. The selector offers job, day, role,
provider and model (default job), and the URL keeps the chosen value; changing it reopens the
stream with the new query. A `heartbeat` frame updates only the visible live line. The page
renders the grouped
rows and the total with key, runs, USD, token usage, source and unknown-entry count. Above the
table, a responsive inline CSS bar chart shows the known USD for the selected grouping as
horizontal bars labelled with the row keys and scaled to the largest known row; the figure carries
a visible caption and a `role="img"` label/summary for assistive technology. A row whose USD is
null is never drawn as zero or included in the scale: the chart lists those keys under "Unknown USD
(not charted)" and the table keeps showing `?`. The chart is rebuilt from the same report when the
selector changes and on every stream frame or five-second fallback refresh, while the table
remains the full accessible data
view. A null USD total is shown as `?` and never as zero, and the page states that unknown costs
stay unknown. A failed fetch or stream shows an error line or a visible "Reconnecting... polling
every 5 seconds." state and keeps the last report, chart and timestamp
visible; an empty report shows "No cost entries." in the chart and the table. Price-table editing
remains out of scope. The page links to the board, sessions and inbox, carries
`<noscript>The cost page requires JavaScript.</noscript>`, and never prints a home path, token or
provider secret.

The inbox page is `crates/vowp-daemon/web/inbox.html`, served at `GET /inbox`. It fetches
`GET /api/v1/inbox?state=open` on load, then follows
`EventSource('/api/v1/inbox/stream?state=<state>')` for open, answered and dismissed; each
`inbox` frame replaces the list. The state selector adds all,
answered and dismissed; changing it reopens the stream with the new query, while "all" fetches
the three API states and merges their lists because the API has no single `all` state. The page
renders id, kind, job, title, age, state, options, a short body preview and, for an open item, an
answer form. The form offers a select of the item's options plus a free-text input, or only a
text input when the item has no options; the Answer button posts the chosen or typed text to
`POST /api/v1/inbox/{id}/answer` as JSON. A token unlock panel stores the `VOWP_API_TOKEN` value
in `sessionStorage` (never the URL) and hides once stored. A `401` clears the stored value,
shows the unlock panel and leaves the list visible; `503` shows the writes-disabled message;
`202` shows "Answer queued; the supervisor applies it on the next tick" and refreshes the list;
any other error shows a line and keeps the list. Reads continue without a token. A failed fetch
or stream shows an error line or a visible "Reconnecting... polling every 5 seconds." state and
keeps the last list and timestamp visible; an empty list shows "No inbox
items." There is no dismiss or other write action. The page links to the other pages, carries
`<noscript>The inbox page requires JavaScript.</noscript>`, and never prints a home path, token
value or provider secret.

The nodes page is `crates/vowp-daemon/web/nodes.html`, served at `GET /nodes`. It fetches
`GET /api/v1/nodes` on load, then follows `EventSource('/api/v1/nodes/stream')`; each `nodes`
frame replaces the table. It renders a table of name, SSH target (or
`-`), local/remote, last seen, version from `capabilities.vowp` and a compact capabilities/probe
summary built only from the stored fields. It shows "No nodes." for an empty list; a failed
fetch or stream shows an error line or a visible "Reconnecting... polling every 5 seconds." state
and keeps the last list and timestamp visible. The page is read-only: it has
no add, remove or probe control and no SSH, exec or write path. It links to the board, sessions,
cost and inbox, carries `<noscript>The nodes page requires JavaScript.</noscript>`, and never
prints a home path, token or provider secret.

The jobs page is `crates/vowp-daemon/web/jobs.html`, served at `GET /jobs`. It fetches
`GET /api/v1/jobs` on load, then opens `EventSource('/api/v1/jobs/stream')` and renders each
`jobs` frame's exact `JobPage`; every `jobs` frame replaces the table. A `heartbeat` frame
updates only the live line. On a stream error or close the page shows "Reconnecting... polling
every 5 seconds.", starts its five-second `refresh()` fallback, retries the stream with bounded
exponential backoff and stops the fallback when a stream open or a `jobs` frame succeeds. If
`EventSource` is unavailable the page polls only. The page fetches again immediately after an
action. It renders a table of job
id, state, kind, role, priority, needs, attempts, updated time and note. Each job id links to
the detail page at `/jobs/{id}`. A long list scrolls
horizontally inside its own table wrapper, so the page stays usable on a narrow screen. Reads are
token-free; a failed fetch shows an error line and keeps the last list and its "last updated" time
visible, and an empty list shows "No jobs." The page carries the shared unlock panel: the
`VOWP_API_TOKEN` value lives in `sessionStorage` (never the URL), a `401` clears it and shows the
panel again, and a daemon started without a token shows the disabled `503` line. Actions appear
only after unlock and only when the state allows them: retry for `failed`, `stalled` and `parked`;
park and cancel for every non-terminal state; no actions for `done`, `cancelled` or `replanned`.
Retry and cancel post to `/api/v1/jobs/{id}/retry` and `/api/v1/jobs/{id}/cancel`; park asks for an
optional reason and posts `{"reason":"<text>"}` to `/api/v1/jobs/{id}/park`, and cancel asks for
confirmation first. Every action sends `Authorization: Bearer`, shows the queued `202` outcome,
refreshes the list immediately and renders the API error message on `4xx`/`5xx`. The page is a
consumer only: it never writes a job row and adds no API route. It links to the other pages,
carries `<noscript>The jobs page requires JavaScript.</noscript>`, and never prints a home path,
token value or provider secret.

The job-detail page is `crates/vowp-daemon/web/job.html`, served at `GET /jobs/{id}`. It
reads the id from its own path and fetches `GET /api/v1/jobs/{id}`,
`GET /api/v1/jobs/{id}/runs` and `GET /api/v1/jobs/{id}/gates` on load, then opens
`EventSource('/api/v1/jobs/{id}/stream')`. Each `job` frame is a `JobDetailPage` whose `job`,
`runs` and `gates` members replace the summary, runs table and gates table together. A
`heartbeat` frame updates only the live line. On a stream error or close the page shows
"Reconnecting... polling every 5 seconds.", starts its five-second reload fallback, retries the
stream with bounded exponential backoff and stops the fallback when a stream open or a `job`
frame succeeds. If `EventSource` is unavailable the page polls only. It renders the job
id, state, kind, role, priority, attempts, updated time, needs and note; a runs table with
run id, role, purpose, executor, provider, node, started, ended, exit, outcome and limit; and
a gates table with attempt, gate index, kind, passed, duration and timestamp. The job id
links to its sessions at `/sessions?job=<id>`, and the page links back to `/jobs` and keeps
the shared nav and a visible "last updated" line. A `404` for an unknown job shows "Job not
found." while keeping the nav and the back link; the page does not start a stream for that
id. The page carries the shared unlock panel:
the `VOWP_API_TOKEN` value lives in `sessionStorage` (never the URL), a `401` clears it and
shows the panel again, and a daemon started without a token shows the disabled `503` line.
Keeper controls appear only after unlock and only when the graph's state table allows them:
nudge and restart for `stalled`; replan for `stalled`, `parked` and `failed`; fail for
`stalled` with a required reason prompt. There is no control for `done`, `cancelled` or
`replanned`. Every control posts to `/api/v1/jobs/{id}/nudge`, `/restart`, `/replan` or
`/fail` with `Authorization: Bearer`, shows the queued `202` outcome, reloads the job and
its runs and gates immediately, and renders the API error message on `4xx`/`5xx`; fail posts
`{"reason":"<text>"}`. The page never writes a job row itself. It never renders the stored
`worktree`, `brief`, `owns`, `cwd` or `window` fields, a home path, a token value or a
provider secret. It carries `<noscript>The job page requires JavaScript.</noscript>`.
Browser chat remains later work.

## PWA shell (implemented)

`vowp-daemon` serves the installable shell assets, embedded with `include_str!` like the pages:

- `GET /manifest.webmanifest` (`application/manifest+json`) names the app, sets
  `start_url` and `scope` to `/`, uses `display: standalone`, theme colour `#16855b`, background
  colour `#f6f5f1` and one `image/svg+xml` icon at `/icon.svg`.
- `GET /icon.svg` (`image/svg+xml`) is a static SVG mark.
- `GET /sw.js` (`text/javascript`) is sent with `Cache-Control: no-cache` and
  `Service-Worker-Allowed: /`, so it can control the root scope and updates are revalidated.

The service worker is hand-written with no build step. On install it opens cache
`vowp-shell-v4` and caches a bounded `SHELL` list: `/`, `/board`, `/jobs`, `/sessions`,
`/import`, `/cost`, `/inbox`, `/nodes`, `/manifest.webmanifest` and `/icon.svg`. The version
was bumped from v3 when the ASIF import page joined the shell, so existing clients replace
the older shell; on activate the worker
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
`cost_inbox_pages.rs`, `nodes_page.rs`, `jobs_page.rs`, `job_detail.rs` and `pwa.rs` are HTTP-only tests over a
temporary home and a real `127.0.0.1:0` listener, and `tests/sse.rs` reads real chunked stream
bodies. The cost-page test pins the inline chart container and its accessible label, the selector
and five-second refresh wiring, the `/api/v1/cost/stream` subscription with the `cost` and
`heartbeat` handlers, the "Reconnecting" state and bounded-retry constants, the unknown-row and
empty-report paths, and the
no-external-library rule, then
reads `/api/v1/cost` in the same test to prove the chart's data source includes known rows and a
null USD row. The board-page test pins the stream path, the `board` and `heartbeat` handlers, the
"Reconnecting..." text and the polling fallback without running a browser. The session-page test pins the stream path, the `after_seq` resume, the
`session` and `heartbeat` handlers, the reconnect text and the polling fallback. It also pins the
session-detail path, the capsule field labels, the lineage links, the "View capsule" link and the
no-capsule branch, and fetches `/api/v1/sessions/{id}` in the same test to prove the panel's data
source. It also pins the switch route, the `Authorization: Bearer` header, the shared
`vowp-api-token` storage key, the force warning and the visible state/error line, and proves the
page and its API response never contain the token value or home path. The ASIF download test
compares the streamed package with a direct core export, pins the attachment and loss headers,
validates the report preview, allows a session outside the served project and pins the
`404`/`409`/`413` paths with no leftover temp directory. The ASIF import test builds synthetic
packages, pins the token-before-body checks, content types, the capped upload removal, the
idle session and `Location`, branch selection, duplicate-id conflict and the absence of
uploaded bytes, filenames or temp paths from every response; the import-page test pins the raw
zip body, Bearer header, browser size check, `401` clearing and the no-filename rule. The capsule-page test pins
the capsule API path, the metadata and section labels, the
session back link, the `<noscript>` fallback and the absence of paths or secrets, fetches
`/api/v1/sessions/{id}/capsules/{capsuleId}` in the same test to prove the page's data source, and
checks that an unknown session or capsule is the API's `404` behind the page's not-found text. The
inbox-page test pins the answer form, the `Authorization: Bearer` header, the `sessionStorage`
unlock, the queued message, the `401` and `503` lines, the absence of a token in the URL, the
`/api/v1/inbox/stream` subscription with the state query, the `inbox` and `heartbeat` handlers,
the "Reconnecting" state and the five-second polling fallback; the
answer API test proves the write itself. The nodes-page test pins the `/api/v1/nodes` fetch, the
`/api/v1/nodes/stream` subscription with the `nodes` and `heartbeat` handlers, the read-only page
with no add, remove or probe control, the node column labels, the empty and failed-fetch messages
and the `<noscript>` fallback, and reads
the same endpoint to prove the page's data source is a schema-valid `NodePage` of the stored
local and remote nodes. Both page tests pin the service-worker `/api/` and `/stream` bypass. The jobs-page test pins the `/api/v1/jobs` fetch, the
`/api/v1/jobs/stream` subscription, the `jobs` and `heartbeat` handlers, the five-second
polling fallback and reconnect text, and the immediate after-action refresh alongside the job
column labels, the state-to-action mapping (retry only for failed/stalled/parked, park and
cancel for non-terminal states, none for terminal states), the cancel confirmation, the
`Authorization: Bearer` write header, the shared `sessionStorage` unlock panel, the queued
`202` and `401`/`503` paths and the `<noscript>` fallback; it also reads the same endpoint to
prove the page's data source is a schema-valid `JobPage` and proves the existing retry, park
and cancel routes still answer. The job-detail-page test pins the `/jobs/{id}` shell, the
`/api/v1/jobs/{id}`, `/runs` and `/gates` fetches, the `/api/v1/jobs/{id}/stream`
subscription, the `job` and `heartbeat` handlers, the five-second polling fallback and
reconnect text, the job and runs/gates column labels, the `/sessions?job=` and `/jobs` links,
the not-found text, the four keeper controls, the shared unlock panel, the exact state lists
(nudge/restart/fail only for `stalled`, replan also for `parked` and `failed`, none for
terminal states), the `Authorization: Bearer` write, the queued `202` immediate reload, the
`401` and `503` paths, and the absence of a home path, token value or the `worktree`, `brief`,
`owns`, `cwd` and `window` fields; it reads the same three endpoints to prove the shell's data
sources, and the job-detail API test pins the single `Job`, the `400` for an invalid id, the
`404` for an unknown job, the run order (`started_at`, then `id`) and the newest-attempt gate
order. The jobs stream tests separately pin the first and changed `jobs` frames against the
`JobPage` schema, the first and changed `job` frames against the `Job`, `RunPage` and
`GateResultPage` schemas after a state, run and gate change, heartbeats, the `no-store` header,
the `400` for an invalid filter or id and the `404` for an unknown job before streaming. The
cost, inbox and nodes stream tests pin the first and changed `cost`, `inbox` and `nodes` frames
against their exported schemas, the state filter, changed-only frames, one whole-page frame
instead of per-row events, heartbeats, the `no-store` header, the `400` for an invalid state and
a clean close after a store error. The
PWA test parses the manifest, pins the three asset content
types and the service-worker headers, checks the bounded `SHELL` list and the API/SSE bypass, and
asserts every page shell links and registers the service worker without printing a home path or
token.
The CLI `up` tests pin the printed window URL and the actual-port
`vowp tunnel open window --remote-port <port>` hint, and prove a missing daemon warns without
printing a hint.

Still later: browser chat, cross-node switching, dismiss and the other write endpoints, and
a token rotation service. Offline data entry, offline event data and push notifications remain
out of scope. Markdown rendering, capsule editing and capsule writes remain out of scope. The
API routes and their fallback are otherwise unchanged; unknown non-API paths still return the
API's JSON 404.

The app must never print a token value, a home path or a provider secret; the board only contains the
fields already shown by `vowp board`, and the session pages only the fields already shown by
`vowp session show` and `vowp session tail`.
