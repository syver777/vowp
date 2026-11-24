# v0.4 — the local orchestrator: a local surface and a local node on the home's terms

Status: **draft; decisions accepted (2026-11-13); Phases 0–3 implemented (2026-11-17).** The owner
accepted the transport, result-return, phase-order and offload/secret proposals; §16.1 records them
and the remaining MVP defaults. Where this note conflicts with the v0.2 specification, the v0.2
specification still governs the shipped tree and this note is a proposal.

The **home** is the VPS. It stays the only home: the global DB, every project DB, every session
log, every lease, the snapshot and the supervisor are there. v0.4 adds two things on the owner's
Mac:

1. a **local surface** — the opencode app (and `vowp mcp`, and optionally `vowp chat`) runs on the
   Mac against the local checkout, and its turns are mirrored into the same session log on the
   home under a session lease;
2. a **local node** — an outbound-only `vowp agent` that enrolls once, reports capacity, maps
   project paths per node, receives offloaded jobs, runs them in isolated worktrees, spools every
   event, and returns commits to the home through the project's git remote.

The Mac never opens a VOWP database, never keeps a second session log, and never receives a home
secret value. The home never dials the Mac and, in the MVP, opens no new public port: the agent
dials out over the existing key-only SSH path and speaks a small framed protocol through an SSH
stdio gateway (a forced command in the hardened setup). The same protocol can later run over WSS
through a reverse proxy without changing the semantics.

| Decision | Proposal | Why |
|---|---|---|
| Transport | SSH stdio for the MVP; WSS optional later | no new public listener; reuses key-only SSH |
| Local app turns | attached surface: the app drives, VOWP mirrors | the app is the harness; no brittle interception |
| Offload | opt-in per project/job; home-only for merge, deploy and home secrets | safety first |
| Results | the node pushes `job/<id>` with its own git credentials; the home fetches and verifies | no credential copying |
| Disconnect | spool, continue for a bounded grace, then fence | resilient but bounded |
| Credentials | node key + surface token + short lease token; hashes at rest | least privilege |
| Paths | project-relative `PathRef` on the wire; absolute paths stay local | portability and privacy |

What this is not: a second home, a DB replica, a distributed scheduler, a sandbox, a way to copy
provider logins, or a public API.

On 2026-11-13 the owner accepted the transport, result-return, phase-order and offload/secret
proposals. §16.1 is the decision record; the remaining open questions are recorded as MVP
defaults and can be changed before their phase.

## 1. Goals, non-goals and invariants

### 1.1 Goals

- Let the owner use the Mac's local checkout, local tools, local harness login and local compute
  without forking the session.
- Keep one session log and one lease per session on the home, so the Mac, the home app and the
  phone continue the same conversation.
- Offload eligible jobs to the Mac with explicit placement, capacity and secret rules.
- Make disconnects boring: spool, resume, no gaps, no duplicates, no silent divergence.
- Keep the auth story honest: the Mac gets only the credentials it needs; the home sends names,
  never values.

### 1.2 Non-goals

- A second home, replication, leader election, CRDTs or two supervisors.
- A session log or project DB replica on the Mac.
- Copying SSH private keys, harness logins, git credentials, provider keys or VOWP secret values
  between machines.
- A public read/write API, a public app port, or the home dialing the Mac.
- Sandboxing arbitrary repository code. The Mac's user account (or a separate user/VM) is the
  isolation boundary; VOWP documents this rather than pretending.
- Automatic merge or conflict resolution.
- Multi-user accounts, quotas or roles.
- A phone joining the Mac's loopback app server; the phone continues through the home.
- Windows.

### 1.3 Invariants

These bind if the design is accepted:

1. **One home.** The home is the only writer of the global DB, project DBs and session logs. The
   Mac sends events and requests; it never opens a DB file.
2. **One log.** A local surface appends to the same `sessions/<id>/log.jsonl` through the home.
   Pending events on the Mac are an outbox, not a second log.
3. **One lease.** At most one surface writes a session. The lease holder is the only writer; a
   surface without the lease is read-only.
4. **No credential copying.** Private keys, harness logins, provider credentials and home secret
   values never cross the machine boundary. Names may cross; values do not.
5. **No silent fallback.** A job pinned to a node waits when the node is offline; it does not
   silently run on the home. A home-only job is never offloaded. A local surface never falls back
   to a direct home SSH connection behind the agent's back.
6. **No absolute local paths in shared state.** New session events, run rows and snapshot cards use
   `PathRef`; absolute Mac paths stay on the Mac. The owner-only `node_paths` row is the one
   exception, and it is redacted from logs and snapshots.
7. **Idempotent transport.** Every event has a stable id; the home dedupes by id; a replay after a
   drop has no gaps and no duplicates.
8. **No new public listener in the MVP.** The home's VOWP processes bind loopback or a Unix socket.
   The only public service is the owner's existing SSH daemon. A WSS ingress is a separate,
   opt-in deployment.

## 2. Vocabulary and actors

| Term | Meaning |
|---|---|
| **home** | The VPS that owns the truth: global DB, project DBs, session logs, leases, snapshot and supervisor. |
| **local surface** | The opencode app, `vowp mcp` and `vowp chat` running on the Mac against local files, attached to a session on the home. |
| **local node** | The Mac as an enrolled executor: it runs offloaded jobs for projects it has mapped. |
| **agent** | The `vowp agent` process on the Mac. It hosts the surface bridge, the executor and the local spool/cache. It dials the home; nothing dials it. |
| **gateway** | The home-side process started by the SSH command (a forced command in the hardened setup). It forwards frames between the agent and the supervisor and has no DB access. |
| **node credential** | The ed25519 key pair generated on the Mac at enrollment; the public half is stored on the home. It identifies the node, not a user. |
| **surface token** | A long-lived, scoped secret issued by the home when a surface is paired. It identifies the surface; it is stored hashed on the home and mode 0600 on the Mac. |
| **lease token** | A short-lived secret issued by the home when a surface acquires a session lease. It authorizes the current writer; it is stored hashed on the home and only in memory on the Mac. |
| **path map** | The per-node mapping from a project to its local root and worktree root. |
| **path ref** | A project-relative reference (`Project`, `Worktree`, `Subdir`) that the agent resolves locally; the home never needs the absolute path. |
| **run fence** | The `(run_id, epoch)` pair that makes late frames from an old connection harmless. |
| **offload** | Running an eligible job on a local node instead of the home. |
| **placement** | The policy that decides where a job runs: `home`, `node:<name>` or `any`. |
| **pending spool** | The agent's local outbox of normalized events written before send; replayable and capped. |
| **home-only secret** | A secret value that exists only on the home; a job requiring it is not offloadable. |

The existing vocabulary in [`../README.md`](../README.md) still applies: a *session* is the
universal conversation, an *executor* is `harness:model@effort#variant`, a *surface* is where a
person talks to a session, and a *lease* is one surface's right to write. A local surface and a
local node are additions, not replacements. The existing `orchestrator` **session role** is
unchanged; "local orchestrator" in this note names the Mac-side pair, not a new role.

## 3. Architecture

### 3.1 Processes and connections

```
Mac                                              VPS home
┌──────────────────────────────┐                 ┌──────────────────────────────────┐
│ opencode server (local)      │                 │ sshd (owner's existing)          │
│   ▲ loopback, app password   │                 │   │ forced command (phase 4)     │
│   │                          │                 │   ▼                              │
│ vowp mcp (stdio)             │   outbound SSH  │ vowp agent gateway --stdio       │
│   ▲ Unix socket 0600         │  ────────────►  │   │ Unix socket 0600             │
│   │                          │  ◄────────────  │   ▼                              │
│ vowp agent                   │                 │ supervisor                       │
│   ├ surface bridge           │                 │   ├ global DB / project DBs      │
│   ├ executor                 │                 │   ├ sessions/<id>/log.jsonl      │
│   ├ pending spool            │                 │   ├ leases, snapshot, dispatch   │
│   └ snapshot cache           │                 │   └ offload policy               │
└──────────────────────────────┘                 └──────────────────────────────────┘
```

The agent is one process. It owns one outbound connection to the home. Local clients reach it
over a mode-0600 Unix socket. There is no inbound listener on the Mac and no new public listener
on the home in the MVP.

### 3.2 What runs where

| Thing | Mac | Home | Notes |
|---|---|---|---|
| global DB, project DBs | — | yes | the Mac never opens them |
| `sessions/<id>/log.jsonl` | outbox only | yes | the Mac's spool is pending events, not a log |
| leases, run fences, dispatch | — | yes | the home decides |
| snapshot | read-only cache | source of truth | cache is marked `as of` |
| local checkout | yes | optional | the home may have its own checkout or a bare mirror |
| harness login, provider credentials | yes | separate | never copied |
| git credentials | yes | separate | the node pushes with its own |
| opencode server, MCP stdio | yes | existing home app | the local surface is optional |
| home secrets | — | yes | names may be sent; values never |
| merge queue, promotion, deploy | — | yes | never offloaded |

### 3.3 Why not the v0.2 app host plus a tunnel

The v0.2 app host runs opencode on the home; the desktop joins through a tunnel. That gives the
app the home's checkout, not the Mac's. A reverse tunnel to a Mac-side app server would give local
context, but it creates an inbound surface on the Mac, a second control path, and a second place
where leases must be enforced; it does not solve offloaded jobs. The agent bridge is the smaller,
auditable path: one outbound connection, one framed protocol, the same lease checks on the home.

### 3.4 Components and dependency direction

- `vowp-protocol::agent` — frames, enrollment, `PathRef`, policy types. No I/O.
- `vowp-agent` (new crate) — transport client, spool, surface bridge, executor, path map, local
  config. It depends on `vowp-protocol`, `vowp-adapters` and a shared surface-mapping module; it
  **does not** depend on `vowp-store` or `vowp-core`, so the Mac cannot accidentally open a home
  database.
- `vowp-core::agent` — home-side enrollment, connection registry, dispatch, offload policy, run
  fences and ingest. It depends on protocol and store.
- `vowp-store` — new tables and store methods.
- `vowp-cli` — `node enroll/revoke/rm/show`, `agent ...`, `offload ...` and the hidden
  `agent gateway`.
- The supervisor hosts the agent socket and the connection registry, and reuses `IngestSession`
  for event appends.
- `vowp-daemon` — the existing read-only `/api/v1/nodes` keeps its v0.3 `NodePage` shape; the
  redacted `NodeCard` is a separate CLI/MCP read model. The daemon gains no agent write path.
- A shared surface-mapping module — the pure opencode event mapping currently inside
  `vowp-core::surface::mirror` is extracted so both the home and the agent can compile it. The
  home validates the normalized events; it does not trust the agent's mapping blindly.

The gateway connection path reuses the `session ingest` concurrency model: a per-connection store
handle, SQLite WAL, and the session-log writer lock. It adds no second supervisor and no async
runtime to the core.

Dependency direction stays acyclic:

```
vowp-protocol ──► vowp-adapters ──► vowp-agent        (Mac; no store, no core)
      │
      └──► vowp-store ──► vowp-core ──► vowp-cli        (home)
```

## 4. Identity, tokens and leases

### 4.1 Three credentials, three jobs

| Credential | Holder | Lifetime | Scope | On the home | On the Mac | Revocation |
|---|---|---|---|---|---|---|
| **node key** (ed25519) | agent | long | prove node identity at connect | public key in `node_credentials` | private key `~/.vowp-agent/node.key`, 0600 | `node revoke`; next handshake fails |
| **surface token** | agent, for one surface | 30 days default, rotatable | one surface, one session/project, named tool scopes | SHA-256 hash + prefix in `surface_tokens` | `~/.vowp-agent/surfaces/<id>/token`, 0600 | `surface revoke`; next frame fails |
| **lease token** | agent/bridge | per lease or turn (minutes) | one session, one writer | SHA-256 hash in `leases.token_hash` | memory, and a grant file 0600 when needed | release, takeover, 60 s stale |

For a v0.4 local surface, a write needs a valid surface token **and** the current lease token.
Neither alone is enough. Legacy home surfaces with no `surface_tokens` row keep the v0.2 lease-only
check; pairing is additive, not a forced migration. The node key authenticates the transport; it
does not authorize session writes. The surface token scopes what the surface may ask; the lease
token authorizes the current write.

No credential is ever put in argv, a URL, a log line, a snapshot or a session event.

### 4.2 Node enrollment

1. On the home, the owner runs:

   ```sh
   vowp node enroll mac-studio --label darwin-arm64
   ```

   The home creates a `pending` node and a one-time code (stored hashed, TTL 10 minutes, single
   use), then prints the code and the exact `vowp agent enroll` command. The code is a secret; it
   is shown once. The Mac supplies its project path map at enrollment or later with
   `vowp agent paths add`; the home never chooses a local root.

2. On the Mac:

   ```sh
   vowp agent enroll --target <home-ssh-target> --code-file -
   ```

   `--code-file -` reads the one-time code from stdin, so it never appears in argv or `ps`.
   `--target` defaults to the target in `~/.vowp-control/control.json` when the Mac is already a
   control machine. The agent generates an ed25519 key pair in `~/.vowp-agent/node.key` (0600),
   opens `ssh -T <target> vowp agent gateway --stdio`, and sends `Enroll { code, public_key,
   agent_version, os, arch, labels, capabilities, paths }`.

3. The gateway forwards the frame to the supervisor over the Unix socket. The supervisor verifies
   the code hash, TTL and use, binds the public key to the node, sets `status=active`, and returns
   `Enrolled { home_id, node, protocol, server_time, heartbeat_secs }`. The agent writes
   `~/.vowp-agent/agent.json` (0600). The code is consumed.

4. If `[agent] require_approval = true`, the node stays `pending` until the owner answers an inbox
   item. The agent prints "waiting for approval" and retries. The default is no second approval:
   the owner's `node enroll` is the approval. The inbox path is for a node that self-requests
   enrollment (Phase 3).

