# v0.5.1 — session ownership

Status: **design, 2026-11-19; not implemented.** This note is the design-first slice 3 of
[`scope.md`](scope.md). It describes the current lease model, the gap that lets a direct-engine client
bypass it, a proposed ownership model, the protocol/schema/migration impact, the security rules, and a
queue of implementation slices. It changes no code.

Every current-behaviour statement below points at a file or type in this tree. Proposed names and
commands are marked **proposed**; they do not exist yet.

## 0. Summary of the proposal

- **One record per session, not a second truth.** The existing one-row-per-session `leases` row becomes
  the *ownership* record. It gains `owner_mode` (`interactive` or `job`), `owner_epoch` and
  `expires_at`. `sessions` remains a rebuildable index and gets no duplicate owner columns.
- **A new surface kind.** `SurfaceKind::DesktopUi` (wire `desktop_ui`, store `desktop-ui`) names a UI
  that talks to an app engine itself. The opencode app host stays `Opencode`; `vowp chat` stays `Chat`;
  a worker run stays `Agent`.
- **Visible ownership + activity + friendly names (owner confirmed 2026-11-20).** A new `SessionOwner`
  read model is added to `SessionSurfaceCard` (the session card), the orchestrator endpoint document,
  `vowp fleet status`, and MCP `session_resume` / write-refusal payloads. It names the owner, its kind,
  friendly saved surface **label** (stored in the existing `surfaces` table, auto-defaulted as e.g.
  `"StackSpirit · macbook"`, `"OpenCode · macbook"`, `"Claude Code · macbook"`, and renameable via
  `--label` / `vowp surface rename`), node, holder, mode, state, **activity** (`idle` | `running` |
  `waiting`, derived from open runs/active turns), epoch and timestamps, and never contains a token.
- **Explicit takeover, with an epoch — 1-click when idle, extra confirm when running (owner confirmed
  2026-11-20).** Every acquire or takeover increments `owner_epoch` in the same transaction that replaces
  the token. Taking over an `idle` surface is a single click/confirm; taking over a `running` surface
  requires one extra confirmation because a remote or external harness cannot always be process-killed
  directly — takeover immediately fences writes, updates the `leases` table, and sends a clear
  stop/handoff notice (`LeaseRevoked` / `ownership_conflict`) so the other surface/agent knows the
  orchestrator surface switched and stops cleanly.
- **Harness / MCP / CLI awareness (owner confirmed 2026-11-20).** Harnesses without a custom UI
  (OpenCode, Claude Code, CLI) check ownership from the home DB via MCP/CLI before acting: if another
  surface owns the session, the agent briefly informs the user which surface holds it (label, node,
  `idle` vs `running`), asks whether to take over here, and on confirmation takes over (updating
  `leases`, notifying the old surface to stop, and continuing).
- **A direct-engine writer is not a supported writer until it owns.** A UI first attaches as a
  `DesktopUi` surface and takes ownership; the orchestrator endpoint reports the owner and whether the
  asking surface may write. The enforcement point for a UI that keeps using the app's HTTP API is a
  **loopback VOWP engine proxy** carrying a per-surface token; the raw engine password stays on the
  machine that runs the app. Without the proxy, the first release is *recorded and cooperative*, not
  hard-enforced. The owner chose that scope on 2026-11-20 (§6 Q1): the endpoint and attach land first
  and the proxy is deferred, so this release does **not** hard-enforce ownership against a UI that
  talks to the app's HTTP API directly.
- **Worker jobs are the same rule with a different lifetime.** A job run holds ownership in `job` mode
  for the life of the run, not for the 60-second interactive stale window. The run's existing
  `RunState`/assignment traffic renews it; expiry fences the run. An interactive takeover of a running
  job is explicit and fences the run rather than pretending to switch a human executor.

## 1. Current model

### 1.1 One lease row per session

The store table is `leases` in `crates/vowp-store/src/schema/global.sql`:

```sql
CREATE TABLE leases (
  session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
  surface_kind TEXT NOT NULL CHECK (surface_kind IN
                 ('chat','opencode','claude-code','mcp','mirror','api','agent')),
  node         TEXT NOT NULL,
  holder       TEXT NOT NULL,
  token_hash   TEXT NOT NULL,          -- lowercase hex SHA-256, never the plaintext
  acquired_at  TEXT NOT NULL,
  heartbeat_at TEXT NOT NULL
) STRICT;
```

`session_id` is the primary key, so a session has at most one lease. `Store::take_lease` in
`crates/vowp-store/src/repo_surfaces.rs` replaces it with one `INSERT ... ON CONFLICT(session_id) DO
UPDATE` inside a `BEGIN IMMEDIATE` transaction. The protocol type is `Lease` in
`crates/vowp-protocol/src/surface.rs`; its `token` field carries the stored hash when read back
(`lease_from_row`), and the plaintext appears only in a one-time grant. `hash_lease_token` is the
SHA-256 helper.

A lease is live while its heartbeat is younger than `STALE_AFTER` (60 seconds) and heartbeats every
`HEARTBEAT_INTERVAL` (5 seconds); both constants and `is_live`/`is_stale` live in
`crates/vowp-core/src/surface/lease.rs`. `sweep_leases` deletes leases older than 60 seconds and is
called from `fleet::node_tick` in `crates/vowp-core/src/fleet/mod.rs`.

Takeover is one path: `takeover_inner` in `crates/vowp-core/src/surface/takeover.rs` writes the new
`Lease`, drains the old turn for `TAKEOVER_DRAIN_SECS` (5 seconds), calls `SessionService::switch` with
`SwitchReason::Takeover` when the session already has an executor, writes the capsule and then appends
the `note` event. A stale lease can be replaced without asking; a live lease needs
`AcquireLease { takeover: true }` or `LeaseMode::Takeover`.

The lease row is also the cross-process cut-off signal: a takeover replaces the token, and an ingest
writer compares its token with the row on every line/heartbeat (`revocation_ack_for`), producing
`IngestAck::LeaseRevoked`. Paired surfaces receive `AgentFrame::LeaseRevoked` or `LeaseLost`.

### 1.2 Who can hold a lease today

| Path | Code | Lease shape |
|---|---|---|
| `vowp session lease --acquire` | `crates/vowp-cli/src/cmd/lease.rs` | `LeaseGuard::acquire`, one-shot token |
| `vowp chat` | `crates/vowp-cli/src/chat/mod.rs` and `LeaseGuard` | live guard with heartbeat thread |
| home orchestrator host | `crates/vowp-core/src/surface/orchestrator.rs` (`create`, `resume`, `keep_alive`) | `LeaseGuard` then a detached `__keepalive` process |
| paired local surface | `crates/vowp-core/src/agent/gateway.rs` (`SurfaceAttach`, `LeaseAcquire`) and `acquire_agent_lease` | `LeaseGrant`/`LeaseRevoked` over `vowp-agent/1` |
| offloaded worker run | `crates/vowp-core/src/agent/dispatch.rs` (`issue_run_lease`) | `SurfaceKind::Agent`, `surface_id: None`, holder `agent`, `RUN_LEASE_TTL` 600 s |
| remote executor turn | `crates/vowp-core/src/session/remote.rs` (`begin_turn`, `ingest`) | token carried in `TurnGrant`/`RemoteLine::Hello` |

The lease is per **session**, not per run. A run fence (`RunLost`, assignment epoch) is a separate
mechanism; `docs/design/v0.4/local-orchestrator.md` §4.4 states this explicitly.

Not every takeover goes through `takeover_inner`. The agent frame
`LeaseAcquire { mode: Takeover }` calls `acquire_agent_lease`, which replaces the row and lets the
gateway send `LeaseRevoked` and `fence_session`; it does not drain a turn or write a capsule. The MCP
`session_resume { takeover: true }` path uses the same agent path. The home CLI/chat/orchestrator path
uses `takeover_inner` and does drain/capsule. The proposed model unifies the behaviour in §2.6.

`check_lease` and `check_lease_with` in `surface/lease.rs` implement the current write rule:

```text
no lease row                     -> Ok(())          (a job session passes)
row with matching token          -> Ok(())
row with another/missing token   -> Refused("this surface is read-only; taken over by …")
```

They do not require a live heartbeat when the stored token still matches. `check_lease` reads
`VOWP_LEASE_TOKEN`; `check_lease_with` takes an explicit token. `run_turn` calls `check_lease`;
`session turn-begin`, `session ingest`, the agent gateway and the MCP write path call
`check_lease_with` or the equivalent surface checks.

