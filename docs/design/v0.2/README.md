# v0.2 design notes — the API, the window and the handoff

Status: current for the commands shown; this directory is design only. It is **not** a release
schedule and none of it is implemented in v0.1. Each note is one screen: goal, non-goals, the
existing crates it builds on, open questions and a first-issue-sized slice.

## Shared assumptions

1. **The v0.1 wire types stay stable.** `EventEnvelope` and its payload types, `Job`, `NewJob`,
   `JobPatch`, `Session`, `Run`, `GateResult`, `Switch`, `Capsule`, `Limit`, `Cost` and
   `LedgerEntry` are already defined once in `vowp-protocol`; `vowp schema` exports the envelope and
   the main domain types. v0.2 adds transport types around them; it does not rename or repurpose
   existing fields.
2. **The database stays the decision log; files stay the content.** The daemon reads the same
   `Store` and session logs as the CLI. It is not a second source of truth.
3. **The daemon is local.** It binds `127.0.0.1` only, uses a per-node token, stores no provider
   credentials, and keeps the SSH path as the remote-access story.
4. **Write actions reuse existing core paths.** `vowp-core::requests::Request` already models retry,
   park, cancel, nudge, restart, replan, fail, inbox answer and loop pause/resume; a switch uses
   `SessionOps::switch` directly. A browser action should call the same path, not invent a second
   state machine.
5. **Blocking core stays blocking.** The v0.1 crates use plain threads and blocking I/O. A new
   `vowp-daemon` crate should own `axum`/`tokio` and depend on `vowp-core`; the core crates should not
   grow an async runtime.
6. **Every proposed endpoint has a fixture test.** Tests use a temporary node home and a fake
   adapter/SSH, never the operator's `~/.vowp`, network, tmux server or real harnesses.

## The notes

| Note | v0.2 item | First issue |
|---|---|---|
| [`http-api.md`](http-api.md) | Daemon HTTP + SSE API and the stable event/job wire contract. | Read-only `health`, `jobs`, `sessions` and per-session event tail. |
| [`web-window.md`](web-window.md) | Web/PWA window: nodes, sessions, session view with executor switcher, capsule view, board. | Static shell and read-only board page. |
| [`phone-board.md`](phone-board.md) | Phone-friendly board on the same API. | Responsive board cards with polling. |
| [`acp-adapter.md`](acp-adapter.md) | Agent Client Protocol adapter. | ACP update mapping against a fake ACP server. |
| [`cross-node-sync.md`](cross-node-sync.md) | Cross-node session sync. | `session pull` built on the v0.1 bundle. |
| [`cost-dashboard.md`](cost-dashboard.md) | Cost dashboard. | JSON endpoint plus a table view. |

The maintainer-ready issue bodies and labels for the roadmap's "Good first issues" section are tracked with the project's issue tracker, not in this directory.

## Dependency order

```
protocol schemas (already stable)
        │
        ▼
   http-api.md ──────► web-window.md ──────► phone-board.md
        │
        ├────────────► cost-dashboard.md
        │
        └────────────► cross-node-sync.md (later slices)

acp-adapter.md (independent after the protocol question is settled)
```

The API comes first because three notes consume it. ACP is independent of the daemon: it is an
adapter behind the existing `Adapter` trait. Cross-node sync starts with the bundle transport that
already exists and only later grows a daemon transport.

## Out of scope for all of v0.2

- Multi-user accounts, quotas, roles and audit.
- A hosted relay or any public listener.
- Replacing the CLI or the tmux views.
- Rewriting the job graph or the supervisor loops.
- Windows support or a native desktop app.

## See also

- [`../README.md`](../README.md) — the v0.1 implementation spec index.
- [`../architecture.md`](../architecture.md) — components, on-disk layout and compatibility rules.
- [`../types.md`](../types.md) — the wire types the API serves.
- [`../../roadmap.md`](../../roadmap.md) — what ships in v0.1 and what comes later.