5. Re-enrollment needs a new code. An active node is revoked and removed first
   (`vowp node revoke <name> --yes` then `vowp node rm <name> --yes`); its old credentials are
   revoked and its runs are fenced by the revoke. `vowp node enroll <name> --replace` rotates a
   `pending` node's one-time code in one transaction: the old pending code is refused immediately,
   the node stays `pending`, and the new code is printed once. An `active` or `revoked` node is
   refused (exit 4) so a live enrollment is never wiped; revoke it or run
   `vowp node rm <name> --yes` first. `node rm`
   never prompts, accepts `--yes` for scripts, removes the node row and purges its stranded
   enrollment codes; an unknown node is exit 3.

6. Revocation: `vowp node revoke <name>` sets `status=revoked`, revokes every credential, closes the
   connection, fences the node's runs and releases its leases. The agent receives `Revoked {
   reason }` on its next frame, stops, keeps its spool for inspection, and requires a new code. The
   SSH key is not removed automatically; if it was dedicated to VOWP, remove it from
   `authorized_keys`.

The one-time code is never logged; only its hash is stored. The private key never leaves the Mac.
The home never dials the Mac.

### 4.3 Surface pairing and surface tokens

1. On the home, the owner runs:

   ```sh
   vowp surface pair --session <id> --node mac-studio --kind opencode --app opencode
   ```

   The home creates a `pending` surface and a one-time code (TTL 10 minutes), and prints the code
   and the `vowp agent pair` command.

   The reverse flow, where the Mac asks and the home opens an inbox item for approval, is a Phase 3
   improvement. The MVP keeps the code path so there is no unauthenticated self-registration.

2. On the Mac, the owner redeems the code:

   ```sh
   vowp agent pair --code-file -
   ```

   `--code-file -` reads the code from stdin; it is never in argv or `ps`. The agent sends
   `SurfacePair { code, surface_id, session, app }` over the node connection. The
   home verifies the code, the node, the session and the owner, and returns `SurfaceIssued {
   surface_id, token, scopes, expires_at }`. The agent writes the token to
   `~/.vowp-agent/surfaces/<id>/token` (0600) and never prints it.

3. Default scopes are narrow: `session.read:<id>`, `session.write:<id>`, `project.read:<name>`,
   `fleet.read`, and a named subset of MCP tools. A surface cannot enroll nodes, change offload
   policy, read home secrets or read another session.

4. Tokens expire after 30 days by default. `vowp surface rotate <id>` issues a new token with a
   10-minute overlap; the old token expires after the overlap. The home pushes `SurfaceIssued` to a
   live agent; an offline agent must re-pair after the overlap. `vowp surface revoke <id>` revokes
   immediately and pushes `SurfaceRevoked`.

5. The home stores only the hash, a short prefix for display, and the scopes. The app config
   contains the surface id, not the token. The agent holds the token; local clients (`vowp mcp`,
   `vowp chat`) use the mode-0600 agent socket and the surface id, so they never read the token
   file themselves.

6. A surface token is bound to one node. Copying it to another Mac does not work: the home checks
   the surface's node and the connection's node key.

### 4.4 Session leases

The v0.2 lease model is reused: one lease per session, a 5-second heartbeat, a 60-second stale
window, takeover with a drain and a capsule, and a pushed `LeaseRevoked` notice.

What v0.4 adds:

- **Acquire over the agent.** `SurfaceAttach` or `LeaseAcquire { session, surface, mode:
  acquire|takeover }`. The home checks the surface token scope and the existing lease, and returns
  `LeaseGrant { lease_token, expires_at, tail_seq }`. If a live lease exists and no takeover was
  requested, the home returns `Conflict { holder, since }`; the surface asks the owner and retries
  with `takeover`.
- **Heartbeat over the agent.** `LeaseHeartbeat { session, token }` every 5 seconds. The home
  updates `heartbeat_at` with its own clock; the agent's timestamp is informational.
- **Takeover.** `LeaseTakeover { session, surface }` atomically replaces the lease, writes a `note`
  event, writes a `switch` with `reason=takeover` when there is a live run, pushes `LeaseRevoked`
  to the old connection, and drains the old run for up to 5 seconds. The old surface's next write
  is `Refused("this surface is read-only; taken over by …")`; it follows read-only.
- **Disconnect.** The lease goes stale after 60 seconds without a heartbeat; another surface may
  take over. The agent on reconnect receives `LeaseLost` or `Conflict`; its pending surface events
  are discarded with a visible notice, never appended.
- **Release.** `LeaseRelease` on detach deletes the row and writes a `note` event.

The lease is per session, not per run. A **run fence** is separate: it keeps late frames from a
disconnected run harmless even if the session lease has not yet moved.

> **Superseded in part by v0.5.1.** The v0.5.1 ownership model extends this lease row with a mode
> (`interactive` or `job`), an owner epoch and an expiry, so the 60-second stale window above now
> applies to interactive owners only. A job run holds a `job`-mode owner with an expiry renewed by run
> activity; an expired owner fences the run, and an explicit interactive takeover of a running job
> fences it and leaves the job `ready`. A session with no owner refuses an interactive write.
> The text above stays as the v0.4 record; the current rules are in
> [`../v0.5.1/session-ownership.md`](../v0.5.1/session-ownership.md) and
> [home and surfaces](../../concepts/home-and-surfaces.md#who-owns-a-session).

The local app does not need the lease token in its configuration. The agent holds it; MCP tool
calls are authorized by the agent's connection, the surface token and the home's lease check.

### 4.5 What a surface may write

| Surface | Read | Write | Lease | Transport |
|---|---|---|---|---|
| local opencode via agent | its session, its project, fleet | mirrored events; allowed MCP write tools | required; the agent holds it | agent socket → SSH → home |
| local `vowp chat` via agent | its session, its project, fleet | turn input and slash commands | required | agent socket |
| local `vowp mcp` | via agent | allowed MCP write tools | checked by the home | agent socket |
| home opencode / Claude | existing | existing | required | home |
| control machine | cache / SSH | routed to the home | required | SSH |
| `session ingest` | — | event append for one run | grant token | SSH |

A local surface cannot read another session outside its token scope. It cannot enroll nodes,
change policy or read home secrets.

### 4.6 Token storage and comparison

- **Surface token format:** `vowp_surf_` plus 32 random bytes encoded as base64url. The home stores
  SHA-256, a 12-character prefix for display, and the scopes. Comparisons are constant-time. The
  token is never logged, echoed, put in a URL or returned by a read API.
- **Lease token format:** the existing 52-character token (two ULIDs). v0.4 stores
  `leases.token_hash` instead of the plaintext. The plaintext is returned once in the grant; the
  heartbeat and release paths hash the presented token. Existing leases expire within 60 seconds,
  so the upgrade clears them rather than migrating long-lived plaintext.
- **Node key:** ed25519. The home stores the public key. The handshake signs a 32-byte nonce, the
  `home_id` and the `connection_id`; the home verifies. There is no shared node secret.
- Audit rows record the surface id and token prefix, never the token or a secret value.

## 5. The local surface

### 5.1 Attached turns, not VOWP-driven turns

A local app is an **attached surface**: the app starts its own turns, and VOWP mirrors the
normalized events under a session lease. VOWP does not try to wrap every app turn in
`turn-begin`; intercepting another vendor's execution lifecycle is brittle and would force a
plugin or a proxy in front of the app.

The distinction matters:

- **Attached turns** (the local opencode app): the app is the harness. The agent mirrors
  `turn.started`, messages, tool calls, cost and `turn.ended`. The home assigns `seq` and writes
  the rows. The home can refuse to record after a takeover, but it cannot stop the app from
  running; that is the same honest limit as the v0.2 app host.
- **VOWP-driven turns** (offloaded jobs and local `vowp chat`): the home grants a run and the agent
  starts the harness. The home controls the prompt, executor and deadline. This path reuses
  `begin_turn` / `IngestSession`.

If the app emits no execution events, the bridge sends `TurnStarted`/`TurnEnded` control frames
around the mapped events so the home still has a run row for cost and limits. A turn with no
events at all is a `note`, not a fabricated turn.

### 5.2 Attach flow

1. The owner starts or attaches a local app surface:

   ```sh
   vowp agent surface start --app opencode --session <id> --project vowp
   # or attach to an already-running server:
   vowp agent surface attach --app opencode --session <id> --port <p>
   ```

2. The agent resolves the project's local path map. For `start`, it starts the opencode service
   with a private `XDG_STATE_HOME` under `~/.vowp-agent/orchestrators/<name>/`, writes
   `opencode.json` (the MCP server command), `AGENTS.md` and `orchestrator.json`, and prints the
   local pair link. The link is single-use and expires in five minutes; it is a secret and is never
   logged.

3. The agent sends `SurfaceAttach { surface, session, lease: acquire|takeover, app,
   native_session, cwd_ref, backfill }`. The home validates the surface token and lease and returns
   `AttachGrant { lease_token, tail_seq, capsule, tail }`.

4. The agent starts the bridge: it connects to the app's `/api/event` SSE stream (HTTP Basic from
   the local `service.json`), optionally backfills from the app's export endpoint, maps events,
   normalizes paths, redacts local secret values, appends to the pending spool, sends frames to the
   home and consumes acks.

5. A heartbeat thread sends the lease heartbeat every 5 seconds and the node heartbeat every 10.

6. On stop or exit, the agent flushes the spool, sends `SurfaceDetach`, releases the lease, and
   stops the app server if the agent started it.

### 5.3 Event mirroring

The mapping table from the v0.2 specification §7.5 is reused, but it runs on the Mac:

- `session.inbox.enqueued` with a user item → `user.message`;
- `session.text.ended` → one `assistant.message` per completed message (never deltas);
- `session.tool.called` → `tool.call`; `session.tool.success` / `session.tool.failed` →
  `tool.result`;
- `session.usage.updated` → `cost` (tokens and USD; no durable sequence, so never used for
  ordering);
- `session.execution.started` → `turn.started`; `session.execution.succeeded` / `interrupted` →
  `turn.ended`; `session.execution.failed` → `error`, then `turn.ended`;
- `session.model.selected` → `switch` only when the executor/model actually changes, otherwise
  `note`;
- UI-only events (`session.text.started`, `session.text.delta`, `session.step.*`,
  `session.tool.input.*`, `server.connected`, `project.updated`, `mcp.*`) are not session content.

Every mirrored event is paired with a native id. The home inserts `native_events` in the same
transaction as the event index row; a duplicate native id is skipped. The agent also keeps a local `mirror.json`
cursor for backfill efficiency; the home is the authority on duplicates.

**Path normalization.** The bridge maps the local project root and worktree root to a `PathRef`
and replaces other absolute paths with `<external>` before spooling or sending. No absolute
user-home path appears in a session event, run row or snapshot card. This is a deliberate privacy and
portability rule, not only cosmetics.

**Secret redaction.** The bridge loads the values it injected for the run from the local
environment/Keychain, redacts exact values before spooling and sending, and drops the values after
the run. The home runs its own exact-value redaction and the `secret_scan` gate as backstops. A
value the harness prints that was never injected is not covered; that is the existing honest limit.

**Attachments.** A local attachment is sent as bytes with a sha256 and a size cap; the home stores
it under `sessions/<id>/blobs/`. The home never asks the Mac for a path.

### 5.4 MCP tools on the local surface

`vowp mcp` runs on the Mac as a stdio server:

```sh
vowp mcp --agent-socket ~/.vowp-agent/agent.sock --surface <id>
```

- `initialize`, `tools/list` and `ping` are served locally.
- `tools/call` is sent as a `ToolCall` frame to the agent, then to the home. The home checks the
  surface token scope and the lease, executes the same core path as the home MCP server, and
  returns the same result shape.
- Reads prefer the home when connected. When the home is unreachable, they may use the agent's
  snapshot cache, clearly marked `as of`. Writes always require the home; there are no offline
  writes.
- `tunnel_open` and `tunnel_close` are refused on a local surface: the agent's connection is the
  transport, and tunnels are opened by the machine that uses them.
- `secret_request` is local-only or refused. A home secret value is never sent to the Mac. The
  message names the variable and points to the Mac's own environment/Keychain.
- `node_enroll`, `node_revoke`, `surface_pair` and offload-policy changes are CLI-only on the home;
  they are not MCP tools. This keeps the blast radius of a stolen surface token small.
- The opencode 2.0.28 caveat still applies: a server can be configured and report `connected`
  without the model seeing its tools. The `AGENTS.md` template must not promise MCP tool calls; the
  proven fallback is `vowp chat` (or Claude Code on the home).

### 5.5 `vowp chat` as the first local surface

The first local surface is not opencode; it is `vowp chat` over the agent:

```sh
vowp chat --agent --session <id> [--executor <exec>]
```

The local CLI connects to the agent socket. The agent acquires the lease, calls the home's
`begin_turn` over the gateway, runs the adapter locally, spools and streams events, and shows the
follow stream. The turn grant carries a `PathRef`, not the Mac's absolute path. This is the
existing `__exec`/ingest loop with the agent as a persistent transport, and it avoids the app's SSE
and MCP unknowns. It proves the lease, spool, replay and takeover behavior before the app bridge is
added.

`--session <id>` is equivalent to the positional id, and `--follow` and `--takeover` are refused
in agent mode. When a home orchestrator uses the default `chat` app, the agent starts
`vowp chat --agent <session>` with `VOWP_AGENT_HOME` pointing at its own state directory and keeps
a piped stdin open for the life of the host, so the interactive process does not see immediate
EOF. An early exit is `app_exited`; the orchestrator card carries the redacted reason.

### 5.6 Local context and privacy

The app reads local files; the session log records what the app and its tools emit. The home stores
that content. The owner should treat the home as a copy of the work journal. Path normalization
and secret redaction are best-effort; an app or harness can read anything the Mac user can read.
VOWP is not a sandbox and does not claim to make an untrusted repository safe.

The phone cannot join the Mac's loopback app server. The phone continues through the home app or
`vowp chat`; a tunnel from the phone to the Mac is out of scope.

The app's local server binds `127.0.0.1` with its own password. The agent reads the local
`service.json` (0600), or materializes it from the app's stdout readiness lines when the app
prints them, and never logs the port or password. The pair link is a secret.

### 5.7 Orchestrators on a local node

```sh
vowp orchestrator new --name mac --app opencode --node mac-studio --project vowp
vowp orchestrator move mac --node mac-studio
vowp orchestrator move mac --home
```

- The home creates the orchestrator session and card, with `node=mac-studio` in the card. The home
  owns the canonical orchestrator record.
- The agent materializes a local copy of the config under
  `~/.vowp-agent/orchestrators/<name>/` and starts the local opencode service. The home does not
  start a host on the home.
- `join.desktop` is the local pair command; `join.phone` is the home app or `vowp chat`; `join.chat`
  is `vowp chat --agent`.
- `orchestrator stop` sends `SurfaceDetach` and stops the local host. `orchestrator resume` sends
  `SurfaceAttach`/`Start`. If the node is offline, `resume` fails with `node_offline` in the MVP
  (queueing is Phase 3). `resume_all` marks a local orchestrator `degraded` when its node is
  offline; it never starts it on the home.
- `orchestrator move <name> --node <node>` stops the current host, releases its lease, writes a
  handoff capsule on the existing session and starts the orchestrator on that node; `--home`
  reverses the direction. The exact same `SessionId` and the single session log on the home are
  preserved. The target is validated first (active node, mapped project), so an unknown, pending,
  revoked or unmapped target leaves the current host running. Moving to the current placement is
  idempotent, or resumes a stopped record there. The target node must already have a paired surface
  for the session; the home never mints a new session id during a move.
- The node consumes a move as the landed `Start` command: the command names the orchestrator, app
  and project, and the home session is carried by the target node's existing paired surface, not
  by a wire field. The agent's order is: stop a live host for the name and close its bridge first
  (the stop sends `SurfaceDetach` and releases the lease); read the active `surfaces/<id>/session`
  pairing, ignoring a revoked or `needs_pair` surface; overwrite a stale local
  `config.session`/`config.surface` from that pairing; write `config.json`, `AGENTS.md`,
  `opencode.json` and `orchestrator.json` atomically at 0600; start the app bound to that session;
  attach the surface and report `running`. A duplicate `Start`/`Resume` for the same binding
  reports the live host without spawning a second process. A `Resume` whose active pairing names
  a different binding than the live host is refused with `session_conflict` and leaves the live
  host and its surface attached. No active pairing is `surface_missing`; more than one active
  pairing without a configured surface is `surface_ambiguous`. Both refusals start nothing and
  never mint a session id. The agent never rewrites `surfaces/<id>/session`: the home wrote it
  when it paired the surface, and it is the home-issued authority for the move.
- The local `AGENTS.md` template says: you are on a local surface; paths are project-relative; the
  home cannot see the Mac's local files; call `session_resume` if the tool is available; ask before
  public actions; never read a secret. It does not tell the app to open a tunnel; the agent is the
  transport.
- opencode readiness is either the app's own `service.json` (0600) or its stdout. On macOS,
  `opencode serve` may print `server listening on http://127.0.0.1:<port>` and
  `server password <secret>` instead of pre-writing the file; the agent parses those lines and
  materializes a private 0600 `service.json` in the app's state tree. The app never needs a
  pre-written file. The file must still carry a non-empty password and a loopback URL or port; the
  port and password stay private and never enter a card, result, log or `host.json`.