### 1.3 `SurfaceKind` today

`SurfaceKind` in `crates/vowp-protocol/src/surface.rs` has seven variants:

| Wire | Store | Meaning |
|---|---|---|
| `chat` | `chat` | The `vowp chat` fallback. |
| `opencode` | `opencode` | An opencode app or server. |
| `claude_code` | `claude-code` | A Claude Code app. |
| `mcp` | `mcp` | A `vowp mcp` server. |
| `mirror` | `mirror` | A transcript mirror. |
| `api` | `api` | A direct API client. |
| `agent` | `agent` | A v0.4 local surface attached through `vowp agent`; also used by the offload run lease. |

The wire spelling is snake_case (`claude_code`); the store spelling is hyphenated (`claude-code`).
`surface_kind_to_text`/`surface_kind_from_text` in `repo_surfaces.rs` are the mapping. There is no
value for a desktop UI that connects to an app engine directly, so today it is either invisible (no
surface row) or indistinguishable from the opencode app host.

### 1.4 What `active_surface` means today

`SessionSurfaceCard` in `crates/vowp-protocol/src/snapshot.rs` has:

```rust
pub active_surface: Option<SurfaceRef>,
```

with the doc comment "The surface that holds the lease, when one is live." `SurfaceRef` is a compact
lease reference with a **non-optional** `surface_id`, kind, holder, node, `acquired_at` and
`heartbeat_at`.

The implementation does not fill it. `session_cards` in `crates/vowp-core/src/fleet/mod.rs` maps every
session with `active_surface: None`. `docs/concepts/home-and-surfaces.md` says the field is reserved
and always `null` in this build. `docs/design/v0.2/home-and-surfaces.md` §5.3 already intended it to
come from a lease whose heartbeat is younger than 60 seconds, but that part was not implemented.

Two consequences:

- The only operator view of the real owner is `vowp session lease <id>` (`cmd/lease.rs`), which prints
  holder, kind, node, `acquired_at` and `heartbeat_at`.
- A job run lease has `surface_id: None`, so it cannot be represented by `SurfaceRef` even if the
  snapshot builder were filled in.

`vowp fleet status` (`crates/vowp-cli/src/cmd/fleet.rs`, `print_status`) prints the supervisor, project
summary, capacity, limits and inbox count. It does not print sessions or owners. `--json` wraps the
whole `Snapshot` in `FleetStatus` (`crates/vowp-protocol/src/ops.rs`), so it carries the same
`active_surface: null`.

### 1.5 Exactly where a direct-engine client escapes the lease

This is the concrete path that motivates the slice. A desktop UI (for example the StackSpirit UI named
in `scope.md`) is not doing anything exotic: it follows the official endpoint document.

1. **The endpoint hands out the engine credential.** `Orchestrator::endpoint` in
   `crates/vowp-core/src/surface/orchestrator.rs` returns `OrchestratorEndpoint` with `url` and `auth`
   for a live home-hosted opencode host. `OpencodeHost::endpoint` in
   `crates/vowp-core/src/surface/opencode_host.rs` reads opencode's private `service.json` (mode 0600)
   and builds `OrchestratorAuth` with the service Basic password. `vowp orchestrator endpoint` prints
   it; `vowp agent orchestrator endpoint` does the same on a node from that node's `service.json`.
2. **The UI writes to the app's HTTP API, not to VOWP.** The UI uses `url` and `auth` directly. The
   host code already shows the app's surface: `GET /api/session`, `POST /api/session`,
   `GET /api/session/{native}/message`, and the `GET /api/event` stream (`HttpClient` and
   `start_event_listener` in `opencode_host.rs`). A UI uses the app's message/turn route. Nothing on
   that path calls `vowp`, `check_lease`, `SurfaceEvent`, `TurnBegin`, the MCP `lease_gate` or the
   gateway's `verify_surface_writer`.
3. **The recorded owner is the app host, not the UI.** The orchestrator's `Surface` is built by
   `Orchestrator::surface`: `SurfaceKind::Opencode`, holder = orchestrator name, app `opencode`,
   metadata `{"orchestrator": name}`. The UI has no surface row. `vowp session lease` therefore reports
   "opencode" while two clients are writing.
4. **The mirror is a second, unguarded writer.** `mirror_from_host` in
   `crates/vowp-core/src/surface/mirror.rs` reads the app transcript and appends directly to
   `SessionLog` with `SessionLog::append`; it never calls `check_lease`. `Orchestrator::mirror` calls it
   after `create`, `resume` and `resume_all`. Native-id dedupe (`native_events`) prevents duplicate
   mirror rows, not an unauthorised writer. A later mirror pass (`create`, `resume` or
   `resume_all`) can therefore ingest the UI's turns into the canonical log after a takeover has
   supposedly made the UI read-only.
5. **Takeover only moves the VOWP token.** `takeover_inner` replaces the `leases` row and writes a
   `note`, but it does not change opencode's service password or close the UI's HTTP connection. The
   app host's MCP server re-reads the lease token on every write and refuses, and the keeper stops; the
   UI holding the same Basic password does not. It remains writable.
6. **The node case is the same with a different reader.** A home-side endpoint for a node-hosted
   orchestrator returns `url`, `native_session_id` and `auth` as `null`; the local
   `vowp agent orchestrator endpoint` runs on the node and returns the local `service.json` password.
   The UI on that machine has the same bypass.
7. **There is no caller identity in the endpoint document.** `OrchestratorEndpoint` has no surface id
   and no write/read mode. Any local process that can run the explicit command gets the password. The
   home cannot tell the OpenCode window, the desktop UI and an unrelated local process apart.
8. **The worker path has a separate hole.** `dispatch::issue_run_lease` writes a `leases` row with
   `surface_kind = Agent`, `surface_id = None` and `holder = "agent"`, `heartbeat_at = acquired_at`,
   and `RUN_LEASE_TTL` = 600 seconds. No code path calls `heartbeat_lease` for that row. The generic
   `sweep_leases` uses the 60-second interactive `STALE_AFTER`, so a job run longer than 60 seconds
   loses its row while the run continues. After that, `check_lease_with` sees "no lease" and passes;
   a chat or UI surface can take the session with no owner row to conflict with, and the session card
   has no representation for the job anyway.
9. **A released interactive session is open.** `release_lease` deletes the row. The current
   "no lease row -> Ok" rule cannot distinguish a job session from an interactive session after a
   clean release, so a process that skips the acquire step can write. Normal surfaces acquire first,
   but the rule is not a property of the session.

Items 1–7 are the direct-engine escape the owner described. Item 8 is the worker-node counterpart.
Item 9 is the reason the proposed model distinguishes interactive from job sessions instead of
keeping a blanket "no row means no lease required".

## 2. Proposed model

### 2.1 Terms

- **Owner**: the one surface or job run allowed to write a session.
- **Ownership record**: the existing one-row-per-session `leases` row, extended with mode, epoch and
  expiry. It is the source of truth; the session card and endpoint are projections.
- **Mode**: `interactive` for a person's surface, `job` for a worker-node run.
- **Epoch**: a monotonically increasing integer assigned by the store on every acquire or takeover. It
  fences caches, mirrors and proxy tokens; a request carrying an old epoch is read-only.
- **State**: `active`, `stale` or `expired`, derived at read time from the mode and timestamps.
- **Read-only surface**: a registered surface that is not the owner. It can read and follow; it cannot
  append, start a turn, call a write tool or send an engine write request.

### 2.2 Extend the lease row; do not add a second owner table

**Decision:** extend `leases` instead of adding `session_owners` or `sessions.owner_*` columns.

Reasons:

- The table is already keyed by `session_id` and already replaced atomically. A second table would
  duplicate the same state and create a race between "who owns" and "who has the token".
- `sessions` is documented as a rebuildable index of `sessions/<id>/log.jsonl`. Putting authority on
  an index row would make a reindex able to change ownership.
- The only thing missing is *meaning* and *visibility*, not another row.

Proposed schema:

```sql
CREATE TABLE leases (
  session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
  surface_kind TEXT NOT NULL CHECK (surface_kind IN
                 ('chat','opencode','claude-code','mcp','mirror','api','agent','desktop-ui')),
  node         TEXT NOT NULL,
  holder       TEXT NOT NULL,
  token_hash   TEXT NOT NULL,          -- lowercase hex SHA-256, never the plaintext
  owner_mode   TEXT NOT NULL DEFAULT 'interactive'
                 CHECK (owner_mode IN ('interactive','job')),
  owner_epoch  INTEGER NOT NULL DEFAULT 0 CHECK (owner_epoch >= 0),
  expires_at   TEXT,                   -- required for job mode; interactive uses heartbeat_at
  acquired_at  TEXT NOT NULL,
  heartbeat_at TEXT NOT NULL,
  CHECK (owner_mode <> 'job' OR expires_at IS NOT NULL)
) STRICT;
CREATE INDEX leases_heartbeat ON leases(heartbeat_at);
CREATE INDEX leases_expires ON leases(expires_at);
```

`Store::take_ownership(session, lease, mode, expires_at)` writes the row and assigns
`owner_epoch = previous.owner_epoch + 1` inside the existing immediate transaction. The existing
`Store::take_lease` becomes a wrapper that calls it with `Interactive`/`None`, so no `Lease` literal
outside the store has to change. The caller-supplied epoch is ignored; the epoch is a store
invariant, not a caller convention.

```rust
pub enum OwnershipMode { Interactive, Job }
```

`Lease` itself stays unchanged. `SessionOwner` is the read projection that adds `mode`, `epoch`,
`expires_at` and the derived state, so the agent wire, every existing `Lease` constructor and the
`Lease` schema stay source-compatible. The `vowp-agent/1` frames keep their existing shapes.

Read model:

```rust
pub enum OwnershipState { Active, Stale, Expired }

pub enum SurfaceActivity { Idle, Running, Waiting }

pub struct SessionOwner {
    pub session_id: SessionId,
    pub surface_id: Option<SurfaceId>, // None for a job run with no paired surface
    pub kind: SurfaceKind,
    pub label: Option<String>,         // friendly saved surface name, e.g. "StackSpirit · macbook"
    pub holder: String,
    pub node: NodeName,
    pub mode: OwnershipMode,
    pub state: OwnershipState,
    pub activity: SurfaceActivity,     // Idle | Running | Waiting, derived from live runs/turns
    pub epoch: u64,
    pub acquired_at: Timestamp,
    pub heartbeat_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}
```

`Store::session_owner(session, now)` (proposed) reads the lease and derives state:

- `interactive`: `Active` while `now - heartbeat_at < 60 s`, otherwise `Stale`.
- `job`: `Active` while `expires_at > now`, otherwise `Expired`.

The state is derived at read time, so an operator sees a stale owner even before the sweep removes the
row. `SessionOwner` never contains `token_hash`, a plaintext token, or an engine password.

### 2.3 A desktop UI surface

**Proposed variant:** `SurfaceKind::DesktopUi`:

| Property | Value |
|---|---|
| Wire | `desktop_ui` |
| Store | `desktop-ui` |
| CLI spelling | `desktop-ui` |
| Display | `desktop UI` |

A desktop UI that talks to an orchestrator's app engine registers as this kind. Its `Surface` has
`app = Some("opencode")`, `holder = <UI name and instance>`, `node = <home or the app's node>`,
`cwd = <orchestrator directory>` and metadata such as
`{"orchestrator": "<name>", "engine": "opencode", "native_session_id": "<id>"}`. The exact metadata
keys are a slice decision; the invariant is that the UI is a distinct surface from the app host.

Why not reuse `Opencode` or `Api`:

- `Opencode` is the app host's own surface; reusing it would keep the operator unable to tell the
  OpenCode window from the UI.
- `Api` is a generic direct client; the UI needs a named, visible kind for takeover and audit.
- A new variant is additive on the wire but not free for an old *snapshot* reader if it appears in
  `SurfaceKind`. Section 3.4 explains how the first release avoids that.

The UI attaches and takes ownership through a **proposed** `vowp orchestrator attach <name> --holder
<name> [--surface <id>] [--takeover]` command (the exact spelling is a CLI review item). It:

1. resolves the orchestrator and its session;
2. creates or reuses a `DesktopUi` surface row for that session on the orchestrator's node
   (creating the local node row if absent, as `Orchestrator::prepare_project` already does for a
   project-less orchestrator);
3. issues a scoped surface token with `create_pending_surface`/`issue_surface_token`
   (`crates/vowp-core/src/agent/pair.rs`) or a direct home-local equivalent;
4. acquires the lease with `LeaseGuard::acquire`/`acquire_agent_lease` and the requested takeover
   mode;
5. starts a small keeper for the lease, reusing the `KEEPALIVE`/`keeper.pid` pattern in
   `surface/orchestrator.rs`, so a UI process that does not heartbeat does not go stale while the
   surface is in use;
6. returns an `OrchestratorAttach` grant: `surface_id`, one-time surface token, `SessionOwner`,
   `SessionOwner`-bearing endpoint, and expiry. The token is redacted in `Debug` and never logged.

A UI that only wants to observe does not call attach. It calls `vowp orchestrator endpoint`, gets
`owner` and `access: read_only`, and uses only read routes.

### 2.4 Session card and `fleet status`

`SessionSurfaceCard` gains one field:

```rust
#[serde(default)]
pub owner: Option<SessionOwner>,
```

`active_surface` stays in the type for one release but remains `null`, exactly as today. The new
`owner` is authoritative. This is deliberate: an old v0.4/v0.5 reader ignores an unknown `owner` field
and keeps parsing the card, while a new reader sees the full owner (including a `desktop_ui` kind or a
`surface_id: None` job). If `active_surface` were filled with a new `SurfaceKind`, an old reader would
fail to deserialize the whole snapshot. Section 6 asks the owner to confirm this compatibility choice.

`fleet::session_cards` (proposed change) reads `store.list_leases()` once, builds a map by session id,
derives `SessionOwner` for each card at `now`, and leaves `active_surface: None`. It does one query,
not one per session.

`vowp fleet status` gets a compact owner section after the project summary:

```text
owners:
  01ARZ3NDEKTSV4RRFFQ69G5FAV  orchestrator main   opencode    main@home   active   heartbeat 4s
  01ARZ3NDEKTSV4RRFFQ69G5FAW  job fix-parser      agent        mac         active   expires in 8m
```

The human output prints at most the same recent-session set the snapshot carries (200 sessions, seven
days) and never prints a token or password. `--json` already wraps the whole snapshot, so
`sessions[].owner` flows without a separate document.

`vowp session lease <id>` gains `mode`, `state`, `epoch` and `expires_at` in both human and JSON forms.
`vowp session show` (proposed, small) prints a single `owner:` block; `vowp session ls` may add an
`OWNER` column. These are operator conveniences, not new authority.

### 2.5 Orchestrator endpoint document

`OrchestratorEndpoint` in `crates/vowp-protocol/src/surface.rs` gains:

```rust
#[serde(default)]
pub owner: Option<SessionOwner>,
#[serde(default)]
pub access: EndpointAccess, // proposed enum: Owner | ReadOnly; default ReadOnly

pub enum EndpointAccess { Owner, ReadOnly }
```

Rules:

- `owner` is the live `SessionOwner` for `session_id`, or `None`.
- `access` is `read_only` unless the command was asked about a specific surface
  (`vowp orchestrator endpoint <name> --surface <id>`) and that surface is the live owner. Without a
  caller identity the safe default is `read_only`.
- The existing fields keep their meanings for backward compatibility in the first release: a live
  home-hosted opencode endpoint still has `url`, `native_session_id` and `auth` (the engine Basic
  password). New clients must treat `access` as the gate: only `owner` may write.
- In the **proxy** implementation, `url` is the VOWP loopback proxy instead of the raw engine, `auth`
  is `null` (the raw password is not published), and the attach grant carries the one-time surface
  token for the proxy's `Authorization: Bearer` header. This is a deliberate breaking change for old
  UI clients and is gated by the proxy slice.
- A node-hosted record on the home still returns all endpoint values `null`; the agent-side command
  reads the node's private files. Its `owner` is the home's owner when the agent command can see it;
  otherwise `None` plus `read_only`. The node-side proxy is a separate open question (§6).

`vowp orchestrator attach` (landed in slice 2a, 2026-11-20) is the writer path;
`vowp orchestrator endpoint [--surface <id>]` remains the metadata/read path. In 2a `url` and `auth`
still describe the raw engine (the proxy mode above is deferred), so `access` is advice to a
well-behaved client, not enforcement. `vowp agent orchestrator endpoint` keeps its current shape and gains only the
additive fields it can fill locally.

### 2.6 Read-only and takeover

The rule is the same for every surface:

> The owner is the only writer. Everyone else follows read-only.

**Interactive takeover** keeps the existing `takeover_inner` behaviour:

1. atomically replace the lease and increment `owner_epoch`;
2. immediately invalidate the old token and any proxy/keeper token tied to the old epoch;
3. drain a running turn for up to five seconds, interrupt it, write a capsule and a `switch` with
   `reason = takeover` when there is an executor;
4. append one `note` with the old holder, new holder, mode and epoch;
5. send `LeaseRevoked`/`IngestAck::LeaseRevoked` to the old surface; its next write is refused with
   the existing "this surface is read-only; taken over by …" text;
6. leave the old surface able to read: `session_tail`, `session_resume` without `takeover`,
   `fleet_status`, `fleet_queue`, `inbox_list`, `capacity_report`, `session show/export`, and read
   engine GETs if it uses the proxy.

**Job takeover** (proposed, worker slice) is explicit, not silent:

1. if the current owner is `job`, find the active `agent_assignments` row for the session;
2. call `dispatch::lose` with a reason naming the takeover, which advances the assignment to `lost`,
   closes the open run with `RunOutcome::Error`, releases the run lease, and leaves the job ready;
3. write a capsule when the session has context, then append the takeover note;
4. take interactive ownership. Do **not** call `SessionService::switch` for a job executor; a job run
   is fenced, not switched to a human surface.

This rule applies to every interactive takeover, including the agent `LeaseAcquire { mode: Takeover }`
and MCP `session_resume { takeover: true }` paths; the current agent path only fences, so the worker
slice routes it through the same capsule/note side effects (or writes the same capsule explicitly).
A takeover of an already-stale interactive lease is the normal `LeaseGuard::acquire` path and does not
need an explicit `--takeover`. A takeover of a `job` lease before `expires_at` always requires the
explicit flag.

On a clean UI release the session has no owner. A still-running orchestrator app host does not silently
re-acquire: its keeper stopped at the takeover, and an automatic re-acquire would be an unannounced
takeover of the UI's session. The owner restores it with `vowp orchestrator resume <name>` (or a UI
"hand back to OpenCode" action that runs the same path). This is part of the crash/expiry story below:
no surface is allowed to win ownership by waiting.

### 2.7 Crash, expiry and disconnect

| Situation | Interactive owner | Job owner |
|---|---|---|
| Clean release/stop | `release_lease` deletes the row; old surface is read-only; proxy token invalid | dispatcher releases on finish/reject/cancel/lost; no owner |
| Process crash (no heartbeat) | state becomes `Stale` at 60 s; snapshot shows it before sweep; next write/heartbeat is refused; another surface may acquire without `--takeover` once stale | assignment goes `disconnected`; ownership stays `Active` until `expires_at`; `sweep_expired` fences the run and releases the row |
| Expiry | no explicit expiry; 60 s stale window | `expires_at <= now`: `Expired`; the run is fenced by `sweep_expired` on the next gateway heartbeat/reconnect; a later acquire may replace it |
| Home crash/restart | rows persist; old heartbeats age naturally; no in-memory owner is trusted; `resume_all` reuses a lease only when `can_reuse_live_lease` still matches the token file | rows persist; assignments persist; reconnect `Resume` re-mints the run token/expiry for a still-current assignment |
| Node disconnect (agent) | lease stays live for the 60 s window; on reconnect `Resume`/`LeaseHeartbeat` either continues or returns `LeaseLost`; spooled events are refused, not appended | `disconnect_node` marks assignments `disconnected`; ownership stays until `expires_at`/grace; reconnect `resume_assignment` re-mints; expiry `lose` fences |
| Node revoked | `fence_node` closes runs and releases leases; proxy tokens are invalid | same; run lost, worktree kept for inspection |
| Takeover while a turn runs | existing 5 s drain, interrupt, capsule, switch, note | fence via `dispatch::lose`, capsule, note; no switch |
| Stale row not yet swept | `SessionOwner.state = stale`; `check_lease` refuses the old writer because the heartbeat is not live; new acquire is allowed | `state = expired`; the dispatcher owns cleanup; generic sweep ignores job rows |

The generic `sweep_leases` must stop treating job rows as interactive stale rows. Proposed split:

- `stale_leases` selects `owner_mode = 'interactive' AND heartbeat_at < before`.
- `sweep_leases` removes only those interactive rows.
- `dispatch::sweep_expired` owns job rows: it fences an expired assignment and releases its lease;
  the gateway heartbeat/reconnect already calls it. That is sufficient for correctness: a node that
  never reconnects leaves a visible `expired` row, and a new interactive acquire may replace it
  because it is not live. A home-tick backstop would be a separate small addition and is not required
  by this design.

`check_lease`/`check_lease_with` become stricter:

- a live interactive owner must match the token **and** have a fresh heartbeat;
- a job owner must match the token **and** be unexpired;
- an interactive session with no owner row is refused (`Refused("session has no owner; acquire
  one")`) instead of silently passing. "Interactive" means `User` or `Orchestrator`, or no role and
  no `job_id` (an imported legacy session);
- a `Planner`, `Keeper`, `Worker` or `Verifier` session with no `job_id` and no owner may still pass
  for a local home run, preserving the current "a job session passes" rule; an offloaded job must
  have a job owner row and a live assignment. This is the distinction that closes item 9 without
  breaking the supervisor's planner/keeper sessions.

The exact predicate belongs in `surface/lease.rs`; it needs the session row to distinguish roles. A
surface with a valid token but a stale/expired owner is read-only and says so.

### 2.8 Worker node versus interactive surface

The same rule applies; only the lifetime and the recovery path differ.

| | Interactive surface | Worker job run |
|---|---|---|
| Unit | One registered `Surface` (a window/CLI/UI) | One `agent_assignments` row / run |
| Mode | `interactive` | `job` |
| Owner identity | `surface_id` is set; kind is Chat/Opencode/ClaudeCode/Mcp/DesktopUi/Agent | `surface_id` may be `None`; kind `Agent`; holder names the node/job |
| Liveness | 5 s heartbeat, 60 s stale | run lifetime; `RUN_LEASE_TTL` 600 s + `RunState`/assignment renew; expiry fences |
| Write path | `run_turn`, `turn-begin`, ingest, MCP tools, proxy | `session ingest` under the run grant; `RunState`/`TurnEnd`; no MCP |
| Read-only | follow/tail/MCP read tools/read engine GETs | stop sending, keep spool, report `lost`; the job returns ready |
| Takeover | drain/interrupt/capsule/switch/note | fence `dispatch::lose`, close run, capsule/note; no switch |
| Crash | stale after 60 s; row swept | assignment `disconnected`; run lease until expiry; node reconnect resumes or loses |
| Visibility | `SessionOwner` with surface id | `SessionOwner` with `surface_id: None`, holder `agent` or `agent:<node>` |

The job run still has a normal session and a normal log; it has no second session model. The only
difference is that the owner is the run, not a person's surface. An interactive surface may **read** a
job session while it runs; it may only take it over with an explicit takeover, which fences the run.

## 3. Protocol, schema and migration impact

### 3.1 Types

| Type | File | Change |
|---|---|---|
| `SurfaceKind` | `crates/vowp-protocol/src/surface.rs` | Add `DesktopUi` (`desktop_ui`/`desktop-ui`). |
| `OwnershipMode`, `OwnershipState` | `crates/vowp-protocol/src/surface.rs` | New enums, `snake_case`. |
| `SessionOwner` | `crates/vowp-protocol/src/surface.rs` | New read model; no token. |
| `Lease` | `crates/vowp-protocol/src/surface.rs` | Unchanged: the store projection carries mode/epoch/expiry. |
| `LeaseStatus` | `crates/vowp-protocol/src/surface.rs` | Add `mode`, `state`, `epoch`, `expires_at`; keep `token` only on `--acquire`. |
| `SessionSurfaceCard` | `crates/vowp-protocol/src/snapshot.rs` | Add `owner: Option<SessionOwner>`; keep `active_surface` as-is. |
| `OrchestratorEndpoint` | `crates/vowp-protocol/src/surface.rs` | Add `owner` and `access` with safe defaults. |
| `EndpointAccess` | `crates/vowp-protocol/src/surface.rs` | New `owner`/`read_only` enum (proposed name). |
| `OrchestratorAttach` | `crates/vowp-protocol/src/surface.rs` | New attach grant: surface id, one-time surface token (redacted `Debug`), owner, endpoint, expiry. |
| `TakeoverReport` | `crates/vowp-protocol/src/surface.rs` | Unchanged; the post-takeover `SessionOwner` carries the new mode/epoch. |
| `Snapshot` | `crates/vowp-protocol/src/snapshot.rs` | No new field beyond `sessions[].owner`; version stays `1` in the first release. |
| `FleetStatus` | `crates/vowp-protocol/src/ops.rs` | No shape change; it embeds `Snapshot`. |

