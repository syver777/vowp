# v0.3 design notes — the daemon and the browser

Status: planned for v0.3, **not** part of v0.2. These notes were moved here by the v0.2 spec
(`docs/design/v0.2/home-and-surfaces.md`) because v0.2's surfaces are the opencode/Claude apps and
`vowp chat`, and v0.2 opens no **public** HTTP/SSE API or public port. Its only listeners are the
loopback-only secret paste page and the app server joined through an SSH tunnel.

## Why they moved

v0.2 is one home with the apps people already use. The phone needs an SSH app and `vowp chat`, not a
browser board. A loopback HTTP+SSE API, a web/PWA window and a phone browser board would each be a
second client stack before the transport, leases and app surfaces are proven. They are deliberately
sequenced after the v0.2 release.

## The notes

| Note | What it covers | First slice |
|---|---|---|
| [`http-api.md`](http-api.md) | A loopback HTTP + SSE API for jobs, sessions, events, board, inbox and cost, with a stable wire contract. | Read-only `health`, `jobs`, `sessions` and a per-session event tail. |
| [`web-window.md`](web-window.md) | A web/PWA window: nodes, sessions, session view with an executor switcher and capsule view, and the board. | Static shell and a read-only board page. |
| [`phone-board.md`](phone-board.md) | A phone-friendly board on the same API. | Responsive board cards with polling. |

`acp-adapter.md` and `cost-dashboard.md` stay in [`../v0.2/`](../v0.2/README.md) as deferred design
notes; they are not v0.2 delivery and do not belong to this browser/API group.

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

```
v0.2 release
    │
    ▼
http-api.md ──► web-window.md ──► phone-board.md
    │
    └────────► cost-dashboard.md (v0.2 design note, later slice)

acp-adapter.md (independent after the ACP protocol version is pinned)
```

## See also

- [`../v0.2/home-and-surfaces.md`](../v0.2/home-and-surfaces.md) — the v0.2 home, surfaces, leases,
  remote execution and snapshot the v0.3 clients consume.
- [`../v0.2/README.md`](../v0.2/README.md) — the v0.2 index and shared assumptions.
- [`../types.md`](../types.md) — the wire types the API serves.
