# v0.2 — web / PWA window

Status: current for the commands shown; this note is design only. v0.1 has no browser UI and no
listening socket.

## Goal

A small installable web app served by the local daemon, usable from the same laptop or over an SSH
tunnel. It should answer the questions an operator asks from a browser without opening a terminal:

- **Nodes** — name, SSH target, last seen, version and probe state.
- **Sessions** — filter by project, job, role and state; show executor, last event and last capsule.
- **Session view** — an event timeline with live updates, lineage, a capsule panel, and an **executor
  switcher** that sends the same switch request as `vowp session switch`.
- **Capsule view** — the latest capsule rendered as Markdown, with its `at_seq`, trigger and executor.
- **Board** — job counts, running jobs, slot occupancy, limits, today's cost and open inbox count.

The board uses the same JSON shape as `vowp board --json`; the session list and event timeline use
the API from [`http-api.md`](http-api.md). The app is read-only in the first slice. When writes are
added, answering an inbox item is the only one that uses the existing request queue, and switching an
executor runs the same `SessionOps::switch` path as the CLI (the queue has no switch variant).

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

- TypeScript, built to static assets and served by the daemon; no runtime Node dependency.
- Routes: `/` (board), `/sessions`, `/sessions/:id`, `/sessions/:id/capsules/:capsuleId`,
  `/nodes`, `/cost`, `/inbox`.
- PWA manifest and a small service worker; cache the shell, never cache event data as authoritative.
- The token is kept in memory or `sessionStorage`, not in a URL. A 401 shows a one-field unlock page.
- SSE with `after_seq` resume and a visible "reconnecting" state; fall back to polling if SSE fails.

## Open questions

1. **Frontend stack.** Plain TypeScript + a small DOM helper is enough for the first slice and avoids a
   build-tool decision. Pick a framework only when the session timeline needs it.
2. **Asset embedding.** Serve `web/dist` from disk in development and embed the built files in the
   binary for release, or always serve from a directory? Embedding keeps one binary but adds a build
   step to `cargo build`.
3. **Write scope.** Is executor switching safe from a browser before a live-input/queue design exists?
   The switch protocol drains an active turn; the UI must show the drain and its `blocked` result.
4. **Mobile vs desktop.** The phone board is a separate note; decide whether it is the same app at a
   narrow width or a dedicated route with less data.

## First-issue-sized slice

Serve a static `index.html` and a read-only board page from the daemon: fetch `/api/v1/board` and
render job counts, running jobs, slots, limits and cost. Add a fixture test that starts the daemon on
`127.0.0.1:0`, opens the board route with a temporary store and asserts the snapshot values. No
sessions page, no SSE and no write action yet.

The app must never print a token value, a home path or a provider secret; the board only contains the
fields already shown by `vowp board`.