`SessionOwner` and `OrchestratorAttach` must be added to the `vowp schema` registry in
`crates/vowp-protocol/src/schema.rs`; otherwise `docs/reference/generate.py` cannot see them.

### 3.2 JSON schema pages

Generated by `python3 docs/reference/generate.py` from `vowp schema <Type>`; do not hand-edit.

| Page | Change |
|---|---|
| `docs/reference/schema/lease.md` | `SurfaceKind` gains `desktop_ui`; the `Lease` fields are unchanged. |
| `docs/reference/schema/lease-status.md` | New owner fields. |
| `docs/reference/schema/session-surface-card.md` | New `owner`; `active_surface` unchanged. |
| `docs/reference/schema/snapshot.md` | Nested `SessionOwner`; `sessions[].owner`. |
| `docs/reference/schema/fleet-status.md` | Nested snapshot change. |
| `docs/reference/schema/orchestrator-endpoint.md` | New `owner`, `access`. |
| `docs/reference/schema/session-owner.md` | New page. |
| `docs/reference/schema/ownership-mode.md`, `ownership-state.md`, `endpoint-access.md` | New pages. |
| `docs/reference/schema/orchestrator-attach.md` | New page. |
| `docs/reference/schema/orchestrator-auth.md` | Unchanged for the direct path; proxy mode sets `auth: null` and uses the attach token. |
| `docs/reference/schema/README.md` | New rows. |
| `docs/reference/cli.md` | New `orchestrator attach` / `session` owner output; regenerate after the CLI slice. |

The generated pages are not a public compatibility promise by themselves; the wire types are. A
`vowp schema` page that is stale is a gate failure (`docs/reference/check-reference.py`).

### 3.3 Store tables and migration

`GLOBAL_SCHEMA_VERSION` is `7` in `crates/vowp-store/src/db.rs`; the next migration is v7 → v8.

- `crates/vowp-store/src/schema/global.sql`: update the `surfaces.kind` CHECK and the `leases`
  definition. SQLite cannot alter a CHECK constraint with `ALTER TABLE`, so the v7 → v8 migration must
  **rebuild** `surfaces` and `leases` (create the new table, copy rows, drop the old, rename, recreate
  `leases_heartbeat` and `leases_expires`) while preserving the `surface_tokens` foreign key. Run
  `PRAGMA foreign_key_check` and `PRAGMA integrity_check` before commit. `surface_tokens.surface_id`
  references `surfaces(id)` with `ON DELETE CASCADE`, so a naive `DROP TABLE surfaces` would delete
  every token; disable foreign-key enforcement around the rebuild (outside the transaction, because
  the pragma is a no-op inside one), copy parent and child rows in order, and re-check before commit.
- `crates/vowp-store/src/migrate.rs`: add `migrate_v07`, `migrate_v07_if_needed`, `V08_NEW_TABLES = []`,
  and the v7 source validation. Copy `surfaces`, `surface_tokens` and `leases`; keep every row and
  every id. Write the backup first (`pre-0.8-<ms>.db`).
- Existing rows:
  - `surfaces.kind` stays as-is; the rebuild only widens the CHECK.
  - `leases` rows default to `owner_mode = 'interactive'`, `owner_epoch = 0`, `expires_at = NULL`.
  - a row with `surface_kind = 'agent' AND surface_id IS NULL` is an old offload run lease. The
    migration should set `owner_mode = 'job'` and `expires_at = heartbeat_at + 600 s` (or clear it and
    let the dispatcher re-mint; choose one and test it). A row that is already past its computed
    expiry is fenced by the dispatcher on the next reconnect, not silently deleted while the run is
    active.
  - compatibility shim: a lease written after the migration by the old `take_lease` wrapper, but with
    `surface_kind = 'agent'` and `surface_id = NULL`, is still read as job mode with
    `expires_at = heartbeat_at + 600 s`. The interactive stale query must also exclude that shape, so
    a legacy run lease is not deleted after 60 seconds. This keeps a long-running offload job from
    being judged "stale" between the foundation slice and the worker slice; slice 3 then writes the
    explicit columns.
  - interactive rows keep their token hash and acquired time; a live surface keeps working, and a
    dead one goes stale on the normal 60 s clock.
- New code reads old v7 rows only after the migration; old binaries opening v8 get
  `StoreError::SchemaTooNew` and do not write.
- No project database change (`PROJECT_SCHEMA_VERSION` stays `2`); no session log format change.

The existing v4 → v5 migration (`migrate_v04`) is the precedent for clearing ephemeral leases when a
token representation changes. This migration is additive and should **not** clear rows: the token hash
format is unchanged.

### 3.4 Backward compatibility with installed v0.4/v0.5 agents

- **Agent protocol stays `vowp-agent/1`.** No new required frame, field or handshake. The home accepts
  an old agent connection, sends the existing `LeaseGrant`/`LeaseRevoked`/`LeaseLost`, and the old
  agent keeps working. The new owner fields do not appear in any agent frame.
- **Job lease heartbeat is home-side.** The home can renew a run's ownership from the existing
  `RunState` frames and accepted remote lines (both already sent by v0.4/v0.5 agents) and from
  assignment events. An old agent does not need a new heartbeat frame. If a run is completely idle,
  the assignment expiry fences it; the behavior is still explicit, not silently open.
- **Snapshot fields are additive.** `SNAPSHOT_VERSION` stays `1`; `SessionSurfaceCard.owner` is a new
  optional field. A v0.4/v0.5 control CLI ignores it. `active_surface` stays `null`, so an old reader
  never sees the new `desktop_ui` enum value in a field it decodes.
- **Endpoint fields are additive.** An old UI ignores `owner`/`access`. It keeps its current behavior
  until the proxy slice changes `url`/`auth`; that change is intentionally breaking and should be
  gated by an explicit owner decision (§6).
- **Store downgrade is refused.** A v0.5.1 binary migrates v7 to v8 with a backup; a v0.4/v0.5 binary
  refuses v8 with `SchemaTooNew` instead of misreading it.
- **`Lease` is unchanged.** The new mode/epoch/expiry data lives in the lease row and the
  `SessionOwner` projection, so every existing `Lease` constructor and `Lease` JSON document stays
  source-compatible. `LeaseStatus` gains fields with serde defaults; old JSON documents still parse.
- **No new `PROTOCOL_VERSION` bump.** `PROTOCOL_VERSION` (`crates/vowp-protocol/src/lib.rs`) is for
  incompatible wire shapes; this change is additive on the read side and unchanged on the agent wire.
  A node-side proxy that needs new agent frames would be a separate `vowp-agent/2` decision.

## 4. Security

### 4.1 Tokens and storage

| Secret | Where it lives | Storage | Exposure |
|---|---|---|---|
| Lease token | holder memory / 0600 grant file | SHA-256 in `leases.token_hash` | one-time in `LeaseGrant`, `LeaseStatus.token` on `--acquire`, `TurnGrant.lease_token`; never snapshot/log |
| Surface token | paired surface's private state / UI memory | SHA-256 + 12-char prefix in `surface_tokens` | one-time in `SurfaceIssued`/attach grant; never `surface ls` |
| Engine Basic password | `service.json` 0600 on the app machine | opencode's own service state | only the explicit local endpoint document; custom `Debug` redacts; never a card/snapshot/log |
| Proxy token (proposed) | UI memory | same surface token hash or a dedicated hash | one-time in attach grant; `Authorization: Bearer`; never URL/log |

The attach grant's token must use a custom `Debug` like `OrchestratorAuth` (password `[REDACTED]`) and
must not be serialized into `snapshot.json`, `fleet status`, `orchestrator ls`, a result frame, a
log line or an error string. `SessionOwner` deliberately carries no secret.

### 4.2 Loopback only

- The app host binds `127.0.0.1` only (`opencode_host.rs` `loopback_address` refuses a non-loopback
  host; the service is started with a loopback port). The proxy must do the same.
- A remote UI reaches it through the owner's SSH tunnel, exactly as `join.desktop` does today. No
  public listener, no LAN address, no hostname and no pairing link in a card/snapshot/log.