- A default app binary is resolved on `PATH` before spawning. A missing default `opencode`,
  `claude` or `vowp` is `capability_missing` and starts no process; a custom command or an
  absolute path keeps the existing `spawn_failed` behavior.
- `OrchestratorCard.last_error` is the redacted failure detail. It is set only for `failed`,
  `degraded` and `node_offline` records, is `null` for `starting`, `running` and `stopped`, and
  `resume`/`stop` clear it. It never carries a port, password, secret value or absolute local path.
- If the home restarts while a local orchestrator was `running`, `resume_all` marks it `degraded`
  and the home resumes it when the node reconnects. A `stopped` orchestrator stays stopped.

## 6. The local node

### 6.1 Node model

A node row keeps the existing name, SSH target, capabilities and timestamps, and adds:

- `kind` — `home`, `ssh` or `agent`;
- `status` — `pending`, `active` or `revoked` (enrollment state, not presence);
- `labels_json` — owner-assigned strings such as `darwin-arm64`, `xcode`, `gpu`;
- `agent_version`, `protocol`, `last_error`.

Presence is computed, not stored: `online` under 30 seconds since the last heartbeat, `degraded`
from 30 to 90 seconds, `offline` after 90 seconds. A revoked node is `revoked` regardless of
presence.

Capacity is reported every 15 seconds and on change: CPU cores, load1, RAM total/free, disk free,
busy/max worker and verifier slots, battery/thermal state (optional), and network state (optional).
The home stores the latest snapshot for dispatch and shows a summary.

Provider limits are **per node**. The existing `limits` table stays home-wide; a new
`node_limits(provider, node, …)` table holds a Mac's account limits. A Claude limit on the Mac must
not park the home's Claude jobs.

Capabilities are names, never values: harness names and versions, git version, OS/arch, the names
of secrets the node can resolve, the mapped projects, and labels. No environment values and no
absolute paths in the snapshot.

### 6.2 Enrollment and path mapping

The path map is persisted in the agent's 0600 `agent.json` as `path_maps`. The owner adds,
updates and removes entries with `vowp agent paths add|rm` and reads them with `vowp agent paths
ls` or `vowp agent status`. `vowp agent enroll --project <PATH>` supplies the initial entries;
without `--project`, enrollment reuses the saved map.

```sh
vowp agent paths add --project ~/src/vowp --name vowp --allow src
vowp agent paths add --project ~/src/vowp-docs --name docs
vowp agent paths ls --json
vowp agent paths rm docs
```

Validation on the agent and on the home:

- the project exists on the home;
- the local root is absolute, exists, and is a git checkout;
- the worktree root is inside an allowed root and is writable;
- canonicalization resolves symlinks; the check compares path components, not string prefixes;
- `..`, a NUL byte, a non-UTF-8 path, a path over the length cap, or a symlink escaping the root is
  refused;
- macOS case-insensitivity is handled by canonicalizing both sides and comparing components; a
  case-only difference is not treated as the same directory unless the filesystem says so;
- the home stores the last reported absolute map in `node_paths` for the owner, but `vowp node
  show` redacts it from the default output and from the snapshot/API.

Path changes are initiated on the Mac, because only the Mac knows its filesystem. A running agent
reloads `agent.json` on every heartbeat and sends one `PathMapUpdate` for each added or changed
project over its live connection. The home validates and persists the `node_paths` row before it
answers `Pong`; a bad map is answered with `Error` and the connection stays open. Jobs waiting on
`path_unmapped` become eligible on the next tick. `vowp node show <node> --json` shows the new
project name in `mapped` immediately, without re-enrollment or a reconnect; the redacted card
never carries the reported absolute roots. A removal changes only local eligibility because the
frame vocabulary has no removal message. The home never edits the Mac's paths.

A missing local root reports `path_missing`; pinned jobs wait, `any` jobs may use another eligible
node or the home. The owner fixes the path and the node becomes eligible again.

### 6.3 Assignment and run lifecycle

1. The supervisor's dispatcher chooses an eligible job for a node (see §7).
2. The home inserts a durable `agent_assignments` row and sends `AssignRun` on the node's live
   connection. The durable payload has the run id, session id, project, job id, executor, purpose,
   the home-seeded prompt, a `PathRef::Worktree`, the base commit, the remote name, environment
   names, the epoch, a deadline and the spool cap. It **never** stores the lease token: the home
   issues a short-lived token when it sends the frame, stores only its hash in the lease row, and
   re-issues a token on resume.
3. The agent validates: node active, project mapped, local root and worktree root present and
   writable, cwd inside the allowlist, harness installed, required environment names present,
   disk above the reserve, no conflicting worktree. It sends `RunAccepted { run_id, epoch }` or
   `RunRejected { run_id, epoch, reason, detail }`.
4. The agent creates `<worktree_root>/<job-id>` with `git worktree add --detach <base_commit>`,
   then a `job/<id>` branch. It fetches the base commit from the local `origin` if it is missing.
   The main checkout is never modified.
5. The agent starts the harness with the prompt and the resolved environment, and streams events.
   Every event is written to the spool and fsynced before it is sent. The home appends and acks.
6. On completion, the agent sends the summary (exit code, outcome, native id, cost, usage, result
   text, head commit, push status). The home writes the run row, ledger and cost rows.
7. With `result_transport = "push"`, the agent pushes `job/<id>` to the project's origin using its
   own git credentials, then reports the head commit. The home fetches, verifies the base and head,
   and runs the authoritative `owns_respected` and `secret_scan` gates.
8. The home's merge queue and promotion stay on the home. The node never merges, tags, deploys or
   pushes the default branch.
9. After the home marks the job done, cancelled or abandoned, it sends `Cleanup`; the agent removes
   its own worktree. A failed or lost run keeps the worktree until the owner or the retention policy
   removes it.

Run states:

| State | Meaning |
|---|---|
| `assigned` | the home sent the assignment; no accept yet |
| `accepted` | the agent validated the run |
| `running` | the harness is running and events are flowing |
| `disconnected` | the home lost the connection; the run may resume within grace |
| `finishing` | the summary/push is in progress |
| `finished` | the home wrote the terminal run row |
| `lost` | the grace expired or a takeover fenced the run |
| `rejected` | the agent refused before starting |
| `cancelled` | the owner or the home cancelled it |

The project `runs` row is canonical. It gains `transport` (`local` or `agent`), `epoch`,
`base_commit`, `head_commit`, `spool_bytes` and `cwd_ref`. The global `agent_assignments` row is
the durable dispatch queue and reconnect record; it is not a second run history.

An offloaded job is represented as a normal session with an `agent`-kind surface on the node. The
home issues the session lease; the run's events are appended under it. There is no second run log
and no second session model.

Idempotency: a repeated `AssignRun` for the same `(run_id, epoch)` resumes; a higher epoch
supersedes; a lower epoch is rejected. A second connection for the same node supersedes the first;
the agent enforces a single instance with a lock file.

### 6.4 Result return

The MVP uses **push with the node's own credentials**:

- The agent verifies that its local `origin` URL, with credentials stripped, matches the home's
  `projects.remote_url`. A fork or a different push remote is refused in the MVP; the owner can
  request an explicit `push_remote` later.
- The agent pushes only `job/<id>` (or a namespaced `refs/vowp/jobs/<id>`); it never pushes the
  default branch, tags or releases.
- The agent reports `head_commit`. The home fetches the branch, checks that the head is a
  descendant of the base, and runs the `owns_respected` and `secret_scan` gates. A head that moved
  unexpectedly or a branch with commits outside the job's owns fails the run; no merge happens.
- If the push fails, the run fails with `git_push_failed`; the worktree remains for inspection; the
  job's retry policy applies. No partial merge.

The alternative is a **git bundle upload** over the agent connection: the agent creates a bundle
of `base..head`, sends it in chunks with a sha256, and the home applies it to a staging bare repo.
This needs no push access on the Mac and is fully authenticated by the agent channel, but it adds
a binary protocol, a size cap and storage on the home. It is a Phase 4 option for read-only nodes.

The home never sends a git credential. The node uses its own credential helper or deploy key. A
node without push access is not eligible for `result_transport = "push"`.

### 6.5 Worktrees and cleanup

- One worktree per job, at `<worktree_root>/<job-id>`, branch `job/<id>`.
- Created from the home-supplied base commit, not from the main checkout's dirty working tree.
- The main checkout is fetched, never checked out or reset by the agent.
- The agent reports a dirty worktree (uncommitted files) and the `owns` diff summary; the home's
  `owns_respected` gate is authoritative.
- Cleanup removes only the agent's own worktree and branch; it never removes a user directory or a
  worktree VOWP did not create.
- A retry reuses the worktree and branch only when the branch head is the expected base and the
  worktree is clean; otherwise the agent re-creates them, so a stale branch cannot leak into a new
  run.
- Disk guards: a minimum free-space reserve (5 GiB default), a per-run spool cap (512 MiB default),
  a total spool cap (2 GiB default), and a worktree retention (7 days default). A node below the
  reserve reports `disk_low` and gets no new runs; a running run may finish.

### 6.6 Capacity and limits

- The agent enforces `max_workers` and `max_verifiers`; the home dispatches at most that many.
- Home capacity and node capacity are separate pools. The home's `advised_target` stays home-only;
  `vowp capacity report` and `vowp fleet status` show a node section.
- The home's brake stops new **home** runs. Remote dispatch continues by default because the
  harness uses the Mac's CPU, not the home's; `offload.pause_on_home_brake = true` makes the brake
  global. This is an owner decision (§16).
- A node on battery or thermally limited reports `draining`; with `allow_battery = false` it gets no
  new runs, while running runs continue.
- Provider limits are per node: a limit on the Mac's account is written to `node_limits` and does
  not park the home's jobs. A remote run's failover consults the node's limits, not the home's.
- Cost and usage rows keep `node`, so the cost report can group by node.

### 6.7 What can be offloaded

| Work | Offload? | Why |
|---|---|---|
| build, test, lint, docs, hermetic fix and verify | yes | local compute; repo code; no home secret |
| review/verifier | yes if no home-only context | the node has the diff and the worktree |
| merge, promote, deploy, release, probe | no | home merge queue and production credentials |
| a job requiring a home-only secret | no | no credential copying |
| a job needing a home-only path or database | no | the Mac does not have it |
| a local-only project with no home checkout | Phase 4 | needs a bare mirror and result return |
| an untrusted repository | only in a separate user/VM | VOWP is not a sandbox |

