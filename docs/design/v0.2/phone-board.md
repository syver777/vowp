# v0.2 — phone-friendly board

Status: current for the commands shown; this note is design only. v0.1 has a terminal board
(`vowp board`) and no browser UI.

## Goal

One screen that answers "is the system healthy?" from a phone in a few seconds:

- Jobs by state as compact cards, with the running jobs first.
- For each running job: id, executor, age and seconds since the last event.
- Slots busy/total; planner and keeper executors.
- Active limits with reset times; open inbox count; cost today.
- A job card opens a read-only detail sheet: recent events, gate results and the latest capsule.
- An inbox item can be answered (the existing `inbox answer` request), because a stalled question is
  the one thing that should not wait for a laptop.

The page is the same web app at a narrow viewport, or a dedicated route if that is simpler. It uses
the board and inbox endpoints from [`http-api.md`](http-api.md); the data must match
`vowp board --json`.

## Non-goals

- Full session chat or a terminal on the phone.
- Creating or editing jobs, editing config, or promoting a release.
- Push notifications in the first slice; the PWA install and a visible open-inbox badge come first.
- A native iOS/Android app.

## Existing crates it builds on

- [`web-window.md`](web-window.md) — app shell, routing and the PWA manifest.
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
- The token unlock and 401 flow is shared with the web window.
- A "last updated" timestamp makes stale data obvious when the phone was offline.

## Open questions

1. **Polling vs SSE.** Polling is simpler and robust to phone sleep; SSE gives live last-event ages.
   A hybrid (poll the snapshot, SSE for the event ticker) is likely.
2. **PWA or plain page.** A manifest and service worker help with install and icon, but a cached shell
   can show stale data. Decide whether the first slice ships installability or just a responsive page.
3. **Notifications.** A later slice could send a Web Push/ntfy message when an inbox item opens. That
   needs a notification service and a user decision; do not fake it with a poll badge.
4. **Approvals.** Is answering an approval from a phone acceptable for every inbox kind, or should
   approval items require a re-auth/confirmation step?

## First-issue-sized slice

Add a `/board` route at a 360 px viewport that fetches `/api/v1/board` and renders the job counts,
running jobs, slots, limits, cost and open-inbox count as cards. Add a fixture test with a temporary
snapshot containing one running job and one limit, and assert the rendered text includes the job id,
executor, reset time and `cost today`. No service worker, no session detail and no inbox answer yet.

The page must remain useful with JavaScript disabled enough to show a "requires JavaScript" message,
and must not embed a home path or a token.