- The endpoint document's URL is `http://127.0.0.1:<port>` or `http://localhost:<port>` only; the home
  canonicalises to `127.0.0.1` and refuses `0.0.0.0`, `https`, a missing port and zero.
- The proxy must bind before the endpoint is published, and stop with the host. A stale proxy URL is
  not a live endpoint.

### 4.3 What a read-only surface may still do

Allowed:

- read the session log: `vowp session tail`, `session show`, `session export`, the daemon/API read
  routes, and `session_resume` **without** `takeover` (the home MCP read path must not require the
  stale lease token for a read; only `takeover` gates on the lease);
- read fleet state: `fleet_status`, `fleet_queue`, `inbox_list`, `capacity_report`;
- follow new events in `vowp chat --follow` or a UI follow mode;
- read the app engine over the proxy with GET/HEAD and the `/api/event` stream;
- show the owner's name and state in its own chrome.

Refused:

- `run_turn`, `session turn-begin`, `session ingest`, `session handoff`;
- every MCP write tool (`inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_set`,
  `session_handoff`);
- engine POST/PUT/PATCH/DELETE, including a new turn, an abort or a permission answer, when the proxy
  is in force;
- claiming/taking over without the explicit owner action;
- starting, stopping or moving an orchestrator host.

The read-only state is not a UI convention: the same refusal must come from `check_lease`,
`verify_surface_writer`, `mcp/backend::lease_gate` and the proxy. A read-only surface with a valid
surface token still has its read scopes; the lease is what removes write authority. `session_resume`
with `takeover` is the explicit takeover path, not a read-only operation; a surface in follow mode
does not call it. The home owner's `vowp session switch` and `vowp orchestrator resume` are control
commands, not a surface write; they keep their existing owner authority and are not exposed to a
read-only surface.

### 4.4 Honest limits

- A local process running as the same OS user can still read a mode-0600 `service.json` if it is the
  same user (or root). VOWP cannot make a local co-tenant harmless. The design removes the *supported*
  bypass and makes the accidental second writer visible and refusable; it does not claim a sandbox.
- The opencode desktop window reached through `opencode pair` or a raw `--server` URL is an unmanaged
  engine client. The proxy only covers clients that use the VOWP proxy endpoint. To make the OpenCode
  window itself read-only, the join instructions must send it through the proxy (one token per window)
  or VOWP must rotate the engine credential/restart the host; restarting kills an in-flight turn, so
  it is not automatic.
- A client that keeps an old Basic password can keep writing until the engine password is rotated or
  the host is stopped. The owner must decide whether v0.5.1 rotates the password on takeover or waits
  for the proxy (§6).
- The agent-side proxy for a node-hosted orchestrator needs a node-local ownership view and possibly a
  new agent frame; an installed v0.4/v0.5 agent cannot enforce it. The first release records ownership
  and makes the paired agent surface read-only; hard node enforcement is a follow-up.

## 5. Implementation slices

Four jobs. Slice 1 is deliberately broad because the ownership record, the wire enum and the read
model are one compile unit: `SurfaceKind` has exhaustive matches in `vowp-store`, `vowp-core` and
`vowp-cli`, and `LeaseStatus`/`SessionSurfaceCard` have constructors. A protocol-only or store-only
job would leave the workspace uncompilable. Slices 2 and 3 are independent after slice 1 and can run
in parallel; slice 4 is documentation.

| # | Slice | Size | Depends on |
|---|---|---|---|
| 1 | Ownership foundation: protocol, store, core checks and read model | L | — |
| 2 | Orchestrator endpoint, desktop UI attach and engine proxy | L | 1 |
| 3 | Worker/job ownership lifetime and takeover | M | 1 |
| 4 | Operator and design documentation | S | 1, 2, 3 |

The generated `docs/reference/schema/` tree is a shared artifact: every slice that changes a protocol
type runs `python3 docs/reference/generate.py` and commits the changed pages. `README.md` in that tree
is an index; it is refreshed by the last slice that adds a type. Treat the generated directory as one
artifact, not a code-ownership boundary.

One sequential overlap is unavoidable: the endpoint shape lives in the same protocol file as the
ownership types. Slice 1 and slice 2 both touch `crates/vowp-protocol/src/surface.rs`,
`crates/vowp-protocol/tests/surface.rs`, `crates/vowp-protocol/src/schema.rs` and the generated schema
index. Run slice 1 before slice 2; never run them concurrently. Slices 2 and 3 are disjoint from each
other and may run in parallel after slice 1.

### Slice 1 — Ownership foundation: protocol, store, core checks and read model

**Size:** L.

**Owns**

- `crates/vowp-protocol/src/surface.rs`
- `crates/vowp-protocol/src/snapshot.rs`
- `crates/vowp-protocol/src/schema.rs`
- `crates/vowp-protocol/tests/surface.rs`
- `crates/vowp-protocol/tests/snapshot.rs`
- `crates/vowp-protocol/tests/schema.rs`
- `crates/vowp-store/src/schema/global.sql`
- `crates/vowp-store/src/db.rs`
- `crates/vowp-store/src/migrate.rs`
- `crates/vowp-store/src/repo_surfaces.rs`
- `crates/vowp-store/tests/agent.rs`
- `crates/vowp-store/tests/migration.rs`
- `crates/vowp-store/tests/schema_enums.rs`
- `crates/vowp-core/src/surface/lease.rs`
- `crates/vowp-core/src/surface/takeover.rs`
- `crates/vowp-core/src/surface/mirror.rs`
- `crates/vowp-core/src/surface/mcp.rs`
- `crates/vowp-core/src/session/service.rs`
- `crates/vowp-core/src/session/remote.rs`
- `crates/vowp-core/src/fleet/mod.rs`
- `crates/vowp-core/tests/surface_lease.rs`
- `crates/vowp-core/tests/mirror.rs`
- `crates/vowp-core/tests/remote_exec.rs`
- `crates/vowp-core/tests/fleet_snapshot.rs`
- `crates/vowp-cli/src/cmd/lease.rs`
- `crates/vowp-cli/src/cmd/fleet.rs`
- `crates/vowp-cli/src/cmd/session.rs`
- `crates/vowp-cli/src/chat/mod.rs`
- `crates/vowp-cli/src/mcp/backend.rs`
- `crates/vowp-cli/tests/session.rs`
- `crates/vowp-cli/tests/status.rs`
- `crates/vowp-cli/tests/mcp.rs`
- `docs/reference/schema/session-owner.md`
- `docs/reference/schema/ownership-mode.md`
- `docs/reference/schema/ownership-state.md`
- `docs/reference/schema/lease.md`
- `docs/reference/schema/lease-status.md`
- `docs/reference/schema/session-surface-card.md`
- `docs/reference/schema/snapshot.md`
- `docs/reference/schema/fleet-status.md`
- `docs/reference/schema/README.md`

**Work:** add `SurfaceKind::DesktopUi` and update every exhaustive match/mapping
(`surface_kind_name`, `surface_kind_to_text`/`from_text`, the chat and `session lease` kind names, the
`desktop-ui` parser); add `OwnershipMode`, `OwnershipState`, `SessionOwner`; add `SessionSurfaceCard.owner`
and `LeaseStatus` owner fields with safe defaults; extend the `leases` row with `owner_mode`,
`owner_epoch`, `expires_at`; widen the `surfaces.kind` and `leases.surface_kind` CHECKs; implement
`Store::take_ownership`, `session_owner`, interactive-only stale selection, job expiry selection and
`renew_job_lease`; migrate v7 → v8 with a backup and a table rebuild; make `check_lease`/
`check_lease_with` role- and liveness-aware; make takeover increment the epoch and write one note;
fence the mirror (extend `MirrorCursor` with the owner epoch/surface and refuse a pass that does not
match the live owner); make the home MCP read path allow a stale lease token for `session_resume`
without `takeover` (only the write/takeover path gates on the lease); fill `SessionOwner` in
`fleet::session_cards`; add the `fleet status` owner section and the `session show`/`session ls` owner
view.

**Tests**