## 7. Offload policy

### 7.1 Placement

A job gains a `placement` value:

- `home` — the default; the job never leaves the home.
- `node:<name>` — only that node. If it is offline or ineligible, the job waits. `fallback = "home"`
  is an explicit opt-in, never a silent default.
- `any` — the home and every eligible node; the dispatcher picks by project affinity, free slots
  and load.

The project default is `home` unless `[offload.projects.<name>]` opts in. `offload.enabled = false`
forces every job home; a job with `placement = node:<name>` is refused at add time when offload is
disabled. There is no silent fallback.

### 7.2 Eligibility checks, in order

1. Offload is enabled; the node is active, not revoked, and `online`.
2. The placement matches the node.
3. The project opted in and is not `home_only`.
4. The job kind is allowed (default: build, test, docs, fix, verify; not merge, promote, deploy).
5. Required labels and capabilities are present.
6. No required secret is `home_only`; the node declares the names it can resolve.
7. The path map exists and is valid; the local root and worktree root are present and writable;
   disk is above the reserve.
8. The node has a free worker/verifier slot and is not battery/thermal limited (unless allowed).
9. The node's provider limit does not block the executor.
10. No `owns` overlap with a running job; no conflicting run for the same job.
11. The local `origin` matches the project remote; the node can push (for `result_transport =
    "push"`); the base commit is fetchable.

The decision and the reason are recorded. A refused job stays `ready` with a `waiting_reason`;
it is not failed unless the policy says the requirement can never be met.

### 7.3 Secret classes

| Class | Meaning | Offload |
|---|---|---|
| `node_ok` | the node has its own value for the name (provider login, build token, git credential) | yes; only the name is sent |
| `home_only` | the value exists only on the home (deploy key, production token, home vault secret) | no |
| `either` | the home and/or the node may have it | only if the node declares the name |

The home sends `env_names` / `secret_names`, never values. The node resolves them from its own
environment or Keychain, refuses with the missing name if absent, and redacts the values it
injected before spooling or sending. The home runs its own redaction and the `secret_scan` gate as
backstops.

`secret_request` on a local surface never returns a home value. It either points at the Mac's own
local secret setup or is refused with a clear message.

### 7.4 Policy file

```toml
[offload]
enabled = true
default = "home"              # home | any
max_remote_runs = 2           # fleet-wide cap
grace_secs = 600
result_transport = "push"     # push | bundle (phase 4)
pause_on_home_brake = false
require_clean_base = true
min_disk_bytes = 5368709120

[offload.projects.vowp]
placement = "any"
allow_kinds = ["build", "test", "docs", "fix", "verify"]
deny_kinds = ["merge", "promote", "deploy"]
allow_secrets = ["CARGO_REGISTRY_TOKEN"]   # names the node must have locally

[offload.projects.secret-app]
placement = "home"            # home-only project
home_only = true

[offload.nodes.mac-studio]
max_workers = 2
allow_battery = false
labels = ["darwin-arm64", "xcode"]   # must match the node's enrolled labels
```

The effective policy is shown by `vowp offload policy --json`; the per-job `placement` wins over
the project default, but it cannot override `home_only`, a denied kind or a home-only secret.

### 7.5 Waiting reasons

`node_offline`, `node_revoked`, `node_busy`, `node_battery`, `node_disk_low`, `node_limit`,
`path_unmapped`, `path_missing`, `missing_secret`, `home_only_secret`, `kind_not_allowed`,
`project_home_only`, `remote_mismatch`, `no_eligible_node`.

`vowp fleet queue` and `vowp offload status` show the reason; the ledger records the offload
decision. The reason is a first-class part of the read model, not a log line the owner must grep.

### 7.6 Examples

- A docs job in `vowp` with `placement = "any"` goes to `mac-studio` when it is online and the home
  is busy; it returns a branch by push; the home runs `secret_scan` and merges.
- A build job in a `home_only` project stays home even if a node is idle.
- A deploy job is refused for offload by kind, not by a runtime error after dispatch.
- An Xcode job with `requires = ["xcode"]` waits for `mac-studio`; when the Mac is asleep it shows
  `node_offline`, and it does not run on the home.
- A job requiring `PROD_DEPLOY_KEY` shows `home_only_secret` and waits for the home; the key never
  goes to the Mac.

## 8. Transport and protocol

### 8.1 Transport choice

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| **SSH stdio** | no new public port; reuses key-only SSH; simple; works with the existing control-machine setup | SSH must be reachable; one connection; forced-command setup | **MVP** |
| **WSS ingress** | works where SSH is blocked; standard proxies; good middlebox behavior | a new public listener; TLS/ops; rate limits; larger attack surface | Phase 4, opt-in |
| SSH reverse tunnel | outbound from the Mac; no new public port | a listener on the Mac; the home connects in; more moving parts | rejected |
| Tailscale/WireGuard | NAT traversal | a new network/auth layer; not VOWP identity | deployment option, not a protocol |

The frame protocol is transport-agnostic. WSS carries the same frames; only the connection setup
differs.

### 8.2 Connection lifecycle

- The agent runs:

  ```sh
  ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
      <home-ssh-target> vowp agent gateway --stdio
  ```

- The remote command is exactly `vowp agent gateway --stdio`; the client adds no `--project`,
  and the SSH session starts in the home user's `$HOME`. The gateway therefore opens the home's
  global store (`~/.vowp/vowp.db`) directly, without requiring `--project` or a project cwd, and
  works on a home with zero, one or several registered projects. Node cards, credentials,
  sessions, leases, surface tokens and offload assignments are global. Session-bound work (chat
  and paired surfaces) resolves the session row's `project` binding (`_node` for a project-less
  session), and offload work resolves the assignment's `project`; the run, job, gate and ledger
  rows are then read from and written to that project's database. An explicit `--project` or a
  cwd inside a registered checkout still selects that project's context.
- The gateway runs as the home user with the same store access as any other home command. It
  has no shell and no project checkout of its own. Phase 4 may move it behind a low-privilege
  supervisor socket owned by a dedicated user and group; that is future work, not current
  behavior.
- Handshake: `AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome`; then `Resume` for the
  node's runs and surfaces.
- One active connection per node. A second connection supersedes the first; the old connection
  receives `Superseded` and closes. The agent's single-instance lock prevents this in normal
  operation.
- Heartbeat every 10 seconds. The home sends `Ping`; the agent answers `Pong`; no pong for 30
  seconds closes the connection. The agent reconnects with exponential backoff (1, 2, 4 … 60
  seconds, with jitter) and resets after 60 seconds of a stable connection.
- Each agent heartbeat also reloads `agent.json` and sends a `PathMapUpdate` for every project
  added or changed since the last report. The home validates the map, persists it and answers
  `Pong`; an invalid map gets `Error` without closing the authenticated session. A home that
  predates the frame answers `Error`; the agent disables further updates for that run instead of
  reconnecting in a loop.
- A post-handshake `Error` from the home is a connection failure, not a terminal node event: the
  agent records a redacted reason, keeps its spool and reconnects. The exception is a `Refused`
  error whose message names a surface token: the agent marks that surface `needs_pair` and stays on
  the same connection, because a dead surface token is not a dead node key. `Revoked` and
  `Superseded` remain terminal. An unexpected but known frame after the handshake is logged and
  ignored; the gateway never ends an authenticated session just because a newer agent sent a frame
  it does not use.
- Home restart closes the socket; the agent reconnects and reconciles with `Resume`.

### 8.3 Frames

Frames are JSON lines, one object per line, UTF-8, maximum 1 MiB (configurable). A frame has a
`type` and a `protocol` version at the handshake; payloads are typed.

Agent → home:

| Frame | Purpose |
|---|---|
| `AgentHello` | node, key id, agent version, OS/arch, labels, capabilities, path map |
| `AgentAuth` | signature over the home nonce, home id and connection id |
| `Resume` | runs and surfaces with `last_acked` and epoch |
| `Heartbeat` | time, capacity, app state, pending spool bytes |
| `RunAccepted` / `RunRejected` | validation result for an assignment |
| `RemoteLine` | one `RemoteLine` (event, heartbeat, summary) for a run, wrapped with run id and epoch |
| `RunState` | running/disconnected/finishing, spool bytes, head commit |
| `SurfacePair` | redeem a one-time pairing code |
| `SurfaceAttach` / `SurfaceDetach` | attach or detach a surface and request a lease |
| `SurfaceEvent` | a normalized event with a native id, for an attached surface |
| `SurfaceTurn` | `started`/`ended` control frame for an attached turn |
| `ToolCall` | an MCP tool call from a local client |
| `LeaseAcquire` / `LeaseTakeover` / `LeaseHeartbeat` / `LeaseRelease` | lease operations |
| `PathMapUpdate` | a project's local root/worktree root changed; the home persists it and answers `Pong` |
| `Pong` | heartbeat reply, and the path-map update acknowledgement |

Home → agent:

| Frame | Purpose |
|---|---|
| `HomeChallenge` | nonce, home id, connection id, server time, heartbeat interval, max frame |
| `HomeWelcome` | protocol, node, connection id, resume result |
| `AssignRun` | a job/run assignment with prompt, path ref, base commit, env names, lease, epoch |
| `CancelRun` | cancel with a reason |
| `IngestAck` | ack, duplicate, summary, lease revoked, or error for a run |
| `LeaseGrant` | lease token, expiry, tail sequence |
| `LeaseLost` | the lease was taken over or expired; the surface is read-only |
| `LeaseRevoked` | the old surface is read-only; stop sending events |
| `SurfaceIssued` | surface token, scopes, expiry |
| `SurfaceRevoked` | the surface token was revoked; detach and delete it |
| `ToolResult` | the MCP result |
| `ResumeOk` | per-run and per-surface resume result after a reconnect |
| `Credit` | replenish the in-flight event window for a stream |
| `RunLost` | the run was fenced; stop and discard |
| `Cleanup` | remove the worktree |
| `Ping` | heartbeat |
| `Revoked` | the node credential was revoked |
| `Superseded` | another connection took over |
| `Error` | protocol/policy error with a retryable flag |

`RemoteLine` and `IngestAck` are reused from the v0.2 protocol; the envelope adds `run_id` and
`epoch`. The home's `IngestSession` remains the state machine that appends events and writes the
run/ledger rows.

`SurfaceIssued` carries the only long-lived secret in a frame. The channel is encrypted (SSH in the
MVP, TLS under WSS), the frame is never logged, and the agent writes it 0600 and acks.

### 8.4 Flow control and idempotency

- One stream per run or surface. Events are sent in order; the home assigns `seq`; the agent never
  sends `seq`.
- A 64-event in-flight window per stream, as in the existing `__exec` client. The home acks each
  event; the agent updates `acked.json`. A `Credit` frame may replenish the window; the default is
  the fixed window.
- The agent writes an event to the spool and fsyncs it **before** sending. An event that was not
  durably spooled is never sent.
- A replay starts from the first unacked event. The home dedupes by event id: a known id with the
  same content is `Duplicate` and is not appended; a known id with different content is
  `Conflict`, the stream stops, and the spool is quarantined.
- A slow agent does not block the supervisor: each connection has a bounded channel and a thread;
  the home applies backpressure and never grows an unbounded queue.
- The spool cap stops the stream rather than the home: the agent sends `Error { kind: spool_full }`,
  stops the harness, and marks the run failed. The home records the reason.

### 8.5 Versioning

- Protocol `vowp-agent/1`. The handshake exchanges the home and agent versions and the protocol.
- The home supports the current and previous minor protocol. A mismatch is a clear error naming
  both versions; there is no silent downgrade and no partial frame handling.
- Capabilities: `path_ref_v1`, `spool_v1`, `surface_mirror_v1`, `tool_proxy_v1`, and later `wss`.
- Upgrade the home first, then the agents. A v0.3 home has no gateway; the agent reports
  `unsupported_home` and exits with a clear message.

### 8.6 WSS ingress (Phase 4, optional)

- A reverse proxy (Caddy or nginx) terminates TLS on 443 and forwards `/agent/v1` to the VOWP
  gateway on `127.0.0.1`. The VOWP processes still bind loopback; the proxy is the only public
  listener. This is an explicit deployment choice, not the default.
- TLS 1.3 with the system trust store. The enrollment records the home's `home_id` and a
  fingerprint; a changed fingerprint is a warning that requires owner confirmation, never a silent
  trust-on-first-use reset.
- The same node challenge-response. mTLS is optional defense in depth, not the primary auth.
- Limits: max connections, handshake timeout, frame size, rate per node, no other routes, no
  directory listing, no health details. Logs contain no tokens or node secrets.
- Deployment docs cover the firewall, certificate renewal, proxy hardening and rollback to SSH.

## 9. Disconnect, failure and recovery

### 9.1 States

| Layer | States |
|---|---|
| connection | `connected`, `degraded` (spooling or slow), `offline` (retrying), `revoked` |
| surface | `attached`, `pending` (spooling), `read_only` (lease lost), `detached` |
| run | `assigned`, `accepted`, `running`, `disconnected`, `finishing`, `finished`, `lost`, `rejected`, `cancelled` |
| node presence | `online`, `degraded`, `offline`, `revoked` |

### 9.2 Short disconnect

- The SSH connection drops. The agent keeps the harness or app running and spools every event,
  fsynced, redacted and capped.
- The agent reconnects with backoff and sends `Resume { runs: [{run_id, epoch, last_acked}],
  surfaces: [...] }`.
- The home checks the run fence and the lease. If both are current, it returns `ResumeOk` per run;
  the agent replays from the first unacked event; the home dedupes.
