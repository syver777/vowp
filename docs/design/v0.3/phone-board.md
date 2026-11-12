# v0.3 — phone-friendly board

Status: the read-only card board is implemented in v0.3 by `vowp-daemon` at `GET /board` (the
same shell is also at `/`). The PWA shell, session detail and inbox answer are implemented; push
notifications remain later. A phone reaches the board through the same SSH tunnel as the web
window, and the rest of this note is the design that led there.

## Goal

One screen that answers "is the system healthy?" from a phone in a few seconds:

- Jobs by state as compact cards, with the running jobs first.
- For each running job: id, executor, age and seconds since the last event.
- Slots busy/total; planner and keeper executors.
- Active limits with reset times; open inbox count; cost today.
- A job card opens a read-only detail sheet: recent events, gate results and the latest capsule
  (later slice).
- An inbox item can be answered (the existing `inbox answer` request), because a stalled question is
  the one thing that should not wait for a laptop (later slice; the first board is read-only).

The page is the same web app at a narrow viewport, or a dedicated route if that is simpler. It uses
the board and inbox endpoints from [`http-api.md`](http-api.md); the data must match
`vowp board --json`.

## Non-goals

- Full session chat or a terminal on the phone.
- Creating or editing jobs, editing config, or promoting a release.
- Push notifications in the first slice; the responsive card board and a visible open-inbox badge
  come first.
- A native iOS/Android app.

## Existing crates it builds on

- [`web-window.md`](web-window.md) — app shell and routing; the PWA manifest there is still planned.
- `vowp-core::ledger::BoardSnapshot` — the exact fields to show.
- `vowp_store::Store::list_inbox` and `Store::answer_inbox` — `answer_inbox` is the first write once
  writes are added; the first slice is read-only and does not answer inbox items.
- `vowp-protocol::InboxItem`, `Job`, `Run`, `Limit`, `Cost`.

## Design notes

- Use cards, not a wide table. A card shows state as text as well as colour; colour alone is not
  accessible.
- Poll `/api/v1/board` every 5 seconds first. Switch to SSE for running-job events only after the
  polling version is correct; the phone may sleep, so every reconnect must re-fetch the snapshot.
- Never show a full session log on the board; link to the session view and keep the card small.
- The token unlock and 401 flow is shared with the web window when the auth token exists; the first
  read-only board slice has neither a token nor a write action.
- A "last updated" timestamp makes stale data obvious when the phone was offline.

## Open questions

1. **Polling vs SSE.** Polling is simpler and robust to phone sleep; SSE gives live last-event ages.
   A hybrid (poll the snapshot, SSE for the event ticker) is likely.
2. **PWA or plain page.** A manifest and service worker help with install and icon, but a cached shell
   can show stale data. The first slice ships just the responsive page; installability stays later.
3. **Notifications.** A later slice could send a Web Push/ntfy message when an inbox item opens. That
   needs a notification service and a user decision; do not fake it with a poll badge.
4. **Approvals.** Is answering an approval from a phone acceptable for every inbox kind, or should
   approval items require a re-auth/confirmation step?

## First slice (implemented)

`vowp-daemon` serves the read-only board at `GET /board`; the same shell is an alias at `GET /`.
The page fetches `GET /api/v1/board` on load and every five seconds, and renders:

- job counts as chips with both the count and the state name;
- running jobs first, as cards with job id, executor, age and last-event age;
- slots busy/total, planner and keeper;
- active limits as cards with provider, kind and reset time;
- today's cost (`unknown` when the API reports null), the open-inbox badge,
  the last checkpoint and a visible last-updated time.

The card layout has `max-width: 360px` rules and wraps long ids and executors, and there is no
horizontal table, so the board fits a 360 px viewport. Text labels accompany every state; colour is
never the only signal. A failed fetch shows an error line and keeps the last board and its timestamp
visible. With JavaScript disabled, `<noscript>` says the board requires JavaScript. The page never
embeds a home path, a token or a provider secret.

The implementation is one hand-written `crates/vowp-daemon/web/index.html` embedded with
`include_str!`, with no build step. The HTTP-only test is
`crates/vowp-daemon/tests/phone_board.rs`: it starts a real `127.0.0.1:0` listener over a temporary
home and asserts the card markup, the field labels, the `<noscript>` text, the narrow-viewport rule,
and the fixture's running job, limit reset and unknown cost from the same API.

## Reaching it from a phone (implemented)

On the home, `vowp up` prints the loopback window URL and the control-machine command for the
actual daemon port:

```text
web window: http://127.0.0.1:54321/
remote window: vowp tunnel open window --remote-port 54321
  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)
```

With the home configured as a control machine (`vowp control init --name home
<home-ssh-target>`), run the printed `vowp tunnel open window --remote-port 54321` on the phone
or laptop and open the local loopback URL it prints. `/board` is the same PWA shell as `/`, and
its 360 px card layout is the phone view. The daemon still binds `127.0.0.1` on the home; the
existing SSH forward is the only path, no public port is opened, and the firewall allows only
SSH. Pass `--local-port <n>` when a fixed local URL is wanted; otherwise the command prints the
chosen port.

Reads are token-free. When an inbox answer or session switch needs a write, enter
`VOWP_API_TOKEN` in the page's unlock panel; the page keeps it in `sessionStorage`, never in the
URL, and clears it on `401`. The value is the one the daemon inherited from the home's `vowp up`
environment. The PWA shell caches static pages only and bypasses `/api/` and `/stream` requests,
so board data is never served from the cache as authoritative. Push notifications remain later.

Still later: push notifications and any native app. The PWA manifest, service worker, session
detail page and inbox answer are implemented; the board's first slice stays read-only, and
`GET /api/v1/board` and its schema are unchanged.