- `surface_kind_desktop_ui_round_trips_as_desktop_ui`
- `lease_owner_fields_default_for_an_old_document`
- `session_card_without_owner_still_parses`
- `surfaces_and_leases_accept_desktop_ui_and_job_mode`
- `take_lease_increments_owner_epoch_once_per_replace`
- `interactive_stale_sweep_ignores_an_unexpired_job_lease`
- `legacy_agent_run_lease_is_not_selected_as_interactive_stale`
- `expired_job_lease_is_selected_only_after_expires_at`
- `v7_home_migrates_to_v8_preserving_surfaces_tokens_and_leases`
- `v7_agent_run_lease_becomes_job_mode_with_an_expiry`
- `v8_migration_is_idempotent_and_writes_a_backup_first`
- `desktop_ui_takeover_invalidates_the_previous_token_immediately`
- `read_only_surface_refuses_a_turn_and_an_ingest_line`
- `user_or_orchestrator_session_without_an_owner_refuses_a_write`
- `planner_and_keeper_sessions_without_an_owner_still_run_on_the_home`
- `stale_interactive_owner_is_refused_before_the_sweep`
- `mirror_pass_refuses_a_session_owned_by_another_surface`
- `takeover_writes_one_note_with_the_new_epoch`
- `released_interactive_session_has_no_owner_and_refuses_a_write`
- `session_card_reports_the_live_interactive_owner`
- `session_card_reports_a_job_owner_with_no_surface_id`
- `session_card_shows_a_stale_owner_before_the_sweep`
- `fleet_status_human_lists_active_owners`
- `snapshot_owner_never_contains_a_token_or_password`
- `session_lease_accepts_desktop_ui_and_prints_mode_state_epoch`
- `session_show_and_ls_report_the_owner`
- `mcp_read_tools_work_read_only_and_write_tools_refuse_after_takeover`
- `mcp_read_resume_works_after_takeover_without_takeover`
- `schema_export_covers_lease_session_owner_and_session_card`