- The lease heartbeat stops during the drop. Under 60 seconds, the lease is still valid. Over 60
  seconds, another surface may take over; on reconnect the agent learns `LeaseLost` and drops the
  pending **surface** events with a notice. An offloaded job's run lease is separate and may still
  resume if its fence is current.
- The app shows `offline, recording locally (N events pending)` when the surface spool is active.
  MCP write tools fail; reads use the cache with `as of`.

### 9.3 Grace and fencing

- Run grace is 600 seconds by default (configurable, capped). The home marks the run
  `disconnected`; the job stays `running`; no new run starts for that job/session.
- Reconnect within grace, with a current fence and lease: `ResumeOk`, replay, finish.
- Grace expiry: the home writes `error { kind: transport_lost }` and `turn.ended`, closes the run
  with `outcome=error` and a detail, releases the run, and records the ledger. The agent receives
  `RunLost` on reconnect and stops the harness.
- A takeover during the disconnect fences the old run **immediately**, writes a capsule from the
  events already appended, and gives the old agent `RunLost`. Takeover wins over grace.
- Late frames are checked against `(run_id, epoch, connection_id)`. A stale epoch is
  `RunLost`/`Conflict`; nothing is appended.
- The agent self-fences after `grace + margin` without a home connection: it stops the harness and
  marks the spool `orphaned`. The home's fence is authoritative; the self-fence is a resource guard.

### 9.4 Failure table

| Failure | Home | Agent | Owner sees |
|---|---|---|---|
| Wi-Fi drop under 60 s | run `disconnected`; lease alive; resumes | spool, reconnect, replay | reconnecting |
| Wi-Fi drop over 60 s, no takeover | lease stale; run `disconnected`; resumes if fence current | spool; on reconnect `ResumeOk` or `RunLost` | offline, then replay |
| takeover during a drop | fence, capsule, new surface writes | `RunLost`; discard pending; stop harness | old app read-only |
| Mac sleep | same as a drop; grace | resume on wake | offline |
| home restart | connections lost; reconcile; runs within grace `disconnected`, else lost | reconnect, `Resume` | home restarting |
| agent crash | run stays `disconnected` until grace | on restart scans the spool; a summary is replayed; no summary means no harness restart; the run closes with `error` and detail `agent_crash` | job fails and retries |
| disk full | node `disk_low`; no new runs | spool cap; stop harness; `spool_full` | job failed with reason |
| node revoked | fence all runs; close | `Revoked`; stop; keep spool | node revoked |
| path moved | jobs wait `path_missing` | reports; no runs | waiting reason |
| git push fails | run fails; no merge | retries; reports | `git_push_failed` |
| home-only secret | job waits or fails `home_only_secret` | not dispatched | waiting reason |
| protocol mismatch | refuses | clear error | upgrade both sides |
| clock skew | home decides lease and grace | monotonic local timeouts | no false takeover from the agent clock |

### 9.5 User-visible behavior

- `vowp agent status` shows connection, node, home, protocol, pending events, runs, spool bytes,
  path maps, surface token expiry (not values) and the last error.
- `vowp node show` and `vowp fleet status` show presence, runs, capacity and waiting reasons;
  `node show` shows project names in `mapped` and never the absolute path map.
- `OrchestratorCard.last_error` shows a redacted failure reason for a `failed`, `degraded` or
  `node_offline` local orchestrator and is `null` while it is starting, running or stopped. It
  never contains a port, password, secret value or absolute local path.
- The local app receives MCP tool errors with a clear message. The bridge cannot force another
  vendor's UI to display a banner; the `AGENTS.md`, the MCP responses and the agent status carry
  the notice. This is the same limitation as the v0.2 takeover notice.
- No silent gaps. If the bridge cannot record, it writes a local `gap.json`; on reconnect, if the
  lease is still held, it sends a `note` event for the gap. If the lease was lost, the gap stays
  local and the owner is told. A gap is never fabricated into a contiguous log.

### 9.6 What is never done

- No divergent second log, no automatic merge, no replay after a fence, no silent fallback to the
  home, no automatic harness restart after an agent crash, and no deleting a spool without the
  owner's knowledge.

## 10. Security model and allowlists

### 10.1 Trust boundaries

| Boundary | Trusted for | Not trusted for |
|---|---|---|
| owner | choosing projects, approving public actions, revoking | — |
| home | policy, leases, logs, dispatch, secrets | not a sandbox for repo code |
| Mac user account | the agent, app and local files | other local processes can read the same files |
| harness | running the prompt, calling its provider | not exfiltrating local files; it runs as the user |
| repo code | — | anything; it is untrusted code |
| network | — | SSH/TLS only; no plaintext |

The home trusts an enrolled node to run repo code in its mapped roots and to report honestly. It
does not trust the node with home secrets or other sessions. The Mac trusts the home to send only
allowed runs and prompts; a compromised home can ask the node to run code in the mapped roots.
That is the fundamental limit; a separate macOS user or VM is the mitigation for untrusted work.

### 10.2 No credential copying

- No private key, harness login, provider key, git credential or home secret value crosses the
  boundary.
- The node key is generated on the Mac; only the public key is enrolled.
- The surface token is a VOWP credential, not a provider credential; it is scoped and revocable.
- The node uses its own provider and git credentials. The home sends names, never values.
- The agent redacts the values it injected; the home redacts again and runs `secret_scan`.
- `vowp secret` on the home never sends a value to an agent. `secret_request` on a local surface is
  local-only or refused.
- `vowp agent doctor` checks that `~/.vowp-agent` contains no `~/.ssh`, `~/.claude`, opencode auth
  file or copied `.env`; it reports names, never contents.

### 10.3 Agent-side allowlists

```toml
[agent]
home = "vowp-home"           # the home's node name
node = "mac-studio"
state_dir = "~/.vowp-agent"
roles = ["surface", "executor"]
heartbeat_secs = 10
max_spool_bytes = "2GiB"
max_frame_bytes = 1048576
grace_secs = 600

[agent.transport]
kind = "ssh"                 # ssh | wss (phase 4)
target = "<home-ssh-target>"
batch_mode = true

[agent.paths.vowp]
root = "/Users/me/src/vowp"
worktrees = "/Users/me/src/vowp/.vowp/worktrees"

[agent.allow]
cwd = ["/Users/me/src"]
write = ["/Users/me/src/vowp/.vowp", "/tmp/vowp-agent"]
exec = ["opencode", "claude", "git", "cargo", "npm", "pytest"]
env_pass = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]   # names only
deny_read = ["~/.ssh", "~/.aws", "~/.config/gh", "~/Library/Keychains"]
```

- `cwd`, `write` and `root` are checked with canonicalization and path-component prefixes, not
  string prefixes. A symlink escape or `..` is refused.
- `exec` is the allowlist for processes the **agent** spawns (harnesses and helper binaries). Repo
  gate commands run through the harness or a shell inside the worktree and are repo code; the list
  does not sandbox them. State this honestly.
- `env_pass` is names. The agent resolves values from its own environment or Keychain, never logs
  them, and redacts them before spooling or sending.
- `deny_read` is best-effort. The harness runs as the user and can read anything the user can. For
  strong isolation, run the agent as a separate macOS user or in a VM; that is not the MVP.
- The agent refuses to run as root, has no sudo path, writes state under a 0700 directory, uses a
  0600 socket and spool, and logs no tokens or values.

### 10.4 Home-side allowlists

- The node must be active and not revoked; the project must be mapped; the job must be eligible;
  the path map must be valid; the secret class must allow it; the labels and capacity must match.
- Frame allowlist: known types, maximum size and rate, maximum streams, handshake timeout. An
  unknown or oversized frame closes the connection; no partial processing.
- The gateway has no DB code and connects only to the supervisor's 0600 Unix socket. In the MVP it
  runs as the owner's account, so this is a code boundary, not an OS sandbox; a gateway exploit is
  a home compromise. Phase 4 runs it as a dedicated low-privilege user with an `authorized_keys`
  forced command `vowp agent gateway --stdio`, `no-pty`, `no-port-forwarding`,
  `no-agent-forwarding`, `no-X11-forwarding`.
- Surface token scopes and the lease are checked on every write tool and on attach; tool names are
  allowlisted; node/policy administration is not a surface tool.
- Per-node and per-surface rate limits; fail closed.
- The owner's sshd stays key-only with no passwords, a restricted `AllowUsers` where possible, and
  a login rate limit. VOWP does not manage sshd.

### 10.5 Scopes

| Principal | May | May not |
|---|---|---|
| node credential | connect, heartbeat, receive assignments for mapped projects, append events for assigned runs, report capacity | read other sessions/projects, write job/inbox/policy, read secrets, enroll |
| surface token | attach to one session, read/write its events under lease, call allowed MCP tools | other sessions, node admin, policy, home secrets |
| lease token | append for one session while held | any other session or run; anything after takeover |
| gateway | forward frames | open a DB, read logs, execute tools |
| local MCP | tool calls through the agent | direct DB or SSH access to the home |

### 10.6 Redaction and paths

- Local secret values are redacted by the agent before the spool and by the home before the log;
  the marker is `[REDACTED:<NAME>]`.
- Absolute local paths are replaced with `PathRef` or `<external>` before they leave the Mac.
- Tokens never appear in logs, errors, snapshots or events. Audit rows carry the surface id and
  token prefix, not the token.
- The home's `secret_scan` gate runs on offloaded diffs; the node also scans before pushing.
- Known gap: exact-value redaction covers values VOWP injected, not arbitrary credentials a harness
  prints. `secret_scan` and the gate are the backstop. This is the existing honest limit.

### 10.7 Threat model

| Threat | Mitigation | Residual |
|---|---|---|
| stolen Mac with a live lease | scoped surface token; lease expiry; `surface revoke`; pending events discarded | can append to its session until revoked; can read local files |
| stolen Mac with the node key | node revoke; SSH key removal; run fences | can run assigned jobs in mapped roots until revoked |
| malicious repository | gates, `owns`, `secret_scan`, separate user/VM for untrusted work | code runs as the user; not a sandbox |
| compromised home | agent allowlists roots/binaries; the owner can revoke | the home can send prompts that read local files |
| network attacker | SSH/TLS; node signature; nonce; no token in URL | provider-side and OS compromise remain |
| malicious node | event validation, gates, `owns`, `secret_scan`, revocation | false cost/status reports; the home trusts the node's honesty within its scope |
| replay | nonce + connection id + epoch + event ids | none within the grace window |
| DoS | frame/connection/rate limits, spool caps, bounded queues | a determined node can still consume its own resources |

### 10.8 What VOWP does not protect against

A compromised owner account or home, a harness that exfiltrates local files, a malicious app or
plugin, a keylogger or screen recorder on the Mac, provider-side logging, disk theft without
FileVault, a malicious repo's build scripts, or multi-tenant isolation.

### 10.9 Audit and incident response

- Ledger: enroll, revoke, path change, offload decision, assignment, fence, surface issue/rotate/
  revoke, lease takeover. No tokens, secret values or absolute paths.
- `vowp node show` and `vowp agent status` show the last error.
- Revoke a node (`vowp node revoke`) to close every surface and fence every run; revoke one surface
  (`vowp surface revoke`) for a lost app. Remove the SSH key if it was dedicated; rotate provider
  and git credentials on the Mac if they may have been exposed; inspect the local spool before
  deleting it.

## 11. Data model and migrations

### 11.1 Global schema (proposed, not final)

```sql
ALTER TABLE nodes ADD COLUMN kind TEXT NOT NULL DEFAULT 'ssh'
  CHECK (kind IN ('home','ssh','agent'));
ALTER TABLE nodes ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
  CHECK (status IN ('pending','active','revoked'));
ALTER TABLE nodes ADD COLUMN labels_json TEXT NOT NULL DEFAULT '[]'
  CHECK (json_valid(labels_json));
ALTER TABLE nodes ADD COLUMN agent_version TEXT;
ALTER TABLE nodes ADD COLUMN protocol TEXT;
ALTER TABLE nodes ADD COLUMN last_error TEXT NOT NULL DEFAULT '';

CREATE TABLE node_credentials (
  node        TEXT NOT NULL REFERENCES nodes(name) ON DELETE CASCADE,
  key_id      TEXT NOT NULL,
  public_key  TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  last_used_at TEXT,
  revoked_at  TEXT,
  PRIMARY KEY (node, key_id)
) STRICT;

CREATE TABLE node_paths (
  node          TEXT NOT NULL REFERENCES nodes(name) ON DELETE CASCADE,
  project       TEXT NOT NULL,
  local_root    TEXT NOT NULL,
  worktree_root TEXT NOT NULL,
  allow_json    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(allow_json)),
  updated_at    TEXT NOT NULL,
  PRIMARY KEY (node, project)
) STRICT;

CREATE TABLE surface_tokens (
  id           TEXT PRIMARY KEY,
  surface_id   TEXT NOT NULL REFERENCES surfaces(id) ON DELETE CASCADE,
  token_hash   TEXT NOT NULL,
  prefix       TEXT NOT NULL,
  scopes_json  TEXT NOT NULL CHECK (json_valid(scopes_json)),
  created_at   TEXT NOT NULL,
  expires_at   TEXT,
  revoked_at   TEXT,
  last_used_at TEXT
) STRICT;
CREATE UNIQUE INDEX surface_tokens_hash ON surface_tokens(token_hash);

CREATE TABLE enrollment_codes (
  code_hash  TEXT PRIMARY KEY,
  node       TEXT NOT NULL,
  kind       TEXT NOT NULL CHECK (kind IN ('node','surface')),
  surface_id TEXT,
  expires_at TEXT NOT NULL,
  used_at    TEXT,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL
) STRICT;

CREATE TABLE agent_assignments (
  id           TEXT PRIMARY KEY,
  run_id       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  project      TEXT NOT NULL,
  node         TEXT NOT NULL,
  epoch        INTEGER NOT NULL,
  state        TEXT NOT NULL CHECK (state IN
                 ('queued','sent','accepted','running','disconnected',
                  'finished','lost','cancelled','rejected')),
  payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
  created_at   TEXT NOT NULL,
  claimed_at   TEXT,
  expires_at   TEXT,
  detail       TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX agent_assignments_node ON agent_assignments(node, state);

CREATE TABLE node_limits (
  provider    TEXT NOT NULL,
  node        TEXT NOT NULL REFERENCES nodes(name) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('quota','credits','rate','context','auth','unavailable')),
  resets_at   TEXT,
  detail      TEXT NOT NULL DEFAULT '',
  since       TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (provider, node)
) STRICT;
```

