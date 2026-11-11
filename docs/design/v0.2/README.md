# v0.2 design notes — one home, many surfaces

Status: current for v0.2. **`home-and-surfaces.md` is the normative, implementable specification
for v0.2** (plan §3–§10): schemas, Rust interfaces, interface hooks, CLI surface, snapshot schema,
named tests and open assumptions. The other notes here are either subordinate to it or explicitly
deferred.

v0.2 is one **home** (a VPS) that owns one global DB and one DB per project, one node supervisor,
all session logs, capacity management, leases and surfaces. People talk to it through the opencode
and Claude apps, through `vowp mcp`, or through `vowp chat`; a control machine holds only a snapshot
cache and SSH tunnels. There is no public HTTP/SSE API or public port in v0.2; the only listener is
the loopback-only secret paste page (and the app server joined through an SSH tunnel).

## The notes

| Note | Status | What it covers |
|---|---|---|
| [`home-and-surfaces.md`](home-and-surfaces.md) | **normative for v0.2** | The whole v0.2 spec: data model + DDL, migration, node supervisor, snapshot/fleet, capacity, remote execution, leases/takeover, app hosts/MCP/chat, control machine, secrets, lifecycle, setup, all new CLI commands, interface hooks, named tests and open assumptions. |
| [`cross-node-sync.md`](cross-node-sync.md) | updated for v0.2 | The v0.1 bundle move. v0.2's remote execution (§7.2 of the spec) supersedes the "move a session to another node" slice: the session now stays on the home and only the executor moves. The remaining directory-sync and same-id questions are v0.3. |
| [`acp-adapter.md`](acp-adapter.md) | deferred, not v0.2 delivery | An Agent Client Protocol adapter behind the existing `Adapter` trait. Design only. |
| [`cost-dashboard.md`](cost-dashboard.md) | deferred, not v0.2 delivery | A cost dashboard on the v0.3 daemon API. Design only. |

The daemon API, the web/PWA window and the phone browser board now live under
[`../v0.3/`](../v0.3/README.md); the ACP adapter and cost dashboard stay here as deferred design
notes. v0.2 covers the phone with an SSH app and `vowp chat`, so a browser board is not needed for
this release.

## Shared assumptions (binding for v0.2)

1. **The home is the only writer.** Control machines, apps and MCP servers send `vowp` commands to
   the home; they never open a DB file. The one exception is a control machine's own snapshot
   cache and SSH spool.
2. **The session log stays v0.1.** `log.jsonl` is append-only, one `EventEnvelope` per line, `seq`
   gapless, capsules and switch unchanged. v0.2 adds a transport, a lease and app mirrors, not a
   second session model.
3. **The database is the decision log; files stay the content.** The global DB and the project DBs
   are the same `Store` abstraction with one project attached at a time.
4. **No cross-database foreign keys.** SQLite cannot enforce them across attached files. Ids that
   name the other DB are plain text, validated by `vowp-store` and repaired by the startup
   reconcile. The full list is in the spec §3.6.
5. **No unqualified `inbox`/`ledger`.** Because both schemas had those names, the node-level tables
   are renamed `node_inbox`/`node_ledger`. The spec withdraws the plan's "v0.1 queries work
   unchanged" sentence and has the store name its database in every statement; the public `Store`
   API stays source-compatible.
6. **Blocking core stays blocking.** No async runtime in `vowp-core`. `vowp mcp`, the secret paste
   page and the tunnel supervisor use plain threads and `std::net`.
7. **Tests are hermetic.** A temporary `VOWP_HOME`, a private `TMUX_TMPDIR` **and** `tmux -L
   <unique>`, unset `TMUX`/`TMUX_PANE`, fake adapters, fake SSH and fake apps. Never the operator's
   `~/.vowp`, the default tmux server, the network or a real harness.
8. **Linux and macOS stay supported.** Linux-only measurement is `cfg`-gated or faked; the whole
   workspace passes on both.

## Reading order

Read the notes in this order:

1. This index for the scope, the shared assumptions and the out-of-scope list.
2. [`home-and-surfaces.md`](home-and-surfaces.md) — the normative v0.2 specification: data model,
   supervisor, snapshot, capacity, sessions and surfaces, control machine, secrets, lifecycle and
   setup.
3. [`cross-node-sync.md`](cross-node-sync.md) — the v0.1 bundle move and what v0.3 leaves open.
4. [`acp-adapter.md`](acp-adapter.md) and [`cost-dashboard.md`](cost-dashboard.md) — the deferred
   design notes.

The v0.1 design under [`../`](../README.md) remains the base for types, schema, sessions and CLI.

## Out of scope for v0.2

- More than one home, replication or a multi-VPS scheduler.
- A listening HTTP/SSE API, a web window or a phone browser board (v0.3); the ACP adapter and
  cost dashboard are deferred design notes, not v0.2 delivery.
- Multi-user accounts, quotas, roles or audit.
- Windows support or a native desktop app.
- A VOWP-specific chat app: the product surfaces are the opencode and Claude apps, with `vowp chat`
  as the fallback.

## See also

- [`../README.md`](../README.md) — the v0.1 implementation spec index (types, schema, crates).
- [`../architecture.md`](../architecture.md) — components, on-disk layout and compatibility rules.
- [`../sessions.md`](../sessions.md) — the log, capsule, switch and bundle rules v0.2 reuses.
- [`../cli.md`](../cli.md) — every v0.1 command and exit code; v0.2 adds the commands in the spec §12.
- [`../v0.3/README.md`](../v0.3/README.md) — the daemon and browser notes moved out of v0.2; the ACP and cost notes stay here as deferred.