**Evidence:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test -p vowp-protocol -p vowp-store -p vowp-core -p vowp-cli` (or the equivalent per-target
commands), `python3 docs/reference/generate.py --check`, `python3 docs/reference/check-reference.py`.
The migration tests build real v7 temporary homes; no test touches the real `~/.vowp` or the network.

### Slice 2 — Orchestrator endpoint, desktop UI attach and engine proxy

**Size:** L.

**Status (2026-11-20): split.** Slice **2a** lands the endpoint owner/access read model
(`owner`, `access`, `vowp orchestrator endpoint --surface`) and `vowp orchestrator attach`
(`OrchestratorAttach` grant, a `DesktopUi` surface, the existing lease token, fencing of a job owner
through `dispatch::fence_job_owner`). `attach` holds the lease and heartbeats until stdin reaches EOF;
there is no detached keeper and no `--detach` while §6 Q8 is open. The loopback engine proxy, the proxy
token, the `auth: null` mode and the `engine_proxy` tests are **deferred** to a later job; a node-hosted
orchestrator cannot be attached from the home and node enforcement stays a follow-up. Until the proxy
lands, recorded ownership is cooperative: a UI that calls the app's HTTP API directly is not stopped.
The proxy and keeper items in the lists below are the deferred part.

**Owns**

- `crates/vowp-protocol/src/surface.rs` (endpoint types; sequential after slice 1)
- `crates/vowp-protocol/src/schema.rs` (endpoint schemas; sequential after slice 1)
- `crates/vowp-protocol/tests/surface.rs` (endpoint constructors; sequential after slice 1)
- `crates/vowp-core/src/surface/orchestrator.rs`
- `crates/vowp-core/src/surface/opencode_host.rs`
- `crates/vowp-core/src/surface/mod.rs`
- `crates/vowp-core/src/surface/engine_proxy.rs` (new)
- `crates/vowp-core/tests/orchestrator_endpoint.rs`
- `crates/vowp-core/tests/engine_proxy.rs` (new)
- `crates/vowp-agent/src/orchestrator.rs`
- `crates/vowp-agent/tests/orchestrator.rs`
- `crates/vowp-cli/src/cmd/orchestrator.rs`
- `crates/vowp-cli/src/cmd/agent.rs`
- `crates/vowp-cli/src/cli.rs`
- `crates/vowp-cli/src/cmd/mod.rs`
- `crates/vowp-cli/tests/orchestrator.rs`
- `crates/vowp-cli/tests/agent.rs`
- `docs/reference/schema/orchestrator-endpoint.md`
- `docs/reference/schema/orchestrator-auth.md`
- `docs/reference/schema/orchestrator-attach.md`
- `docs/reference/schema/endpoint-access.md`
- `docs/reference/schema/README.md` (shared index; sequential after slice 1)

**Work:** add `owner` and `access` to `OrchestratorEndpoint`; add the proposed
`vowp orchestrator attach` command and `OrchestratorAttach` grant; create the `DesktopUi` surface,
issue a scoped surface token, acquire/take over the lease, start and stop a lease keeper; add the
loopback engine proxy that forwards reads and gates writes on the live owner/epoch; keep the engine
password private; bind loopback only; expose the proxy URL only with the attach token; keep the
stopped, node-hosted and corrupt-service cases path-free and password-free.

**Tests**

- `endpoint_reports_owner_and_read_only_access_without_a_surface`
- `endpoint_tells_the_owner_surface_it_may_write`
- `attach_creates_a_desktop_ui_surface_and_returns_a_one_time_token`
- `attach_conflicts_with_a_live_owner_and_takeover_replaces_it`
- `attach_starts_and_stops_a_lease_keeper`
- `proxy_allows_get_and_refuses_a_non_owner_post`
- `proxy_forwards_an_owner_post_and_rotates_on_takeover`
- `proxy_binds_loopback_and_never_exposes_the_engine_password`
- `endpoint_and_snapshot_never_carry_the_auth_password`
- `proxy_survives_a_stopped_or_corrupt_service_file_without_leaking_a_path`
- `agent_endpoint_reports_read_only_without_a_home_owner_view`
- `orchestrator_attach_debug_redacts_the_surface_token`

**Evidence:** `cargo test -p vowp-core --test orchestrator_endpoint --test engine_proxy`,
`cargo test -p vowp-agent --test orchestrator`, `cargo test -p vowp-cli --test orchestrator
--test agent`. Proxy tests use a fake upstream HTTP server on `127.0.0.1:0`; no real opencode,
network, app or `~/.vowp`.

### Slice 3 — Worker/job ownership lifetime and takeover

**Size:** M.

**Owns**

- `crates/vowp-core/src/agent/gateway.rs`
- `crates/vowp-core/src/agent/dispatch.rs`
- `crates/vowp-core/src/agent/result.rs`
- `crates/vowp-core/tests/agent.rs`
- `crates/vowp-core/tests/offload_dispatch.rs`
- `crates/vowp-core/tests/offload_e2e.rs`
- `crates/vowp-core/tests/offload_result.rs`

**Work:** write job mode and an expiry in `issue_run_lease`; renew the job ownership from `RunState`,
accepted remote lines and assignment activity; exclude job rows from the interactive 60 s sweep; fence expired assignments
and release their lease; keep the assignment `disconnected` path; implement explicit interactive
takeover of a job through `dispatch::lose`, capsule and note; align the agent
`LeaseAcquire(Takeover)`/`session_resume` path with the same interactive capsule/note rule; release
on accepted result, reject, cancel and lost; keep the run fence separate from the session lease.

**Tests**

- `run_lease_is_job_mode_and_survives_past_sixty_seconds`
- `run_state_renews_the_job_ownership_expiry`
- `node_disconnect_keeps_the_job_owner_until_the_assignment_expires`
- `expired_assignment_fences_the_run_and_releases_the_owner`
- `interactive_takeover_of_a_job_fences_the_run_and_writes_a_capsule`
- `accepted_result_releases_the_job_owner`
- `a_late_run_frame_after_expiry_is_fenced`

**Evidence:** `cargo test -p vowp-core --test agent --test offload_dispatch --test offload_e2e
--test offload_result`.

### Slice 4 — Operator and design documentation

**Size:** S.

**Owns**

- `docs/concepts/home-and-surfaces.md`
- `docs/guides/operating.md`
- `docs/design/types.md`
- `docs/design/v0.4/local-orchestrator.md`
- `docs/design/v0.5.1/scope.md`
- `docs/reference/cli.md`
- `docs/roadmap.md`

**Work:** replace the "reserved and always null" text with the implemented owner field; document
attach/claim, read-only mode, job ownership and the proxy; update the v0.4 lease section and the
v0.5.1 slice status; regenerate the CLI page; do not claim hard node enforcement before it lands.

**Tests/checks:** `docs/tests/run.sh`, `python3 docs/reference/generate.py --check`,
`python3 docs/reference/check-reference.py`.

## 6. Open questions for the owner

1. **Is the proxy in v0.5.1, or is recorded/cooperative ownership enough for the first ship?**
   **Owner answer (2026-11-20): proxy deferred.** Land the endpoint and attach read/write path now
   (slice 2a); recorded ownership stays cooperative for a UI that talks to the app's HTTP API
   directly, and the docs say so. The proxy is a later job.
   Recording ownership and filling the endpoint/card/fleet is small. Hard read-only enforcement for a
   UI that keeps using the app's HTTP API requires the loopback proxy, and the OpenCode window remains
   a raw bypass unless it is launched through the proxy. Recommendation: land slice 1 and the
   endpoint/attach part of slice 2 first; then decide whether the proxy must ship in v0.5.1. If it is
   deferred, split slice 2 into "endpoint and attach" now and a later proxy job, and do not claim hard
   enforcement before it lands.
2. **Does v0.5.1 rotate the opencode service password on takeover (or restart the host) to invalidate
   an old Basic client?** Recommendation: no automatic restart; a restart kills a running turn. Use
   the proxy for new clients and rotate only on explicit owner request.
3. **What is the socket/ownership story for a node-hosted orchestrator?** A home-only proxy cannot
   intercept a UI talking to the node's loopback engine. Options: a node-local proxy in the agent
   (needs a `vowp-agent/2` or an existing frame extension), or a recorded-only first release.
   Recommendation: home-hosted proxy first; node enforcement in a follow-up with an explicit agent
   protocol decision.
4. **Name and spelling of the new kind.** Recommendation: `desktop_ui` on the wire, `desktop-ui` in
   the store/CLI. If the owner prefers `ui`, decide before slice 1 because it is a wire value.
5. **Can an interactive surface take over a running job?** Recommendation: yes only with an explicit
   `--takeover`, and it fences the run (`dispatch::lose`), writes a capsule, and leaves the job ready.
   A silent takeover of a running job would lose work without a visible decision.
6. **Should ownership be copied onto the `sessions` row for query convenience?** Recommendation: no.
   The lease row is the atomic authority; a second copy can disagree after a crash. If a `sessions`
   projection is wanted, it should be a read-only cache rebuilt from `leases`, not a write source.
7. **Keep the `active_surface` field or retire it?** Recommendation: keep it for one release, leave
   it `null`, and put the owner in the new `owner` field. This lets an installed v0.4/v0.5 control CLI
   keep parsing a v0.5.1 snapshot. If the owner prefers `active_surface` to be the one visible field,
   bump `SNAPSHOT_VERSION` to 2 and accept that old control CLIs refuse the new snapshot.
8. **Who heartbeats a desktop UI's lease?** Recommendation: the proposed `orchestrator attach` starts a
   small keeper on the machine that owns the lease, using the existing `KEEPALIVE`/`keeper.pid` pattern,
   so a UI process without a heartbeat thread does not flap. Confirm that a keeper process is
   acceptable for a desktop app.
9. **One surface per UI window or one per UI application?** Recommendation: one per process/window,
   with a holder like `stackspirit-laptop-1`, so two windows are visible and individually fenced.
10. **What is the audit requirement?** Recommendation: the existing session `note` plus the lease row
    is enough for v0.5.1; an append-only `session_ownership_events` table is a later retention/audit
    decision, not part of this slice.
11. **Permissions and approvals (v0.6 ACP).** A read-only surface cannot answer a permission prompt.
    Recommendation: route permission prompts only to the owner; a read-only surface shows them
    disabled. Confirm this matches the owner's expectation before the v0.6 permission work.
12. **Should a released interactive session remain open to a writer with no acquire step?**
    Recommendation: no. The proposed `check_lease` refuses an interactive session (`User`,
    `Orchestrator`, or no role and no `job_id`) with no owner; a local
    planner/keeper/worker/verifier session keeps the old pass. This is a behavior change, so confirm
    it.
13. **Is the new command called `orchestrator attach` or `orchestrator claim`?**
    **Owner answer (2026-11-20): `attach`.** `vowp orchestrator attach <name>` creates the `DesktopUi`
    surface, returns the one-time grant and is the writer path; `vowp orchestrator endpoint` stays the
    metadata/read path. Recommendation:
    `attach`, because it creates a surface and returns an attach grant; `claim` sounds like a one-time
    ownership grab. Decide before slice 2 because the command name is in the CLI contract.

## Appendix A — Rejected alternatives

- **A separate `session_owners` table.** Rejected: duplicates the one-row-per-session authority and
  creates a transaction window where owner and token disagree. The lease row already has the right
  key and atomicity.
- **Owner columns on `sessions`.** Rejected: `sessions` is a rebuildable index; a reindex or import
  could change authority. If a cache is later needed, derive it from `leases`.
- **Reuse `SurfaceKind::Opencode` for the UI.** Rejected: the operator cannot tell the app host from
  the UI, and the app host's `surface_id` is the only one that can own the current lease.
- **Make `active_surface` the only new field.** Rejected: `SurfaceRef.surface_id` is non-optional, so
  a job owner cannot be represented; a new `SessionOwner` with `Option<SurfaceId>` covers both.
- **Rely on the UI to check `access`.** Rejected as the only enforcement: it is cooperative. The
  proxy is the enforcement point for a client that can reach the engine's HTTP API.
- **Use the 60-second stale window for job runs.** Rejected: it was the observable bug in the current
  code. A worker job is live for the run, and its expiry is the assignment/run lease TTL, not the
  interactive heartbeat window.

## Appendix B — References

- `crates/vowp-protocol/src/surface.rs` — `SurfaceKind`, `Lease`, `LeaseStatus`, `TakeoverReport`,
  `OrchestratorEndpoint`, `OrchestratorAuth`.
- `crates/vowp-protocol/src/snapshot.rs` — `SessionSurfaceCard`, `Snapshot`.
- `crates/vowp-protocol/src/ops.rs` — `FleetStatus`.
- `crates/vowp-store/src/schema/global.sql` — `surfaces`, `leases`, `surface_tokens`.
- `crates/vowp-store/src/repo_surfaces.rs` — `take_lease`, `heartbeat_lease`, `release_lease`,
  `surface_kind_to_text`.
- `crates/vowp-store/src/migrate.rs` — the v1 → v7 migrations and `migrate_v06` pattern.
- `crates/vowp-core/src/surface/lease.rs` — `LeaseGuard`, `check_lease`, `check_lease_with`,
  `revocation_ack_for`, `sweep_leases`, `HEARTBEAT_INTERVAL`, `STALE_AFTER`.
- `crates/vowp-core/src/surface/takeover.rs` — `takeover_inner`, `TAKEOVER_DRAIN_SECS`.
- `crates/vowp-core/src/surface/mirror.rs` — `mirror_from_host`, `MirrorCursor`.
- `crates/vowp-core/src/surface/orchestrator.rs` — `endpoint`, `surface`, `keep_alive`,
  `KEEPER_PID_FILE`.
- `crates/vowp-core/src/surface/opencode_host.rs` — `endpoint`, `HttpClient`, `start_event_listener`.
- `crates/vowp-core/src/agent/gateway.rs` — `active_surface`, `verify_surface_writer`,
  `handle_surface_attach`, `handle_surface_tool_call`.
- `crates/vowp-core/src/agent/dispatch.rs` — `issue_run_lease`, `release_run_lease`, `sweep_expired`,
  `resume_assignment`, `disconnect_node`, `lose`, `RUN_LEASE_TTL`.
- `crates/vowp-core/src/agent/result.rs` — result and lease release.
- `crates/vowp-core/src/fleet/mod.rs` — `session_cards`, `node_tick`.
- `crates/vowp-cli/src/cmd/lease.rs` — `session lease`, `session takeover`.
- `crates/vowp-cli/src/cmd/fleet.rs` — `fleet status`.
- `crates/vowp-cli/src/mcp/backend.rs` — `lease_gate`, `lease_gate_optional`.
- `crates/vowp-agent/src/bridge.rs` — the lease-aware local opencode bridge and `note_lease_lost`.
- `crates/vowp-agent/src/orchestrator.rs` — the node-side endpoint document.
- `docs/design/v0.5/orchestrator-endpoint.md` — the current endpoint contract.
- `docs/design/v0.4/local-orchestrator.md` §4.4 — leases, run fences and disconnect behavior.
- `docs/concepts/home-and-surfaces.md` — current operator-facing lease rules.