The existing `limits` table stays home-wide. `leases` changes from `token TEXT` to `token_hash
TEXT` with a 64-character check; existing leases are cleared on upgrade because they are stale
within 60 seconds. `surfaces` gains `state` (`pending`, `attached`, `revoked`) and `revoked_at`;
`SurfaceKind` gains `Agent` (wire `agent`), and the `surfaces.kind` and `leases.surface_kind`
checks gain it for an offloaded job's run surface. `sessions` gains `cwd_ref_json`; `projects`
keeps its policy in `config_json`.

### 11.2 Project schema (proposed)

```sql
ALTER TABLE runs ADD COLUMN transport TEXT NOT NULL DEFAULT 'local'
  CHECK (transport IN ('local','agent'));
ALTER TABLE runs ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN base_commit TEXT;
ALTER TABLE runs ADD COLUMN head_commit TEXT;
ALTER TABLE runs ADD COLUMN cwd_ref_json TEXT;
ALTER TABLE runs ADD COLUMN spool_bytes INTEGER NOT NULL DEFAULT 0;
```

`PRAGMA user_version` becomes 2 for the project schema. The project DB stays on the home; the Mac
never opens it.

### 11.3 Types (proposed)

```rust
pub enum PathRef {
    Project { project: ProjectName },
    Worktree { project: ProjectName, job: JobId },
    Subdir { project: ProjectName, rel: String },
}

pub enum Placement {
    Home,
    Node { node: NodeName, fallback: Option<NodeFallback> },
    Any,
}

pub enum NodeFallback { Home }

pub struct NodeCard { /* name, kind, status, presence, labels, capacity, runs, mapped, version */ }

pub struct AgentFrame { /* the tagged enum in §8.3 */ }

pub struct RunFence {
    pub run_id: RunId,
    pub epoch: u64,
    pub node: NodeName,
    pub connection_id: String,
    pub leased_until: Timestamp,
}
```

`PathRef` serializes as a short string (`project:vowp`, `worktree:vowp/<job>`, `subdir:vowp/src`)
and parses strictly: no `..`, no leading slash, no NUL, no backslash, normalized `/`, a length cap.
The full types belong in [`../types.md`](../types.md) when the design is accepted; they are sketched
here only to fix the names.

### 11.4 Migration and compatibility

- Global schema version 3; project schema version 2. Back up before migrating, as with every
  version. The migration is additive except the lease token hash; leases are ephemeral, so the
  upgrade stops the supervisor, clears leases, and lets surfaces re-acquire.
- Existing `node add`, `node ls`, `node rm`, `__exec`, `session ingest`, `vowp chat`, `vowp mcp`
  and the v0.3 daemon keep working. Offload is disabled by default.
- Existing surfaces have no surface token; they continue with the lease token. New local surfaces
  require a surface token. A home-only app surface is not forced to pair.
- The agent and the home negotiate the protocol at the handshake. Upgrade the home first. A v0.3
  home has no gateway; the agent fails with `unsupported_home`.
- The v0.3 `/api/v1/nodes` route keeps its `NodePage` shape; the redacted `NodeCard` is a new
  CLI/MCP document, so no v0.3 client breaks. A later API version can add the card. The full `Node`
  (with path maps) is CLI-only on the home.
- Rollback: stop the agents, revoke the nodes, set `offload.enabled = false`. The home-only path is
  unchanged. A binary downgrade cannot read the new columns, but SQLite ignores them; the lease
  hash is the only incompatible change, and leases expire. Test the downgrade on a copy.
- SQLite cannot change a `CHECK` constraint in place. The migration that adds `agent` to
  `surfaces.kind` and `leases.surface_kind` rebuilds those tables with the existing new-table,
  copy, rename pattern, row-count checks and a backup. The lease table is rebuilt empty; the
  surfaces table is rebuilt with `state`.

### 11.5 What is not stored on the Mac

`vowp.db`, `projects/*/project.db`, `sessions/*/log.jsonl`, capsules/blobs (except pending
attachments in the spool), home secrets, and other projects' data. The snapshot cache and the
pending spool are local state, not a source of truth. `vowp agent status` reads the spool for
pending counts; it does not offer a queryable session history.

## 12. CLI surface

### 12.1 Home commands

| Command | Purpose |
|---|---|
| `vowp node enroll <name> [--label <l>]... [--replace]` | create a pending node and a one-time code; `--replace` rotates a pending node's code |
| `vowp node rm <name> [--yes]` | remove a node and purge its stranded enrollment codes |
| `vowp node revoke <name> [--yes]` | revoke credentials, close, fence runs |
| `vowp node show <name> [--json]` | presence, capacity, mapped project names, runs, last error |
| `vowp surface pair --session <id> --node <name> --kind opencode [--app <app>]` | one-time surface code |
| `vowp surface ls [--node <name>] [--json]` | surfaces, scopes, expiry |
| `vowp surface rotate <id>` / `vowp surface revoke <id>` | token lifecycle |
| `vowp offload status [--json]` | nodes, runs, waiting reasons |
| `vowp offload policy [--project <p>] [--json]` | effective policy |
| `vowp job add ... --placement node:<name>` | pin a job |
| `vowp fleet status` | human output adds a node summary and waiting reasons; `--json` stays the v0.2 snapshot in the MVP |
| `vowp doctor` | checks the agent socket, gateway, credentials, path maps, token expiry, spool |

`node show --json` is a redacted `NodeCard`: it names mapped projects but never carries a local
root, worktree root, secret name or path map. The full map stays owner-only in `node_paths` and in
the agent's `agent.json`; the snapshot and the daemon never include it.

`vowp node ls --json` and the MCP `node_status` tool are the machine-readable node model in the
MVP. The snapshot stays `vowp-snapshot/1`; adding a `nodes` array is a separate schema version and
is not required for the local surface or offload.

### 12.2 Mac commands

| Command | Purpose |
|---|---|
| `vowp agent enroll [--target <ssh-target> \| --wss <URL>] --code-file <path\|-> [--project <PATH>]... [--worktree-root <PATH>]` | enroll this Mac |
| `vowp agent run [--foreground] [--role <role>]` | the daemon; role is `surface`, `executor` or `both` |
| `vowp agent status [--json]` | connection, spool, runs, paths, last error |
| `vowp agent paths add --project <PATH> [--name <n>] [--worktree-root <path>] [--allow <rel>]...` | add or update one path-map entry |
| `vowp agent paths rm <PROJECT>` | remove one path-map entry |
| `vowp agent paths ls [--json]` | local path map |
| `vowp agent pair --code-file <path> --surface <id> --session <id> [--app <app>]` | redeem a surface code |
| `vowp agent surface start --app opencode --session <id> [--project <p>] [--port <p>]` *(planned)* | start the local app host |
| `vowp agent surface attach --app opencode --session <id> [--port <p>] [--takeover] [--backfill]` *(planned)* | attach to a running app |
| `vowp agent surface detach --session <id>` *(planned)* | detach and release the lease |
| `vowp agent doctor` | local checks |
| `vowp chat --agent --session <id>` | local chat surface |
| `vowp mcp --agent-socket <path> --surface <id>` | local MCP proxy |
| `vowp agent service install` / `uninstall` / `status` | launchd or systemd user unit |

Both `--code-file` flags accept `-` for stdin, so a one-time code never appears in argv, `ps` or
shell history.

### 12.3 Hidden gateway

```sh
vowp agent gateway --stdio
```

It is the SSH command (pinned with a forced command in the hardened setup). It reads frames on
stdin and writes frames on stdout, opens the home's global store and resolves each project's
store from the session or assignment binding. It has no shell and no project checkout of its own.
It exits on EOF or protocol error; `vowp agent gateway --listen <addr>` serves the same frames on
a loopback WebSocket. A `--check` diagnostic for `doctor` is planned.

### 12.4 JSON shapes and exit codes

New documents: `NodeShow`, `NodePaths`, `AgentStatus`, `OffloadStatus`, `SurfacePair`,
`SurfaceInfo`, `AgentFrame` (debug schema). They follow the existing `vowp --json` error shape.

Exit codes reuse the v0.2 vocabulary:

- `0` success;
- `2` usage;
- `3` not found (node, project, session, surface);
- `4` conflict (already enrolled, live lease without takeover, path invalid, protocol mismatch,
  duplicate assignment);
- `5` refused (revoked, home-only, cwd refused, not a sandbox);
- `6` limit (provider limit, capacity);
- `7` remote/transport (agent offline, home unavailable, SSH failure);
- `75` a takeover/lease revocation where the caller must stop (the existing `__exec` convention).

`vowp agent status` exits 0 when it can report, even if offline, unless `--strict` is passed;
`vowp agent doctor` exits nonzero when unhealthy. No JSON document contains a long-lived token or a
secret value. The one-time enrollment and surface-pairing codes are the only exception: they are
shown once, expire in ten minutes, and are never written to a log.

### 12.5 MCP tools

The existing tools keep their names and semantics. `fleet_status` includes a node summary. A new
read-only `node_status { node? }` returns the redacted `NodeCard`. Enrollment, revocation, surface
pairing and offload-policy writes stay CLI-only on the home; they are not exposed to a local
surface. `tunnel_open`/`tunnel_close` are refused on a local surface. `secret_request` is local-only
or refused.

## 13. Compatibility and rollout

- v0.3 behavior is unchanged. The agent, offload and local surfaces are opt-in. Offload is
  `enabled = false` by default.
- The home is upgraded first; the agents second. The protocol handshake refuses a mismatch with a
  clear message; there is no silent downgrade.
- Feature flags: `[offload] enabled`, `[agent] roles`, `placement` per job/project. The owner can
  disable offload without stopping the agent; the agent can run surface-only.
- Dogfood: one Mac, one project, build/test/docs jobs, merge and deploy on the home. Observe
  reconnect rate, spool bytes, run success, gate failures and waiting reasons for a week. Then
  enable fixes/verifiers and the opencode bridge.
- Rollback: revoke the nodes, stop the agent, disable offload. The home-only path remains; leases
  expire; the local spool is inspectable; no data is deleted.
- Until the owner publishes, the project's `origin` push URL points at a local bare repository.
  The merge queue still merges and pushes locally; nothing goes to GitHub. Restore the GitHub push
  URL when the owner is ready to publish.
- The home DB migration is one-way. Back up first and keep the backup until the dogfood period
  ends.

## 14. Phased MVP and test plan

### 14.1 Phases

| Phase | Scope | Exit criteria | Not in |
|---|---|---|---|
| **0** | protocol, SSH gateway, node key, enrollment, presence, `agent status`, `node show` | a Mac enrolls, stays online, reports capacity; revoke and reconnect work; no runs yet | surfaces, offload |
| **1** | local `vowp chat`: lease, turn-begin/ingest over the agent, spool/replay, takeover | a chat turn on the Mac lands in the home log; Wi-Fi drop replays with no gap; phone takeover makes the Mac read-only | opencode app, jobs |
| **2** | offload executor: `AssignRun`, path map, worktree, events, summary, push/fetch, policy, capacity, waiting reasons | a build/docs job runs on the Mac, commits return by push, the home gates/merges; pinned offline waits; home-only secret refuses | opencode app, bundle, WSS |
| **3** | local opencode: app host, bridge, MCP proxy, orchestrator materialize, backfill, local cache | opencode on the Mac writes the same session; MCP tools proxy; takeover; paths normalized; phone continues | WSS, launchd |
| **4** | hardening: WSS ingress, launchd, token rotation UX, bundle result, separate user, metrics, docs | security review, owner live Mac smoke test, rollback drill | multi-user, Windows |

**MVP = phases 0–2.** The product goal is phase 3. If the owner wants the opencode app first,
swap phases 2 and 3; the agent transport is the same. This is open question 14 in §16.

### 14.2 MVP definition

One home, one Mac, one project, SSH transport, one local `vowp chat` surface, offload for build and
docs jobs, push result, home merge. No WSS, no opencode bridge, no bundle, no launchd, no rotation
UI (CLI rotation only), no multi-node policy UI.

Acceptance:

1. The Mac enrolls with a one-time code; the node is `active` and `online` in `vowp node ls`.
2. A `vowp chat` turn on the Mac appends to the home's `sessions/<id>/log.jsonl` with gapless `seq`
   and no duplicate event ids.
3. Killing the SSH connection mid-turn and reconnecting replays from the first unacked event; the
   log has no gap and no duplicate.
4. A phone takeover makes the Mac surface read-only within 2 seconds and writes a capsule; the Mac
   does not append after the notice.
5. A docs job with `placement = any` runs on the Mac in a worktree, pushes `job/<id>`, and the home
   fetches, gates and merges it.
6. A job with `placement = node:mac-studio` waits when the Mac is offline; it never runs on the
   home.
7. A job requiring a home-only secret is refused and shows `home_only_secret`; no secret value
   appears in a frame, log or spool.
8. `vowp node revoke mac-studio` closes the connection, fences the run and releases the lease; the
   Mac stops.
9. No absolute user-home path appears in a session event, run row or snapshot card.
10. The home-only v0.3 flows still pass their tests.

### 14.3 Test strategy

