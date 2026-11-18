# v0.3 design notes — the daemon and the browser

Status: shipped in 0.3.0; the `/jobs` page and the three job-action routes merged after the tag. The
notes mark each implemented slice; the rest is design. These notes were moved here by the v0.2 spec
(`docs/design/v0.2/home-and-surfaces.md`) because v0.2's surfaces are the opencode/Claude apps and
`vowp chat`, and v0.2 opens no **public** HTTP/SSE API or public port. Its only listeners are the
loopback-only secret paste page and the app server joined through an SSH tunnel.

0.3.0 implements the daemon read API and both SSE streams, the token-gated inbox-answer and
local-session-switch writes, the web/PWA window and the phone board. The current tree also carries
the post-tag `/jobs` page and the three token-gated job actions. The 0.3.0 inventory is in
[`CHANGELOG.md`](../../../CHANGELOG.md) and the
[`0.3.0` release notes](../../release/0.3.0.md).

## Why they moved

v0.2 is one home with the apps people already use. The phone needs an SSH app and `vowp chat`, not a
browser board. A loopback HTTP+SSE API, a web/PWA window and a phone browser board would each be a
second client stack before the transport, leases and app surfaces are proven. They were deliberately
sequenced after the v0.2 release; the first browser surface shipped in 0.3.0.

## The notes

| Note | What it covers | Implemented surface |
|---|---|---|
| [`http-api.md`](http-api.md) | A loopback HTTP + SSE API for jobs, sessions, events, board, inbox and cost, with a stable wire contract. | The read endpoints, the per-session and board SSE streams and the five token-gated writes (inbox answer, local switch, job retry/park/cancel). The rest is design. |
| [`web-window.md`](web-window.md) | A web/PWA window: nodes, sessions, session view with an executor switcher and capsule view, and the board. | The board, sessions, capsule, cost, inbox, nodes and jobs pages, the cost chart, the write forms and the PWA shell. The rest is design. |
| [`phone-board.md`](phone-board.md) | A phone-friendly board on the same API. | The responsive `/board` card layout on the same PWA shell. The rest is design. |

`acp-adapter.md` and `cost-dashboard.md` stay in [`../v0.2/`](../v0.2/README.md) as notes whose first
slices shipped in 0.3.0; the remaining slices are design. They are not part of this browser/API
group.

## What v0.3 inherits from v0.2

- The session log, `seq`, capsules, switches and the v0.1 `EventEnvelope` are frozen. The API serves
  the same objects the CLI prints; it does not invent a second model.
- The home is the writer. A daemon must be a read-mostly process on the home; its writes go through
  the same request queue and `SessionOps::switch` as the CLI. No browser action applies a state
  transition directly.
- Leases are the write permission. A browser or app surface without the current lease is read-only,
  with the same takeover notice as `vowp chat` and `vowp mcp`.
- The snapshot document (`vowp-snapshot/1`) is the board's source of truth; the API may serve it
  directly instead of re-deriving the board shape.
- All tests are hermetic: bind `127.0.0.1:0`, a temporary store, a fake clock, no real harness or
  network.

## Dependency order

The v0.2 dependency chain shipped in 0.3.0:

```
v0.2 release
    │
    ▼
http-api.md (implemented in 0.3.0)
    │
    ▼
web-window.md (implemented in 0.3.0) ──► phone-board.md (implemented in 0.3.0)
    │
    └────────► cost-dashboard.md (cost page and API in 0.3.0; later slices design)

acp-adapter.md (first slice in 0.3.0; streaming chunks, permission UI and receipts remain design)
```

The remaining work extends the same API and window; it does not reopen the v0.2 transport decision.

## See also

- [`../v0.2/home-and-surfaces.md`](../v0.2/home-and-surfaces.md) — the v0.2 home, surfaces, leases,
  remote execution and snapshot the v0.3 clients consume.
- [`../v0.2/README.md`](../v0.2/README.md) — the v0.2 index and shared assumptions.
- [`../types.md`](../types.md) — the wire types the API serves.