All tests are hermetic: a temporary `VOWP_HOME`, a temporary `VOWP_AGENT_HOME`, a fake transport
(in-process duplex or a fake `ssh` script), fake adapters, a fake opencode SSE server on
`127.0.0.1:0`, and real git in temporary repos. Never the real `~/.vowp`, `~/.vowp-agent`, network,
tmux server or harness.

Named tests, grouped by area:

**Enrollment and auth**

- `agent_enroll_redeems_a_one_time_code`
- `agent_enroll_refuses_an_expired_or_used_code`
- `agent_enroll_binds_the_node_public_key`
- `agent_auth_rejects_a_bad_signature_and_a_replayed_nonce`
- `agent_auth_rejects_a_revoked_node`
- `agent_second_connection_supersedes_the_first`
- `agent_rejects_a_protocol_mismatch_with_both_versions_named`

**Surface tokens and leases**

- `surface_pair_issues_a_scoped_token_and_stores_only_its_hash`
- `surface_token_cannot_write_another_session`
- `surface_token_rotation_keeps_the_old_token_for_the_overlap`
- `surface_revoke_fails_the_next_frame`
- `local_surface_acquires_and_heartbeats_the_lease`
- `takeover_pushes_lease_revoked_to_the_agent`
- `disconnected_surface_lease_is_swept_after_sixty_seconds`
- `agent_reconnect_after_takeover_is_read_only_and_discards_pending_events`

**Local chat and mirroring**

- `local_chat_turn_appends_to_the_home_log`
- `local_chat_replays_after_an_ssh_drop_without_gaps`
- `local_chat_rejects_a_duplicate_event_id_with_different_content`
- `opencode_bridge_maps_a_completed_message_once`
- `opencode_bridge_records_native_ids_and_skips_duplicates`
- `opencode_bridge_backfills_from_the_cursor`
- `opencode_bridge_normalizes_absolute_paths_to_path_refs`
- `opencode_bridge_redacts_local_injected_values_before_spooling`
- `mcp_tool_call_proxies_through_the_agent_and_checks_the_lease`
- `mcp_tool_call_works_read_only_with_the_cache_when_the_home_is_offline`
- `tunnel_tools_are_refused_on_a_local_surface`

**Offload and path mapping**

- `dispatcher_places_an_any_job_on_an_eligible_node`
- `dispatcher_waits_when_a_pinned_node_is_offline`
- `offload_refuses_a_home_only_project_and_a_denied_kind`
- `offload_refuses_a_job_that_needs_a_home_only_secret`
- `offload_requires_a_valid_path_map`
- `offload_reports_a_missing_harness_and_a_missing_env_name`
- `path_map_rejects_traversal_a_symlink_escape_and_a_case_only_escape`
- `worktree_is_created_from_the_base_commit_without_touching_the_main_checkout`
- `offloaded_run_pushes_the_job_branch_and_the_home_verifies_the_head`
- `offload_refuses_a_remote_that_does_not_match_the_project`
- `offload_fails_cleanly_when_the_push_fails`
- `node_disk_low_and_battery_draining_stop_new_runs`
- `node_provider_limit_does_not_park_home_jobs`

**CLI, gateway and agent end to end**

- `agent_paths_add_reaches_the_home_without_a_reconnect`
- `node_enroll_replace_rotates_a_pending_code_and_node_rm_yes_removes_it`
- `opencode_stdout_readiness_reaches_running_and_a_failed_start_has_a_redacted_last_error`

The three named tests run in `crates/vowp-cli/tests/agent.rs`. The path-map and stdout-readiness
tests use a real `vowp agent gateway --stdio` process and a fake `ssh` script; the stdout-readiness
test also starts the real agent daemon against a fake `opencode` script. The node-enrollment test
runs in-process through `vowp_cli::run` and uses neither. The path-map test counts the fake `ssh`
invocations and requires exactly one connection; the orchestrator fixture writes no `service.json`
before the start, then the test asserts the home card shows `running` and only afterwards that the
agent materialized a 0600 file from the app's stdout.

**Disconnect and recovery**

- `run_survives_a_short_disconnect_and_resumes_without_duplicates`
- `run_is_fenced_after_the_grace_period`
- `takeover_fences_a_disconnected_run_immediately`
- `home_restart_reconciles_disconnected_runs`
- `agent_crash_does_not_restart_the_harness`
- `a_torn_spool_tail_is_truncated_and_a_corrupt_middle_is_refused`
- `spool_cap_stops_the_run_and_records_spool_full`
- `node_revoke_fences_runs_and_releases_leases`

**Security**

- `agent_never_sends_a_secret_value`
- `agent_refuses_to_run_as_root`
- `gateway_rejects_unknown_and_oversized_frames`
- `gateway_has_no_database_access`
- `surface_token_is_absent_from_logs_errors_and_snapshots`
- `lease_token_is_stored_as_a_hash`
- `path_ref_parse_rejects_traversal_and_absolute_input`
- `node_card_contains_no_absolute_path_or_secret_name`

**Property and integration**

- `event_replay_has_no_gaps_or_duplicates` (proptest)
- `path_ref_parse_is_total_for_valid_refs_and_rejects_invalid` (proptest)
- `lease_tokens_are_unique` (proptest)
- `fake_gateway_full_turn_and_job_end_to_end`
- `fake_gateway_disconnect_takeover_and_replay_end_to_end`

### 14.4 Test matrix

| Scenario | Transport | Expected |
|---|---|---|
| enroll, reconnect, revoke | fake SSH | active → online → revoked; no orphan run |
| local chat turn | in-process duplex | one run, gapless seq, cost row |
| chat drop and replay | fake SSH, cut mid-line | no gap, no duplicate, summary once |
| takeover | two fake surfaces | old read-only <2 s, capsule, new writes |
| any job | fake agent, temp git | worktree, push, fetch, gates, merge |
| pinned node offline | fake clock | `node_offline`, no home run |
| home-only secret | fake policy | `home_only_secret`, no frame with a value |
| path escape | temp dirs + symlink | refused on the agent |
| WSS (phase 4) | local TLS proxy | same frames, same auth |

### 14.5 Dogfood and success criteria

Dogfood on the owner's Mac with the VOWP repo for one week. Success: no lost events, no duplicate
`seq`, no secret found by the scan, takeover under 2 seconds, a 1,000-event replay under 10 seconds,
no home-only job offloaded, and a clean rollback drill. Then enable fixes/verifiers, then the
opencode bridge.

## 15. Trade-offs

| Decision | Chosen | Alternative | Why | Cost |
|---|---|---|---|---|
| Transport | SSH stdio | WSS now | no new public listener; reuses keys | SSH must be reachable; extra setup |
| Agent shape | one process, two roles | surface and executor separately | simpler lifecycle and one connection | a compromised surface shares the executor's process |
| Local app turns | attached, mirrored | VOWP-driven via a plugin | no brittle interception | the home cannot control the app's prompt |
| Disconnect | spool and continue | stop on first drop | survives Wi-Fi and sleep | a bounded window of local work can be fenced |
| Paths | `PathRef` | absolute paths | privacy, portability | a type change in new rows and events |
| Results | push with local git credentials | bundle upload | simple, standard | the node needs push access |
| Offload default | opt-in | any idle node | safety | less automatic utilization |
| Tokens | hashed at rest | plaintext | a DB read does not expose live credentials | a small lease migration |
| Gateway | separate low-privilege process | in the supervisor | smaller blast radius | an extra socket/process |
| Node limits | per node | global | a Mac account must not park home jobs | more rows and UI |
| Node read model | `node ls --json` first | snapshot now | no client break | two read paths until the snapshot adds nodes |
| Local events | normalized on the Mac | raw app transcript to the home | less bandwidth; home validates | a shared mapping module to version |
| Home checkout | keep a home checkout/mirror | Mac-only project | the merge queue and gates need git | disk on the home |

## 16. Decisions and open questions

### 16.1 Decision record (2026-11-13)

The owner accepted the first four items and adopted the remaining recommendations as MVP defaults.
A default can be changed before its phase; an accepted decision needs a new owner decision to change.

| # | Question | Decision |
|---|---|---|
| 1 | Transport | **Accepted:** SSH stdio for the MVP; WSS is a Phase 4 opt-in, not a second protocol. |
| 2 | Result return | **Accepted:** the node pushes `job/<id>` with its own git credentials; bundle upload is Phase 4. |
| 3 | Phase order | **Accepted:** prove local `vowp chat` first (phases 0–2); the opencode surface is Phase 3. |
| 4 | Secrets and offload | **Accepted:** offload is opt-in per project/job; the node uses only names it resolves locally; home-only secrets stay home. |
| 5 | Offline local app | **Default:** pending spool with a 2 GiB cap and a visible "recording locally" state; discard on takeover. |
| 6 | Takeover | **Default:** fence immediately; VOWP stops recording; the app may keep editing locally. |
| 7 | Offload default | **Default:** opt-in (same as 4); no automatic offload of idle nodes. |
| 8 | Capacity | **Default:** the home brake stops home runs only; `pause_on_home_brake = true` is an option. |
| 9 | Lease token hashing | **Default:** store hashes; clear the short-lived leases during the upgrade. |
| 10 | macOS isolation | **Default:** normal user for trusted repos; a separate user/VM for untrusted work; no sandbox claim. |
| 11 | Local app scope | **Default:** opencode first; Claude Code later. |
| 12 | Orchestrator start | **Default:** support both; start the local opencode service by default for a managed orchestrator. |
| 13 | Git remote | **Default:** the Mac's `origin` must match the project remote in the MVP; a `push_remote` override is later. |
| 14 | Phase order detail | **Default:** chat-first, as in 3; swap only by a new owner decision. |

### 16.2 Open questions and rationale

The discussion below preserves the reasoning for the record and the remaining defaults. It no
longer blocks the first implementation slice.

1. **Transport.** Is SSH stdio acceptable for the MVP, with WSS only if the network blocks SSH?
   Recommended: yes. It keeps the "no public listener" invariant and reuses the existing key-only
   SSH. If you need a public ingress now, §8.6 becomes a Phase 1 item and the security review grows.
2. **Result return.** Should the Mac push `job/<id>` with its own git credentials, or should it
   upload a bundle? Recommended: push for the MVP, bundle in Phase 4. Push is simpler but requires
   push access on the Mac; a read-only node cannot offload in the MVP.
3. **Local-only projects.** Must a project exist on the home (checkout or bare mirror) in the MVP?
   Recommended: yes. A Mac-only project needs a home checkout or bare mirror plus a result-return
   path first; otherwise the home cannot run gates or merge.
4. **Secrets.** Confirm that home secret values never go to the Mac, and an offloaded job may use
   only names the Mac resolves locally. Recommended: yes. A job needing a home-only secret stays
   home.
5. **Offline local app.** Should the bridge keep a pending spool while the home is unreachable, or
   should the app be strictly read-only? Recommended: spool with a 2 GiB cap and a visible
   "recording locally" state; discard on takeover.
6. **Takeover during a local turn.** Confirm that takeover fences the run immediately and the app
   may keep editing locally, but VOWP stops recording. Recommended: yes; VOWP cannot close another
   vendor's app.
7. **Offload default.** Should offload be opt-in per project/job, or automatic for idle nodes?
   Recommended: opt-in. Automatic offload is a later policy once the dogfood data is in.
8. **Capacity.** Should the home brake pause remote dispatch too, or only home runs? Recommended:
   only home runs by default, with `pause_on_home_brake = true` as an option.
9. **Lease token hashing.** Is clearing short-lived leases during the v0.4 upgrade acceptable, or
   do you want a dual-read migration? Recommended: clear them; leases are stale in 60 seconds.
10. **macOS isolation.** Is running the agent as the owner's normal user acceptable for trusted
    repos, with a separate user/VM documented for untrusted ones? Recommended: yes; do not claim a
    sandbox.
11. **Local app scope.** Is opencode first, with Claude Code later? Recommended: yes. The bridge
    and lease work are reusable; the app-specific mapping is not.
12. **Orchestrator start.** Should VOWP start the local opencode service, or attach to an app the
    owner started? Recommended: support both; start by default for a managed orchestrator.
13. **Git remote.** Must the Mac's `origin` match the home's project remote in the MVP, or may it
    push to a fork? Recommended: same origin in the MVP; a `push_remote` override later.
14. **Phase order.** Should the MVP prove local `vowp chat` first (phases 0–2), or should the
    opencode app come before offloaded jobs? Recommended: chat first; it de-risks leases, spool and
    takeover without the app's SSE/MCP unknowns.

### 16.3 Implementation tracking

Status as of 2026-11-17. The job graph is the source of truth: "done" means the branch merged on
`main`; "running", "pending" and "cancelled" are the graph states on that date. These lists record
the graph, not a promise.

Phase 0 is four build jobs. Each build has a `review` gate, so the supervisor creates its own
verifier (`<id>-verify-<attempt>`) and the merge queue; no verify job is added by hand. The graph
order is `v4-local-0-protocol` → `v4-local-0-store` and `v4-local-0-agent-client` →
`v4-local-0-gateway`. The protocol job also waited on `v4-a1-job-actions-api` (done 2026-11-13)
because both touch the shared protocol schema.

| Phase | Job | Depends on | State | Merged on `main` |
|---|---|---|---|---|
| 0 | `v4-local-0-protocol` | `v4-a1-job-actions-api` (done) | done | `b9d25c4` vowp-protocol: add v0.4 local-node agent wire types; `90fb09e` vowp-protocol: wire SurfaceKind::Agent through downstream matches |
| 0 | `v4-local-0-store` | `v4-local-0-protocol` | done | `f6a86db` vowp-store: add the v3 global node-enrollment schema and migration; `062bbc8` vowp-store: add node credentials, enrollment codes, paths and cards; `ca74a9a` vowp-store: test the v3 migration and node-enrollment records |
| 0 | `v4-local-0-agent-client` | `v4-local-0-protocol` | done | `cff611a` vowp-agent: add the Phase 0 Mac agent client; `8cf5875` docs: regenerate the reference schema pages |
| 0 | `v4-local-0-gateway` | `v4-local-0-store`, `v4-local-0-agent-client` | done | `4003401` vowp-core: add Phase 0 enrollment, node-key auth and gateway; `001c79e` vowp-cli: add node enroll, revoke and show plus the agent gateway |

Phase 1 is complete. The graph order is `v4-local-1a-surface-format` →
`v4-local-1b-surface-core` → `v4-local-1c-surface-pair` → `v4-local-1d-lease-core` →
`v4-local-1e-local-chat`; `v4-local-1b-surface-core` also waits for the Phase 0 gateway.

| Phase | Job | Depends on | State | Merged on `main` |
|---|---|---|---|---|
| 1 | `v4-local-1a-surface-format` | — | done | `8e91582` vowp-protocol: add surface pairing frames and surface types; `f91047a` vowp-store: add surface tokens and the v4 global schema |
| 1 | `v4-local-1b-surface-core` | `v4-local-0-gateway`, `v4-local-1a-surface-format` | done | `351f8c7` vowp-core: add surface pairing and scoped surface tokens; `0ae07be` vowp-core: require an active node when redeeming a pairing code |
| 1 | `v4-local-1c-surface-pair` | `v4-local-1b-surface-core` | done | `5297a9a` vowp-cli: add home surface pairing commands and agent pair |
| 1 | `v4-local-1d-lease-core` | `v4-local-1c-surface-pair` | done | `8c66cc9` vowp-core: add hashed lease control over the agent gateway; `d8443d4` vowp-core: hash lease token comparisons in the orchestrator; `e0d305b` docs: document the orchestrator lease-hash path |
| 1 | `v4-local-1e-local-chat` | `v4-local-1d-lease-core` | done | `5eb1d20` v4-local-1e: add the local agent chat slice; `4adb4c0` v4-local-1e: fix local chat replay, Mac routing and flow control; `d4b971c` vowp-cli: dispatch agent chat before home routing |

`v4-local-1d-lease-core` took three fix rounds: the lease-token hash change broke the orchestrator
callers, and the fix that touched `crates/vowp-core/src/surface/orchestrator.rs` needed a planner
widening of the job's `owns` before it could pass `owns_respected`.

`v4-local-1e-local-chat` also landed the Mac entry point: `vowp chat --agent` is dispatched before
`Ctx::build` and before the control-machine route, so it runs from `~/.vowp-agent` and the local
checkout without a local home database; it still reads the local checkout's `vowp.toml`. The
graph's follow-up `v4-local-1e-local-chat-mac-entry` was cancelled as a duplicate after the final
fix commit landed that path.

Phase 2 is complete. All eleven slices are merged on `main`; their dependency edges are in the
table.

| Phase | Job | Depends on | State | Merged on `main` |
|---|---|---|---|---|
| 2 | `v4-local-2a-offload-format` | `v4-local-1e-local-chat` | done | `a6abeb8` vowp-protocol: add the v0.4 offload wire format — `Placement`, `WaitingReason`, the token-free `AssignmentPayload`, run reject/state/resume records and the assignment, state, resume, path-map and cleanup `AgentFrame`s. |
| 2 | `v4-local-2b-offload-store` | `v4-local-2a-offload-format` | done | `cdfd2f6` vowp-store: persist offload placement, assignments and node limits — project v2 `job_placement`, global v6 `agent_assignments` and `node_limits`, typed repository methods and migration tests; `9c88840` makes `doctor` compare the current project schema version. |
| 2 | `v4-local-2c-offload-policy` | `v4-local-2a-offload-format`, `v4-local-2b-offload-store` | done | `1f45983`, `b9cffe4`, `7d7c285`: `[offload]` config, the pure §7.2 eligibility evaluator, `vowp offload policy`/`status` and `vowp job add --placement`; fix rounds covered implicit placement rows, sibling-project leakage and `any` + `home_only`. |
| 2 | `v4-local-2d-agent-worktree` | `v4-local-2a-offload-format` | done | `45b9431`/`804bb4e`: confined `PathRef` resolution, one worktree per job with safe cleanup, push only `job/<id>`, check every push URL and strip credentials from remote errors. |
| 2 | `v4-local-2e1b-assignment-remote-url` | `v4-local-2c-offload-policy` | done | `857505a`: `AssignmentPayload` carries the credential-stripped expected remote URL so the Mac can refuse a fork; replaced the cancelled `v4-local-2e1-assignment-remote-url`. |
| 2 | `v4-local-2e-offload-dispatch` | `v4-local-2c-offload-policy`, `v4-local-2e1b-assignment-remote-url` | done | `e3b5066`/`3b2e24f`: durable assignment rows, `AssignRun` on the live gateway, accept/reject/state/lost/resume handling, hashed run leases, and the fleet-wide `offload.max_remote_runs` cap. |
| 2 | `v4-local-2f-agent-run-loop` | `v4-local-2a-offload-format`, `v4-local-2d-agent-worktree`, `v4-local-2e1b-assignment-remote-url` | done | `909835b`/`59f8555`: the `AssignRun` loop with spool-before-send, replay from the first unacked line, the 64-event credit window, `RunState`/`TurnEnd` and cancel/lost/resume/cleanup; the fix queues events spooled during resume. |
| 2 | `v4-local-2g-agent-path-map` | `v4-local-2f-agent-run-loop` | done | `883334e`: persist the Mac project path map in `agent.json` (0600), validate roots and allow lists, send it at enrollment, refresh it over `PathMapUpdate`, and add the `vowp agent paths` `add`, `rm` and `ls` subcommands. |
| 2 | `v4-local-2h-agent-harness` | `v4-local-2g-agent-path-map` | done | `9b23294`/`8302926`: the real `CoreRunHandler` resolves the path map, creates or reuses `job/<id>`, runs the adapter, streams redacted events, reports the head, pushes only the job branch and cleans up; eight named tests. |
| 2 | `v4-local-2i-result-return` | `v4-local-2h-agent-harness` | done | `5b35893`/`edb55a9`: home fetch, head/base verification, a staging worktree outside the main checkout, authoritative `owns_respected` and `secret_scan` on `base..head`, then the normal merge path; failed pushes, head/base mismatches and gate violations settle the run, release the lease and keep the job retryable. |
| 2 | `v4-local-2j-offload-e2e` | `v4-local-2i-result-return` | done | `6b8ceec`: `crates/vowp-core/tests/offload_e2e.rs` proves the whole loop with a fake gateway and adapter; `b2d38ac` makes a reconnect re-accept a disconnected assignment. |

The Phase 2 exit test is `crates/vowp-core/tests/offload_e2e.rs`. Its named §14.3 test,
`fake_gateway_full_turn_and_job_end_to_end`, covers dispatch, acceptance, a real worktree, ordered
events, push, home fetch/verify, the `owns_respected` and `secret_scan` gates and gating; the file
also covers a home-only refusal, a denied kind, a pinned offline node, a mid-run disconnect with a
gapless replay, a failed push and a head outside the job's `owns`.

Phase 3 is complete. All its slices are merged on `main`: `v4-local-3a-app-frames`,
`v4-local-3b-agent-socket`, `v4-local-3c-opencode-bridge`, `v4-local-3d-mcp-proxy`,
`v4-local-3e-test-stability`, `v4-local-3f-orchestrator-home`,
`v4-local-3g-agent-orchestrator`, `v4-local-3h-offline-cache` and
`v4-local-3j-agent-app-files`. `v4-local-3i-gateway-commands` was cancelled: the gateway command
drain was folded into the orchestrator-home path and finished by `v4-p0-1-gateway-surface-mcp`.
The Phase 3 proof table in [`../../roadmap.md`](../../roadmap.md) records one row per Phase 3 slice
with its tests; this note does not duplicate it.

**P0 local-surface repair.** The post-Phase-3 acceptance run found the home gateway dropping the
five local-surface frames and gaps in agent pairing and bridge wiring. `v4-p0-1-gateway-surface-mcp`
now handles attach/detach, native events, turns and MCP tool calls, including the home MCP tool
layer, and `v4-p0-2-agent-bridge-pair-e2e` routes pairing through the live agent socket, auto-wires
the opencode bridge for local hosts, and counts unsent bridge frames in `pending_spool_bytes`. The
real-gateway multi-project E2E proof is
[`crates/vowp-cli/tests/agent.rs`](../../../crates/vowp-cli/tests/agent.rs).

A future agent should still start from `vowp job ls --all` and `vowp graph --json`, not from this
table alone.

The supervisor owns dispatch, gates, verify, fix and merge. Track with:

```sh
vowp job ls --all
vowp job show v4-local-2a-offload-format --runs --gates
vowp graph --json
vowp fleet queue
```

The main checkout stays clean because the design note is committed. GitHub pushes remain parked:
`origin` is a local bare repository used by the merge queue, and the `github` remote keeps the real
upstream. Restore the GitHub push URL when the owner is ready to publish.

### 16.4 Framework notes: live provider caps and fix-job owns (2026-11-13; updated 2026-11-14)

**Live provider caps (resolved).** The gap was closed on 2026-11-14 by `v4-c1-live-limits`. The
capacity **target** is live: on 2026-11-13 the owner set it to 4 (fixed override) while two fix runs
were active, and neither run was stopped. The **provider concurrency cap** was not live then:
`[providers.<name>].max_concurrent` was read at supervisor startup, so changing it required a
restart, and a restart could disturb active runs. That was a framework gap, not a provider limit. At
that time, the Phase 0 protocol job stayed ready while the two fix runs held the two provider slots;
the target change alone did not create a third provider slot.

`v4-c1-live-limits` closed the gap. `core: reload provider caps and target without a restart`
(`f4550c8`) keeps the effective config in a live cell, compares the project `vowp.toml` and node
`config.toml` with the applied fingerprint once per project tick, and swaps a changed valid config
atomically. A raised `max_concurrent` frees slots on the next dispatch tick; a lowered cap stops new
admissions and never kills, interrupts or pauses an active run; an invalid or unparseable file keeps
the previous config, logs the path and error and opens one node-inbox notice. `vowp capacity reload
[--check]` validates or asks the running supervisor to apply the same path without a restart.
`core: skip the ledger row for an unchanged forced reload` (`c6fec10`) stops a reload request that
finds the config already current from writing a false applied row. The old caution to raise the cap
only in a quiet window no longer applies; `vowp capacity report` shows the effective caps and
whether the on-disk file differs from the applied config.

**Fix-job owns handoff (open).** A fix child copies its parent's `owns` when it is created
(`crates/vowp-core/src/loops/dispatch.rs`), so a verifier finding in a path outside that list
cannot be fixed in that child. `v4-local-1d-lease-core` hit this: fix-2 failed `owns_respected` on
the orchestrator files, the planner widened the original job's `owns`, and the next fix round
inherited the widened list and landed the change. The follow-up for a future job is the same
planner widening; no job in the graph tracks a code change to the handoff, and this note does not
promise one.

**Live upgrade and drain (in progress).** The current framework work is
`v4-c5-live-upgrade-drain` (running on 2026-11-14), which implements the accepted plan in
[`live-upgrade-and-drain.md`](live-upgrade-and-drain.md): defer store migrations while a live
supervisor holds the lock, add `vowp down --drain`, hand off with `vowp up --replace`, and restart
the web window or reload one project without stopping the others. It is not done: the
drain/replace/deferred-migration code is not on `main`, and nothing here claims otherwise.

## 17. Alternatives considered

- **v0.2 app host plus a tunnel.** Gives the app the home's checkout, not the Mac's. It does not
  solve offload and makes the home the place where local tools are missing.
- **Reverse tunnel to a Mac-side app server.** Outbound from the Mac, but it creates a listener on
  the Mac, a second control path, and a second lease enforcement point. Rejected.
- **A DB replica or CRDT on the Mac.** Violates one home, creates conflicts and a second writer.
  Rejected.
- **Move the session to the Mac with the bundle.** Forks the log, loses the one-home lease model,
  and leaves the phone behind. The bundle remains an escape hatch, not the everyday path.
- **A public daemon API with a bearer token.** The v0.3 daemon is read-mostly and loopback-only.
  Exposing it would be a much larger auth surface than a scoped agent protocol. Rejected.
- **Tailscale/WireGuard as the transport.** A good deployment option, but it is a network layer,
  not VOWP identity; the node key and surface tokens are still required. It can sit under SSH or
  WSS.
- **Git bundle only.** No push access needed, but a new binary path, size caps and staging storage.
  Deferred to Phase 4.
- **Run the app host on the home and sync the Mac's files.** A file sync is a second source of
  truth and a conflict generator. Rejected.

## 18. See also

- [`../v0.2/home-and-surfaces.md`](../v0.2/home-and-surfaces.md) — the normative v0.2 spec:
  §7.2 remote execution, §7.3 surfaces and leases, §7.5 app surfaces, §7.6 orchestrators, §8
  control machine.
- [`../sessions.md`](../sessions.md) — the log, capsule, switch and bundle rules.
- [`../v0.2/cross-node-sync.md`](../v0.2/cross-node-sync.md) — why the v0.2 answer is "move the
  executor, not the session", and what remains for a second home.
- [`../../security.md`](../../security.md) — assets, trust boundaries, secrets, daemon token, SSH
  and tunnels.
- [`../../guides/multi-node.md`](../../guides/multi-node.md) — control machines and executor nodes
  today.
- [`../v0.3/http-api.md`](../v0.3/http-api.md) — the read-only daemon the node card can extend.
- [`../architecture.md`](../architecture.md) — components and on-disk layout.
- [`../types.md`](../types.md) — where the proposed types land when accepted.
- [`../cli.md`](../cli.md) — the command contract to extend.
- [`../../roadmap.md`](../../roadmap.md) — where v0.4 sits after v0.3.
