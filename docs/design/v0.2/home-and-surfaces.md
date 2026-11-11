# v0.2 — home, surfaces and the fleet: the implementable spec

Status: current for the commands shown; **normative for v0.2**. It covers plan §3–§10: the global
+ per-project data model, the one node supervisor, the snapshot read model, capacity, sessions and
surfaces (including remote execution, leases, the apps, MCP and the chat fallback), the control
machine, secrets, lifecycle/backup and AI-first setup. It also fixes the types, signatures, CLI
surface, interface hooks, named tests and open assumptions the implementation follows.

Sources: the v0.2 owner decisions for this release, the v0.1 design under `docs/design/`
(`schema.sql`, `types.md`, `sessions.md`, `loops.md`, `cli.md`, `crates/*.md`) and the v0.1 code
as it stands in the public tree. Where this spec tightens a v0.2 decision, the tightening is stated
here and is binding.
Three technical questions (capacity measurement, remote-exec transport and app hosting) shaped the
design; each place that depends on their answer is listed in §16 "Open assumptions" and is written
so the answer can fill in an implementation detail, not change an interface.

This document uses generic hosts (`vowp@home.example`), contains no owner infrastructure, no build
history and no secret values.

---

## 1. Scope, vocabulary and invariants

### 1.1 In scope

- One **home** (one VPS in v0.2): one global DB, one DB per project, one node supervisor.
- **Sessions** with one append-only log on the home and one **surface** at a time (leases).
- **Surfaces**: the opencode desktop/terminal apps, the Claude apps where a login exists, the
  `vowp mcp` tools those apps call, and `vowp chat` as the guaranteed fallback.
- **Remote execution** from a laptop surface, with event ingest and SSH-drop recovery.
- **Capacity**: measured, admitted, braked, advised; the owner decides.
- **Control machines**: `vowp control init`, snapshot cache, managed SSH tunnels.
- **Secrets** in basic mode and vault-user mode, exact-value redaction, one-time paste page.
- **Lifecycle**: classes, `vowp gc`, closed-session compaction, backup/restore.
- **AI-first setup**: `vowp setup plan|home|control|verify` and the README prompt contract.

### 1.2 Out of scope (v0.2)

- More than one home, replication, leader election, multi-VPS scheduling.
- Any **public** listening HTTP/SSE API, web window or phone browser board; those notes moved to
  [`../v0.3/`](../v0.3/README.md). Loopback-only listeners are not public surfaces: the
  `opencode serve` app server joined through an SSH tunnel (§7.5) and the one-time `127.0.0.1`
  secret paste page (§9.2) are in v0.2. The ACP adapter and cost dashboard stay in `v0.2/` as
  deferred design notes (not v0.2 delivery).
- Multi-user accounts, quotas, roles or audit.
- Windows support, a native desktop app, or a VOWP-specific chat app.
- A second session or job state machine. v0.1's log, capsule, switch and request queue are reused.

### 1.3 Vocabulary

The v0.2 vocabulary is binding in code, CLI help and docs:

| Term | Meaning |
|---|---|
| **home** | The machine that owns the global DB, the project DBs, the session logs and the supervisor. One per installation in v0.2. |
| **control machine** | A computer or phone that talks to the home. It holds no truth; it may cache a snapshot. |
| **project** | One git repository registered on the home. |
| **session** | One conversation with one id and one append-only log on the home. |
| **surface** | Where a person is talking to a session now: a `vowp chat`, an app window, an MCP server. |
| **executor** | The harness + model + provider + node that runs the next turn (`harness:model@effort#variant`). |
| **lease** | The right of one surface to send input to one session. |
| **window** | Only a tmux window. Never a surface. |
| **node** | A machine name in the `nodes` registry. In v0.2 the home is the only node that writes; a laptop can be an executor node. |
| **tick** | One node-supervisor pass: node steps, then each active project's L1–L9 steps, then the snapshot. |

### 1.4 Invariants

1. **One writer per session log.** `sessions/<id>/log.jsonl` is appended only on the home, by the
   session service or by `vowp session ingest`. Seq has no gaps or duplicates.
2. **The home is authoritative.** A control machine, an app and an MCP server never write a DB file.
   Every write is a `vowp` command on the home (directly or over SSH).
3. **One active surface per session.** A lease row exists at most once per session. A surface
   without the current token can read (follow mode) and cannot write.
4. **No cross-database foreign keys.** SQLite cannot enforce them across attached files; every id
   that names a row in the other database is plain `TEXT` and is validated by `vowp-store` and by
   the reconcile pass. §3.6 lists every such id.
5. **No cross-file transaction is assumed.** Writes are ordered (global first, then project), are
   idempotent by id, and a startup reconcile repairs a half-done pair.
6. **Never kill running work for capacity.** Admission and brakes stop new runs; they do not signal
   an existing run. The only interrupts are the existing switch/takeover and `vowp down` paths.
7. **Secrets are referred to by name.** No command takes a secret value as an argument, and no log,
   event, capsule, report or snapshot contains an injected value.
8. **v0.1 behaviour stays.** Commands, exit codes, JSON shapes and the request queue of v0.1 keep
   working, except the documented multi-project corrections in §3.

---

## 2. Architecture

### 2.1 Processes

```
control machine                         home (the authority)
  vowp control init                       vowp.service (one node supervisor)
  vowp fleet status  ── ssh ────────────►  ├─ global DB  ~/.vowp/vowp.db
  vowp tunnel open   ── ssh -L ─────────►  ├─ project DBs ~/.vowp/projects/<p>/project.db
  vowp chat (fallback)                     ├─ sessions/<id>/log.jsonl
  opencode desktop ── ssh -L ───────────►  ├─ capacity sampler + admission
  Claude apps (Remote Control)             ├─ snapshot.json every tick
                                           ├─ tmux: vowp-home + vowp-<project>
                                           └─ workers/verifiers/planner/keeper → stock harnesses
```

- The home runs **one** supervisor for every active project. v0.1's node-wide
  `~/.vowp/supervisor.lock` stays; the "one project per supervisor" restriction goes.
- The home writes `snapshot.json` every tick (tmp + fsync + rename). No process opens SQLite over
  the network.
- A control machine runs no supervisor, no loops, no graph and no worktrees.
- Apps are hosted on the home where the product allows it (opencode server, Claude Remote Control);
  the desktop/phone joins over an SSH tunnel. VOWP does not open a public port.

### 2.2 On-disk layout of a home

```
~/.vowp/
  vowp.db                         global DB (schema version 2)
  projects/<name>/project.db      one DB per project (schema version 1)
  projects/<name>/.lock           project DB writer lock (best effort; WAL is the real guard)
  sessions/<id>/log.jsonl         authoritative session content (unchanged from v0.1)
  sessions/<id>/{blobs,capsules,adapters.json}
  snapshot.json                   node snapshot (§5)
  board.json                      v0.1 compatibility copy: written only when exactly one non-reserved project is active
  orchestrators/<name>/           project-local app config, lease token, host state (§7.6)
  secrets/                        secret files, mode 0700/0600 (basic mode; §9)
  runs/  logs/  backups/  tmp/    unchanged
  archive/projects/<name>-<ts>/   `vowp project rm` archives the project directory here (§4.6)
  config.toml                     node-wide config (unchanged)
  supervisor.lock  heartbeat  stop
  migration.json                  present only while a v0.1 → v0.2 migration is in flight
```

`~/.vowp-control/` on a control machine:

```
~/.vowp-control/
  control.json                    role, home name, ssh target, remote home (0600)
  nodes.json                      node registry mirror
  snapshot.json                   last snapshot from the home
  snapshot.meta.json              {"fetched_at": "...", "source": "remote|local|cache"}
  spool/<session>/<run>/events.jsonl   unacked remote events (§7.2)
  spool/<session>/<run>/acked.json     acked event ids
  surfaces.json                   local surface state
  tunnels.json                    managed forwards, pids (0600)
```

### 2.3 Threading and I/O

- `vowp-store`, `vowp-core` and every command stay blocking; plain threads and `std::sync::mpsc`.
- No new async runtime. `vowp mcp`, the secret paste page and the tunnel supervisor use
  `std::net::TcpListener` and `std::process`.
- One `Store` is `Send` but not `Sync`; each thread opens its own. The node supervisor opens a
  global handle and one handle per active project; it never attaches two project DBs to one
  connection.
- The capacity sampler is one thread that samples every 5 s and writes `host_samples`. Per-run
  scopes are sampled by the thread that owns the run (the runner or the gate command).

---

## 3. Data model: global DB + one DB per project

### 3.1 Why the split, and the correction to plan §3 rule 1

The split gives one directory per project (archive or delete a project = move one directory), one
backup unit per project, and a corrupted project DB that cannot stop the fleet.

**The draft's §3 rule 1 is false as written.** It says table names are unique across the two schemas so
unqualified v0.1 queries keep resolving and working unchanged. In v0.1 both the node and the
project have an `inbox` and a `ledger`. In v0.2:

- an unqualified `SELECT … FROM inbox` on a connection whose `main` is the global DB and whose
  attached schema is a project resolves to the **project** inbox (the only `inbox`), so a v0.1
  query that meant node-level inbox rows silently reads project rows;
- the same applies to `ledger`;
- if both schemas kept the names, SQLite would not error: an unqualified name resolves `main`
  first, then the attachments, so a project query would silently read the global table (or a global
  query would read a project table depending on which file is `main`). Silent shadowing is worse
  than an error; the rename removes the ambiguity entirely.

**Chosen fix:** rename the node-level tables to **`node_inbox`**
and **`node_ledger`**. The project tables keep the names `inbox` and `ledger`. There is therefore
no table-name collision at all. This spec does **not** claim that v0.1 queries work unchanged:
every SQL statement in `vowp-store` is assigned to exactly one database. The **public `Store` API
stays source-compatible**, so the rest of the workspace still builds and its tests still pass with
only two changes outside `vowp-store`: the `Deps::open_store` body in
`crates/vowp-core/src/deps.rs` and the reserved `_node` literal in
`crates/vowp-protocol/src/ids.rs` (both in §14.1). No other crate changes.

The complete rule set that replaces the draft's §3 rules 1–3:

1. A `Store` handle is bound to **the global DB plus exactly one project DB**, attached under the
   fixed schema name `project` (`ATTACH DATABASE ? AS project`). `main` is always the global DB.
2. Table names are disjoint: project-only tables are `jobs`, `edges`, `runs`, `gate_results`,
   `workers`, `loops`, `schedules`, `inbox`, `ledger`, `releases`; everything else is global.
   Unqualified SQL therefore resolves deterministically to one schema. The store still writes
   explicit `project.` or `main.` prefixes for every statement that touches a project table, so a
   reader can see which file a statement changes.
3. Never attach two project DBs to one connection. Fleet reads open each project DB in turn.
4. No cross-file foreign keys. The ids in §3.6 are plain text with an index and are validated on
   write and by reconcile.
5. Writes that span the two files are ordered **global first, then project**, are idempotent by id,
   and are repaired by `Store::reconcile_cross_db` (§3.7) at startup. The one deliberate exception
   is run end: `finish_run` (project) is written before the terminal `turn.ended`/`cost` events
   (global), because the project row is the source of truth for the outcome; §3.7 states the repair.
   A crash-injection test proves both orders.

### 3.2 Global schema (`docs/design/schema/global.sql`, `PRAGMA user_version = 2`)

`docs/design/schema.sql` stays in the tree as the frozen v0.1 schema; `crates/vowp-protocol/tests/`
includes it and leaves that test unchanged. The v0.2 schema adds `schema/global.sql` and
`schema/project.sql` next to it and leaves the old file byte-identical. The store's
`schema_matches_enums` test moves to the new files.

This is the complete DDL. Comments name the writer and the retention class. `STRICT` and the
`CHECK` style follow v0.1; JSON columns are `TEXT` with `CHECK(json_valid(..))`.

```sql
PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------- meta
CREATE TABLE kv (                       -- small key/value state; node-wide
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

-- ---------------------------------------------------------------- nodes, projects
CREATE TABLE nodes (                    -- home and known executor nodes
  name         TEXT PRIMARY KEY,
  ssh_target   TEXT,                    -- 'user@host' or an ssh config alias; NULL for local
  is_local     INTEGER NOT NULL DEFAULT 0 CHECK (is_local IN (0,1)),
  capabilities TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(capabilities)),
  last_seen_at TEXT,
  created_at   TEXT NOT NULL,
  CHECK ((is_local = 1 AND ssh_target IS NULL) OR is_local = 0)
) STRICT;
CREATE UNIQUE INDEX nodes_one_local ON nodes(is_local) WHERE is_local = 1;

CREATE TABLE projects (                 -- the registry (plan §3 rule 5)
  name           TEXT PRIMARY KEY,
  path           TEXT NOT NULL,         -- absolute path of the main checkout on the home
  remote_url     TEXT,
  default_branch TEXT NOT NULL DEFAULT 'main',
  config_json    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_json)),
  state          TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','paused','archived')),
  weight         INTEGER NOT NULL DEFAULT 1 CHECK (weight BETWEEN 1 AND 100),
  paused_reason  TEXT NOT NULL DEFAULT '',   -- 'owner' | 'broken: <error>' | ''
  archived_at    TEXT,
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL
) STRICT;
CREATE INDEX projects_state ON projects(state, name);

-- ---------------------------------------------------------------- sessions (global; project-less sessions allowed)
CREATE TABLE sessions (                 -- INDEX of sessions/<id>/log.jsonl (rebuildable)
  id                 TEXT PRIMARY KEY,
  project            TEXT,              -- plain text; validated against projects(name) on write
  job_id             TEXT,              -- plain text; the job lives in that project's DB
  role               TEXT CHECK (role IS NULL OR role IN
                       ('planner','keeper','worker','verifier','user','orchestrator')),
  title              TEXT NOT NULL DEFAULT '',
  parent_session_id  TEXT,              -- lineage across export/import (may name another home)
  node               TEXT NOT NULL,
  cwd                TEXT NOT NULL,
  state              TEXT NOT NULL DEFAULT 'idle' CHECK (state IN
                       ('idle','running','blocked','switching','closed')),
  executor           TEXT,              -- current Executor string
  last_seq           INTEGER NOT NULL DEFAULT 0,
  last_capsule_id    TEXT,
  log_bytes          INTEGER NOT NULL DEFAULT 0,
  compacted_at       TEXT,              -- set when gc replaced the log with capsule + summary
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL
) STRICT;
CREATE INDEX sessions_job            ON sessions(job_id);
CREATE INDEX sessions_project_updated ON sessions(project, updated_at DESC);
CREATE INDEX sessions_role           ON sessions(role, updated_at DESC);

CREATE TABLE session_events (           -- index of log.jsonl lines (rebuildable)
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL CHECK (seq >= 1),
  id          TEXT NOT NULL,            -- event ULID
  ts          TEXT NOT NULL,
  type        TEXT NOT NULL CHECK (type IN
                ('session.created','session.executor','turn.started','turn.ended','user.message',
                 'assistant.message','assistant.reasoning','tool.call','tool.result','file.change','plan',
                 'limit','cost','switch','capsule','error','note')),
  actor       TEXT NOT NULL CHECK (actor IN ('user','assistant','system','tool')),
  harness     TEXT NOT NULL,
  model       TEXT,
  byte_offset INTEGER NOT NULL,
  byte_len    INTEGER NOT NULL,
  summary     TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (session_id, seq)
) STRICT;
CREATE UNIQUE INDEX session_events_id ON session_events(id);
CREATE INDEX session_events_type ON session_events(session_id, type, seq);

CREATE TABLE session_natives (          -- harness-side ids per session
  session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  harness           TEXT NOT NULL,
  native_session_id TEXT NOT NULL,
  node              TEXT NOT NULL,
  first_seq         INTEGER NOT NULL,
  created_at        TEXT NOT NULL,
  PRIMARY KEY (session_id, harness, native_session_id)
) STRICT;

CREATE TABLE native_events (            -- idempotency map for app transcript mirrors (§7.5)
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  harness    TEXT NOT NULL,
  native_id  TEXT NOT NULL,            -- app message/part id
  event_id   TEXT NOT NULL,            -- the VOWP event written for it
  at         TEXT NOT NULL,
  PRIMARY KEY (session_id, harness, native_id)
) STRICT;

CREATE TABLE capsules (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  at_seq       INTEGER NOT NULL,
  version      INTEGER NOT NULL DEFAULT 1,
  trigger      TEXT NOT NULL CHECK (trigger IN
                 ('switch','manual','turn_end','limit','context','export','job_end')),
  author       TEXT,
  path         TEXT NOT NULL,
  body_json    TEXT NOT NULL CHECK (json_valid(body_json)),
  created_at   TEXT NOT NULL
) STRICT;
CREATE INDEX capsules_session ON capsules(session_id, at_seq DESC);

CREATE TABLE switches (
  id            TEXT PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  from_executor TEXT,
  to_executor   TEXT NOT NULL,
  from_node     TEXT,
  to_node       TEXT NOT NULL,
  capsule_id    TEXT REFERENCES capsules(id) ON DELETE SET NULL,
  reason        TEXT NOT NULL CHECK (reason IN
                  ('user','limit','crash','context','failover','probe_back','takeover')),
  state         TEXT NOT NULL DEFAULT 'started' CHECK (state IN ('started','completed','aborted')),
  error         TEXT,
  started_at    TEXT NOT NULL,
  ended_at      TEXT,
  at_seq        INTEGER NOT NULL
) STRICT;
CREATE INDEX switches_session ON switches(session_id, started_at);

CREATE TABLE limits (                   -- per provider, node-wide (all projects share providers)
  provider          TEXT PRIMARY KEY,
  harness           TEXT,
  kind              TEXT NOT NULL CHECK (kind IN ('quota','credits','rate','context','auth','unavailable')),
  resets_at         TEXT,
  detail            TEXT NOT NULL DEFAULT '',
  since             TEXT NOT NULL,
  last_probe_at     TEXT,
  probe_fail_streak INTEGER NOT NULL DEFAULT 0,
  concurrency_cap   INTEGER,
  cap_until         TEXT,
  updated_at        TEXT NOT NULL
) STRICT;

-- ---------------------------------------------------------------- node-level inbox / ledger (renamed)
CREATE TABLE node_inbox (               -- node-wide questions/notices; project inboxes live per project
  id           TEXT PRIMARY KEY,
  project      TEXT,                    -- NULL = fleet-wide; a name routes it to that project's L7
  job_id       TEXT,                    -- plain text; a job-bearing item MUST also name its project
  kind         TEXT NOT NULL CHECK (kind IN ('question','approval','decision','notice')),
  title        TEXT NOT NULL,
  body         TEXT NOT NULL DEFAULT '',
  options_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options_json)),
  state        TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','answered','dismissed')),
  answer       TEXT,
  answered_by  TEXT,
  created_at   TEXT NOT NULL,
  answered_at  TEXT,
  CHECK ((state = 'open') = (answered_at IS NULL))
) STRICT;
CREATE INDEX node_inbox_open ON node_inbox(state, created_at);

CREATE TABLE node_ledger (              -- append-only node-wide rows; project ledgers live per project
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN
                ('dispatch','gate','merge','cost','switch','limit','promote','rollback','inbox','schedule','state','note')),
  project     TEXT,                    -- always NULL here; plain text
  job_id      TEXT,
  run_id      TEXT,
  session_id  TEXT,
  role        TEXT CHECK (role IS NULL OR role IN
                ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),
  provider    TEXT,
  harness     TEXT,
  model       TEXT,
  usd         REAL,
  tokens_in   INTEGER NOT NULL DEFAULT 0,
  tokens_out  INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning  INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  cost_source TEXT CHECK (cost_source IS NULL OR cost_source IN ('receipt','price-table','unknown')),
  summary     TEXT NOT NULL,
  detail_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(detail_json))
) STRICT;
CREATE INDEX node_ledger_ts   ON node_ledger(ts);
CREATE INDEX node_ledger_kind ON node_ledger(kind, ts);
CREATE UNIQUE INDEX node_ledger_cost_once ON node_ledger(kind, run_id)
  WHERE kind = 'cost' AND run_id IS NOT NULL;   -- a replay cannot double-charge a run
CREATE TRIGGER node_ledger_no_update BEFORE UPDATE ON node_ledger
  BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;
CREATE TRIGGER node_ledger_no_delete BEFORE DELETE ON node_ledger
  BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

-- ---------------------------------------------------------------- surfaces and leases (§7.3)
CREATE TABLE surfaces (
  id            TEXT PRIMARY KEY,        -- ULID
  kind          TEXT NOT NULL CHECK (kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
  node          TEXT NOT NULL,
  holder        TEXT NOT NULL,           -- human-visible: 'laptop', 'phone', 'mac opencode'
  app           TEXT,                    -- 'opencode-desktop', 'claude-code', ...
  cwd           TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
  created_at    TEXT NOT NULL,
  last_seen_at  TEXT NOT NULL
) STRICT;

CREATE TABLE leases (
  session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
  surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
  node         TEXT NOT NULL,
  holder       TEXT NOT NULL,
  token        TEXT NOT NULL,            -- 52 chars: two ULIDs; never logged
  acquired_at  TEXT NOT NULL,
  heartbeat_at TEXT NOT NULL,
  CHECK (length(token) = 52)
) STRICT;
CREATE INDEX leases_heartbeat ON leases(heartbeat_at);

-- ---------------------------------------------------------------- catalog (§10)
CREATE TABLE catalog (
  id          TEXT PRIMARY KEY,          -- ULID
  path        TEXT NOT NULL,
  class       TEXT NOT NULL CHECK (class IN
                ('production','truth','open_work','secrets','closed_session','run_output','worktree','sample','cache')),
  project     TEXT,                      -- plain text
  environment TEXT NOT NULL DEFAULT 'none' CHECK (environment IN ('work','prod','none')),
  job_id      TEXT,
  session_id  TEXT,
  bytes       INTEGER,
  expires_at  TEXT,
  note        TEXT NOT NULL DEFAULT '',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  deleted_at  TEXT
) STRICT;
CREATE UNIQUE INDEX catalog_path ON catalog(path);
CREATE INDEX catalog_class   ON catalog(class, expires_at);
CREATE INDEX catalog_project ON catalog(project);

-- ---------------------------------------------------------------- capacity (§6)
CREATE TABLE host_samples (
  id                      INTEGER PRIMARY KEY AUTOINCREMENT,
  at                      TEXT NOT NULL,
  interval_secs           INTEGER NOT NULL,
  mem_total_bytes         INTEGER NOT NULL,
  mem_available_bytes     INTEGER NOT NULL,
  swap_used_bytes         INTEGER NOT NULL,
  load1                   REAL NOT NULL,
  load5                   REAL NOT NULL,
  load15                  REAL NOT NULL,
  cpu_count               INTEGER NOT NULL,
  cpu_quota_cores         REAL,
  disk_free_home_bytes    INTEGER NOT NULL,
  disk_free_worktrees_bytes INTEGER,
  pressure_cpu_avg10      REAL,
  pressure_mem_avg10      REAL,
  pressure_io_avg10       REAL,
  source                  TEXT NOT NULL CHECK (source IN ('proc','fallback'))
) STRICT;
CREATE INDEX host_samples_at ON host_samples(at);

CREATE TABLE run_usage (                -- one row per finished run (§6.1)
  run_id          TEXT PRIMARY KEY,
  project         TEXT NOT NULL,
  job_id          TEXT,
  job_kind        TEXT,
  role            TEXT NOT NULL CHECK (role IN
                    ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),
  harness         TEXT,
  model           TEXT,
  provider        TEXT,
  node            TEXT NOT NULL,
  started_at      TEXT NOT NULL,
  ended_at        TEXT NOT NULL,
  duration_ms     INTEGER NOT NULL,
  peak_rss_bytes  INTEGER NOT NULL,       -- cgroup memory.peak, or the sampled peak
  anon_peak_bytes INTEGER,                -- sampled memory.stat anon at peak; NULL if not sampled
  file_peak_bytes INTEGER,                -- sampled file pages at peak; NULL if not sampled
  kernel_peak_bytes INTEGER,              -- sampled kernel memory at peak; NULL if not sampled
  cpu_usec        INTEGER NOT NULL DEFAULT 0,  -- cpu.stat usage_usec, or accumulated /proc delta
  mean_cpu_cores  REAL NOT NULL,
  max_cpu_cores   REAL NOT NULL,
  build_mode      TEXT NOT NULL DEFAULT 'unknown'
                    CHECK (build_mode IN ('cold','warm','incremental','unknown')),
  source          TEXT NOT NULL CHECK (source IN ('cgroup','proc_tree','default')),
  oom_killed      INTEGER NOT NULL DEFAULT 0 CHECK (oom_killed IN (0,1)),
  exit_code       INTEGER
) STRICT;
CREATE INDEX run_usage_group ON run_usage(project, job_kind, harness, model, build_mode, started_at);

CREATE TABLE usage_estimates (          -- pooled estimates; '' in a key column means "any"
  project      TEXT NOT NULL,
  job_kind     TEXT NOT NULL,
  harness      TEXT NOT NULL DEFAULT '',
  model        TEXT NOT NULL DEFAULT '',
  build_mode   TEXT NOT NULL DEFAULT 'unknown'
                 CHECK (build_mode IN ('cold','warm','incremental','unknown')),
  samples      INTEGER NOT NULL,
  ram_p50_bytes INTEGER NOT NULL,
  ram_p90_bytes INTEGER NOT NULL,
  cpu_p50_cores REAL NOT NULL,
  cpu_p90_cores REAL NOT NULL,
  duration_p50_ms INTEGER NOT NULL,
  duration_p90_ms INTEGER NOT NULL,
  source       TEXT NOT NULL CHECK (source IN ('measured','pooled','default')),
  updated_at   TEXT NOT NULL,
  PRIMARY KEY (project, job_kind, harness, model, build_mode)
) STRICT;

CREATE TABLE capacity_targets (         -- history; newest row wins
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  at              TEXT NOT NULL,
  mode            TEXT NOT NULL CHECK (mode IN ('auto','fixed')),
  target_workers  INTEGER NOT NULL CHECK (target_workers >= 0),
  advised_workers INTEGER,
  is_override     INTEGER NOT NULL DEFAULT 0 CHECK (is_override IN (0,1)),
  set_by          TEXT NOT NULL,        -- 'owner' | 'supervisor' | 'setup'
  reason          TEXT NOT NULL DEFAULT '',
  advice_id       TEXT
) STRICT;
CREATE INDEX capacity_targets_at ON capacity_targets(at);

CREATE TABLE capacity_advice (
  id              TEXT PRIMARY KEY,     -- ULID
  created_at      TEXT NOT NULL,
  current_target  INTEGER NOT NULL,
  proposed_target INTEGER NOT NULL,
  mode            TEXT NOT NULL CHECK (mode IN ('auto','fixed')),
  reasons_json    TEXT NOT NULL CHECK (json_valid(reasons_json)),
  expected_effect TEXT NOT NULL,
  inbox_id        TEXT,                 -- node_inbox item, kind 'decision'
  state           TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','applied','dismissed')),
  applied_at      TEXT,
  applied_by      TEXT
) STRICT;
CREATE INDEX capacity_advice_state ON capacity_advice(state, created_at);

-- ---------------------------------------------------------------- secrets metadata (§9; values are files)
CREATE TABLE secret_index (
  name       TEXT PRIMARY KEY,          -- ^[A-Z][A-Z0-9_]{0,63}$
  last4      TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

PRAGMA user_version = 2;
```

### 3.3 Project schema (`docs/design/schema/project.sql`, `PRAGMA user_version = 1`)

Every project DB has the same schema. The ten project tables keep their v0.1 names and columns,
with these changes only: `project` is plain text validated by the store; `runs.session_id` is plain
text; there are no references to global tables. Self-references inside one project DB are kept.

```sql
PRAGMA foreign_keys = ON;

CREATE TABLE jobs (
  id               TEXT PRIMARY KEY,
  project          TEXT NOT NULL,       -- plain text; MUST equal the bound project
  title            TEXT NOT NULL,
  kind             TEXT NOT NULL CHECK (kind IN
                     ('plan','spec','build','test','verify','fix','merge','docs','research','promote','release','decision')),
  role             TEXT NOT NULL CHECK (role IN ('planner','keeper','worker','verifier')),
  profile          TEXT NOT NULL,
  state            TEXT NOT NULL DEFAULT 'pending' CHECK (state IN
                     ('pending','ready','running','stalled','gating','reviewing','fixing','merging',
                      'done','failed','parked','replanned','cancelled')),
  priority         INTEGER NOT NULL DEFAULT 50,
  owns_json        TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(owns_json)),
  reads_json       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(reads_json)),
  brief_path       TEXT NOT NULL,
  report_path      TEXT NOT NULL,
  gates_json       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(gates_json)),
  loop_json        TEXT NOT NULL DEFAULT '{"until":"gates","max_attempts":3,"on_exhausted":"escalate"}'
                     CHECK (json_valid(loop_json)),
  branch           TEXT,
  worktree         TEXT,
  attempts         INTEGER NOT NULL DEFAULT 0,
  continues        INTEGER NOT NULL DEFAULT 0,
  hang_restarts    INTEGER NOT NULL DEFAULT 0,
  parked_reason    TEXT,
  note             TEXT NOT NULL DEFAULT '',
  created_by       TEXT NOT NULL DEFAULT 'human',
  last_progress_at TEXT,
  state_changed_at TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL
) STRICT;
CREATE INDEX jobs_project_state ON jobs(project, state);
CREATE INDEX jobs_dispatch      ON jobs(state, priority DESC, created_at);

CREATE TABLE edges (
  src        TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  dst        TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('needs','verifies','fixes','escalates','blocks')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (src, dst, kind),
  CHECK (src <> dst)
) STRICT;
CREATE INDEX edges_dst ON edges(dst, kind);

CREATE TABLE runs (
  id                TEXT PRIMARY KEY,
  job_id            TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  session_id        TEXT,               -- plain text; the session is in the global DB
  role              TEXT NOT NULL CHECK (role IN ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),
  purpose           TEXT NOT NULL DEFAULT 'start' CHECK (purpose IN
                      ('start','continue','fix','verify','plan','keep','capsule','probe','merge','gate')),
  profile           TEXT,
  rung              INTEGER NOT NULL DEFAULT 0,
  harness           TEXT NOT NULL,
  model             TEXT,
  effort            TEXT,
  provider          TEXT,
  node              TEXT NOT NULL,
  native_session_id TEXT,
  pid               INTEGER,
  window            TEXT,
  cwd               TEXT,
  started_at        TEXT NOT NULL,
  ended_at          TEXT,
  exit_code         INTEGER,
  outcome           TEXT CHECK (outcome IS NULL OR outcome IN
                      ('ok','limit','credits','auth','context','rate','crash','stalled','killed','error')),
  limit_kind        TEXT CHECK (limit_kind IS NULL OR limit_kind IN ('quota','credits','rate','context','auth')),
  cost_usd          REAL,
  cost_source       TEXT NOT NULL DEFAULT 'unknown' CHECK (cost_source IN ('receipt','price-table','unknown')),
  tokens_in         INTEGER NOT NULL DEFAULT 0,
  tokens_out        INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning  INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  detail            TEXT NOT NULL DEFAULT '',
  CHECK ((ended_at IS NULL) = (outcome IS NULL))
) STRICT;
CREATE INDEX runs_job     ON runs(job_id, started_at);
CREATE INDEX runs_open    ON runs(ended_at) WHERE ended_at IS NULL;
CREATE INDEX runs_session ON runs(session_id);

CREATE TABLE gate_results (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id       TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  run_id       TEXT REFERENCES runs(id) ON DELETE SET NULL,
  attempt      INTEGER NOT NULL DEFAULT 0,
  gate_index   INTEGER NOT NULL,
  gate_kind    TEXT NOT NULL CHECK (gate_kind IN
                 ('report','cmd','owns_respected','pushed','review','script','deploy','secret_scan')),
  gate_json    TEXT NOT NULL CHECK (json_valid(gate_json)),
  passed       INTEGER NOT NULL CHECK (passed IN (0,1)),
  evidence_json TEXT NOT NULL CHECK (json_valid(evidence_json)),
  output_blob  TEXT,
  duration_ms  INTEGER NOT NULL DEFAULT 0,
  ts           TEXT NOT NULL
) STRICT;
CREATE INDEX gate_results_job ON gate_results(job_id, attempt, gate_index);

CREATE TABLE workers (                  -- project-preferred slots; node capacity is authoritative (§6)
  node       TEXT NOT NULL,
  slot       INTEGER NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'worker' CHECK (kind IN ('worker','verifier')),
  state      TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle','busy','draining')),
  run_id     TEXT REFERENCES runs(id) ON DELETE SET NULL,
  job_id     TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  window     TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (node, kind, slot),
  CHECK ((state = 'idle') = (run_id IS NULL))
) STRICT;

CREATE TABLE loops (
  name          TEXT PRIMARY KEY,
  kind          TEXT NOT NULL CHECK (kind IN ('L1','L2','L3','L4','L5','L6','L7','L8','L9')),
  project       TEXT,                   -- plain text; NULL means "the bound project"
  interval_secs INTEGER NOT NULL CHECK (interval_secs >= 0),
  state         TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','paused')),
  bounds_json   TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(bounds_json)),
  last_run_at   TEXT,
  next_run_at   TEXT,
  last_status   TEXT NOT NULL DEFAULT '',
  fail_streak   INTEGER NOT NULL DEFAULT 0,
  updated_at    TEXT NOT NULL
) STRICT;
CREATE INDEX loops_project ON loops(project, kind);

CREATE TABLE schedules (
  id            TEXT PRIMARY KEY,
  project       TEXT,                   -- plain text; NULL means "the bound project"
  name          TEXT NOT NULL UNIQUE,
  spec_kind     TEXT NOT NULL CHECK (spec_kind IN ('cron','interval')),
  spec          TEXT NOT NULL,
  tz            TEXT NOT NULL DEFAULT 'UTC',
  action_kind   TEXT NOT NULL CHECK (action_kind IN ('job_template','loop','command')),
  action_json   TEXT NOT NULL CHECK (json_valid(action_json)),
  missed_policy TEXT NOT NULL DEFAULT 'run_once' CHECK (missed_policy IN ('run_once','skip')),
  state         TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','paused')),
  last_run_at   TEXT,
  next_run_at   TEXT,
  last_status   TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL
) STRICT;
CREATE INDEX schedules_due ON schedules(state, next_run_at);

CREATE TABLE inbox (                    -- project questions; node-wide items live in node_inbox
  id           TEXT PRIMARY KEY,
  project      TEXT,                    -- plain text; NULL means "the bound project"
  job_id       TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('question','approval','decision','notice')),
  title        TEXT NOT NULL,
  body         TEXT NOT NULL DEFAULT '',
  options_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options_json)),
  state        TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','answered','dismissed')),
  answer       TEXT,
  answered_by  TEXT,
  created_at   TEXT NOT NULL,
  answered_at  TEXT,
  CHECK ((state = 'open') = (answered_at IS NULL))
) STRICT;
CREATE INDEX inbox_open ON inbox(state, created_at);

CREATE TABLE ledger (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN
                ('dispatch','gate','merge','cost','switch','limit','promote','rollback','inbox','schedule','state','note')),
  project     TEXT,                     -- plain text; NULL means "the bound project"
  job_id      TEXT,
  run_id      TEXT,
  session_id  TEXT,                     -- plain text; the session is in the global DB
  role        TEXT CHECK (role IS NULL OR role IN
                ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),
  provider    TEXT,
  harness     TEXT,
  model       TEXT,
  usd         REAL,
  tokens_in   INTEGER NOT NULL DEFAULT 0,
  tokens_out  INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning  INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  cost_source TEXT CHECK (cost_source IS NULL OR cost_source IN ('receipt','price-table','unknown')),
  summary     TEXT NOT NULL,
  detail_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(detail_json))
) STRICT;
CREATE INDEX ledger_ts   ON ledger(ts);
CREATE INDEX ledger_job  ON ledger(job_id, ts);
CREATE INDEX ledger_kind ON ledger(kind, ts);
CREATE UNIQUE INDEX ledger_cost_once ON ledger(kind, run_id)
  WHERE kind = 'cost' AND run_id IS NOT NULL;   -- a replay cannot double-charge a run
CREATE TRIGGER ledger_no_update BEFORE UPDATE ON ledger
  BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;
CREATE TRIGGER ledger_no_delete BEFORE DELETE ON ledger
  BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TABLE releases (
  id             TEXT PRIMARY KEY,
  project        TEXT NOT NULL,         -- plain text; MUST equal the bound project
  env            TEXT NOT NULL,
  change_ref     TEXT NOT NULL,
  from_commit    TEXT,
  to_commit      TEXT,
  state          TEXT NOT NULL DEFAULT 'planned' CHECK (state IN
                   ('planned','guarded','banked','approved','deploying','probing','live','failed','rolled_back','refused')),
  guards_json    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(guards_json)),
  snapshot_path  TEXT,
  rollback_cmd   TEXT,
  probe_json     TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(probe_json)),
  approved_by    TEXT,
  rolled_back_by TEXT REFERENCES releases(id),
  note           TEXT NOT NULL DEFAULT '',
  created_at     TEXT NOT NULL,
  finished_at    TEXT
) STRICT;
CREATE INDEX releases_project ON releases(project, env, created_at DESC);

PRAGMA user_version = 1;
```

### 3.4 Table ownership and the resolved-name table

| Database | Table | Writer | Read by |
|---|---|---|---|
| global | `kv` | all | all |
| global | `nodes`, `projects` | `project`/`node` commands, setup | node supervisor, fleet |
| global | `sessions`, `session_events`, `session_natives`, `native_events` | session service, reindex, mirror | every surface |
| global | `capsules`, `switches` | session service | surfaces, fleet |
| global | `limits` | L6, session service | admission, board |
| global | `node_inbox` | capacity advisor, node supervisor, `inbox ask` with no project | fleet, board |
| global | `node_ledger` | node-scoped cost/switch/state rows | cost, fleet |
| global | `surfaces`, `leases` | lease guard, takeover | ingest, MCP, chat |
| global | `catalog` | gc, backup, project rm, release | gc, fleet |
| global | `host_samples`, `run_usage`, `usage_estimates` | capacity sampler | admission, advisor |
| global | `capacity_targets`, `capacity_advice` | admission, advisor | admission, advisor, board |
| global | `secret_index` | secret commands | `secret ls` |
| project | `jobs`, `edges` | graph, CLI | supervisor, fleet |
| project | `runs`, `gate_results`, `workers` | runner, gates, supervisor | board, fleet |
| project | `loops`, `schedules` | reconcile, schedule engine | supervisor |
| project | `inbox` | L7, `inbox ask` with a project | fleet, board |
| project | `ledger` | ledger writer | cost, fleet |
| project | `releases` | promote | releases list, gc |

**Resolved-name rule.** A statement that names a table must name exactly one of these. The store
never writes an unqualified `inbox`, `ledger`, `jobs`, `runs` or `sessions` where a reader could
misread which file is meant; it writes `main.`/`project.` prefixes at every cross-file site, and a
review checklist test (`every_sql_statement_names_its_database`) greps the `vowp-store` sources for
the old unqualified names.

### 3.5 Types that do not change

`Project` and `Node` stay exactly as in v0.1. The registry state and weight need a new view without
breaking the many struct literals in tests, so the new protocol types live in
`vowp-protocol/src/snapshot.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState { Active, Paused, Archived }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectEntry {
    pub name: ProjectName,
    pub path: String,
    pub remote_url: Option<String>,
    pub default_branch: String,
    pub state: ProjectState,
    pub weight: u8,
    pub paused_reason: String,
    pub archived_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

`Store::upsert_project(&Project)` keeps its signature and sets `state='active'`, `weight=1` on
insert; on update it preserves `state`, `weight`, `paused_reason` and `archived_at`. The
`project` commands use the new `ProjectEntry` methods.

`SessionRole` gains one additive variant, `Orchestrator` (wire name `orchestrator`). `InboxKind`
does **not** gain a `capacity` variant: adding it would force edits in `vowp-core`'s report renderer
outside the capacity modules. A capacity advice row is a normal `decision` inbox item (title
`Capacity: …`, options `["apply","keep"]`) linked by `capacity_advice.inbox_id`; the category is in
`capacity_advice`, not in the inbox enum. This is a deliberate deviation from the plan's wording
"inbox item of kind `capacity`" and is recorded in §16.

### 3.6 Cross-database ids, removed foreign keys and validation

Every id below is plain `TEXT` with an index where it is queried. The "validated by" column names
the check that runs on write and in `Store::reconcile_cross_db`. Reads tolerate a missing target
(the other DB may be archived); writes do not.

| Id | Lives in | Names | Validated by |
|---|---|---|---|
| `jobs.project` | project | `projects.name` | store: must equal the bound project; `project add` created the DB |
| `loops.project` | project | `projects.name` | store: `NULL` or equal to the bound project |
| `schedules.project` | project | `projects.name` | store: `NULL` or equal to the bound project |
| `releases.project` | project | `projects.name` | store: must equal the bound project |
| `inbox.project` | project | `projects.name` | store: `NULL` or equal to the bound project |
| `ledger.project` | project | `projects.name` | store: `NULL` or equal to the bound project |
| `sessions.project` | global | `projects.name` | session service on create; the store tolerates a missing row on reindex/import and keeps the text after `project rm` |
| `sessions.job_id` | global | `jobs.id` in that project | session service on create; validated by the job row it came from |
| `runs.session_id` | project | `sessions.id` | session service; reconcile closes a run whose session is gone |
| `ledger.session_id`, `ledger.run_id`, `ledger.job_id` | both | the other file | ledger writer; tolerated if the target was archived |
| `catalog.project`, `catalog.job_id`, `catalog.session_id` | global | registry / a project | catalog writer; `gc` treats a missing target as disposable cache only if the class also says so |
| `run_usage.project`, `run_usage.job_id` | global | registry / a project | capacity sampler writes from a live run |
| `run_usage.run_id` | global | `runs.id` in a project | capacity sampler writes at run end; a missing run is kept for cost history |
| `capacity_advice.inbox_id` | global | `node_inbox.id` | advisor; a missing item only means the owner dismissed it |
| `node_inbox.job_id`, `node_inbox.project` | global | a project job / registry | L7 and capacity; a missing job leaves a notice |
| `sessions.node`, `leases.node`, `run_usage.node`, `surfaces.node` | global | `nodes.name` | session/lease/usage writers require a registered executor node; a stale surface lease is swept |
| `runs.node`, `workers.node` | project | `nodes.name` | dispatch and worker-slot writers require a registered node; a missing node is a broken row, reported by doctor |

The removals the correction requires are exactly these v0.1 clauses:

```sql
-- v0.1, removed in v0.2:
project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE   -- jobs
project TEXT REFERENCES projects(name) ON DELETE CASCADE            -- loops, schedules, inbox
project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE   -- releases
job_id  TEXT REFERENCES jobs(id) ON DELETE SET NULL                 -- sessions (global file!)
session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL          -- runs (project file!)
```

They become the plain columns shown in §3.2/§3.3. The store exposes:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    // ... v0.1 variants ...
    #[error("wrong project: handle is bound to {bound:?}, operation names {found:?}")]
    WrongProject { bound: ProjectName, found: ProjectName },
    #[error("no project bound to this store handle")]
    NoProject,
    #[error("schema v0.1 database: run `vowp migrate` before opening it")]
    MigrationRequired,
}
```

Exit mapping: `WrongProject` and `NoProject` are `ErrorKind::InvalidInput` (exit 2) with a hint
naming the bound project or `--project`; `MigrationRequired` is `ErrorKind::Conflict` (exit 4) with
the hint `vowp migrate`. The CLI exit table (§12.4) repeats this.

Only methods whose arguments or filters can name a project return `WrongProject`:
`insert_job(project, …)`, `insert_release`/`update_release` (`Release.project`),
`upsert_loop` (`Loop.project`), `insert_schedule` (`Schedule.project`), `insert_inbox`
(`InboxItem.project`), `append_ledger` (`LedgerEntry.project`), `list_jobs` (`JobFilter.project`),
`list_edges(Option<&ProjectName>)`, `count_jobs_by_state(Option<&ProjectName>)`,
`list_releases(project, …)` and `cost_totals` (`CostQuery.project`). A `None` where the method
allows it means "the bound project".

Methods with no project argument never take another project: `patch_job`, `insert_run`, `list_inbox`,
`ledger_since` and the run/worker/session repositories operate on the bound project. They return
`NoProject` on an unbound handle; a row that belongs elsewhere is `NotFound` (the row is not in this
project DB), `InvalidInput` (a job/run id that cannot belong here) or `Corrupt` (a stored
`jobs.project` that disagrees with the bound project). `list_inbox` and `ledger_since` read the
bound project plus the node rows; they need no project filter. This keeps the v0.1 signatures
source-compatible.

### 3.7 Store binding, ordered writes and reconcile

```rust
pub const GLOBAL_SCHEMA_VERSION: i64 = 2;
pub const PROJECT_SCHEMA_VERSION: i64 = 1;

impl Store {
    /// Opens the global DB only; project-scoped methods return NoProject. The node supervisor and
    /// fleet reads use this handle for global writes and open each project in turn.
    pub fn open_global(paths: &NodePaths) -> Result<Store, StoreError>;
    /// Opens the global DB. If exactly one non-reserved project is registered (ignoring `_node`),
    /// attaches that project's DB too, creating it if the file is absent (the v0.1 fixture path);
    /// with zero or several projects the handle is fleet-only and project methods return
    /// NoProject. `_node` alone does not count as a project for this rule.
    pub fn open(paths: &NodePaths) -> Result<Store, StoreError>;
    /// Opens the global DB and attaches exactly `project`. The project must be registered and its
    /// DB file must exist (created by [`Store::create_project_db`], `project add` or the migration).
    pub fn open_with_project(paths: &NodePaths, project: &ProjectName) -> Result<Store, StoreError>;
    /// Creates `projects/<name>/project.db` with `schema/project.sql` and `user_version=1`.
    /// Idempotent: an existing DB with the right version is left untouched.
    pub fn create_project_db(paths: &NodePaths, project: &ProjectName) -> Result<(), StoreError>;
    /// Read-only variant for fleet snapshots: opens with `PRAGMA query_only=ON`, never migrates,
    /// never writes. (SQLite's `SQLITE_OPEN_READ_ONLY` cannot reliably open a live WAL database
    /// unless its `-shm` exists; `query_only` gives the same guarantee without that failure mode.)
    pub fn open_read_only(paths: &NodePaths, project: Option<&ProjectName>) -> Result<Store, StoreError>;
    /// In-memory global + one lazily attached in-memory project DB (tests).
    pub fn open_memory() -> Result<Store, StoreError>;
    pub fn project(&self) -> Option<&ProjectName>;
    /// Compatibility accessor: always 1 (`PROJECT_SCHEMA_VERSION`) for a bound handle and for an
    /// unbound global handle, matching v0.1's "schema 1". The v0.1 tests
    /// (`vowp-core/tests/deps.rs`, `vowp-cli/tests/maint.rs`) assert 1 and stay untouched.
    pub fn schema_version(&self) -> Result<i64, StoreError>;
    /// The global file's real `PRAGMA user_version` (2). `vowp doctor` and `vowp migrate` use this.
    pub fn global_schema_version(&self) -> Result<i64, StoreError>;
    /// The bound project's real `PRAGMA user_version` (1).
    pub fn project_schema_version(&self) -> Result<i64, StoreError>;
    /// Runs at supervisor startup and after a crash. Repairs half-done cross-file pairs.
    pub fn reconcile_cross_db(&self) -> Result<CrossDbReconcile, StoreError>;
}
```

`schema_version()` is the compatibility shim: it reports **1** for both a bound project and an
unbound global handle. It is deliberately not the global file's value; new code that means the
global file calls `global_schema_version()` (2). This keeps `vowp-core/tests/deps.rs` and
`vowp-cli/tests/maint.rs` valid; `vowp doctor` prints both
(`global 2, project 1`).

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossDbReconcile {
    pub runs_closed: u32,              // open run whose session already has turn.ended
    pub runs_missing_session: u32,     // open run whose session row is gone: closed as error
    pub sessions_repaired: u32,        // state 'running' with no open run: idle + note
    pub terminal_events_repaired: u32, // closed run without turn.ended: note + session idle
    pub ledgers_repaired: u32,         // a finished run without its cost ledger row gets a 'note' repair row
    pub leases_dropped: u32,           // lease for a missing session
}
```

`open_memory` creates the global schema and starts unbound. `upsert_project` is the one registry
writer that also guarantees the project DB exists: for a file-backed store it calls
`create_project_db` (idempotent) for a newly inserted row, and for `open_memory` it attaches a lazy
in-memory project DB. This is what keeps the v0.1 fixtures working: they do `Store::open` +
`upsert_project` and immediately insert jobs (`vowp-store/tests/{jobs,runs}`, `misc.rs` except its
one multi-project FK test, `vowp-core/tests/gates_support.rs`, `vowp-cli/tests/common/mod.rs`).
The registry row and its DB appear together; `vowp project add` does the same call and then opens
the project.

Binding rules (all enforced by `upsert_project` and the constructors; an `open_global` or
`open_read_only` handle never auto-binds):

- an **unbound** handle binds the one **non-reserved** project when that project is inserted and
  the registry contains no other non-reserved project;
- an unbound handle binds `_node` when `_node` is inserted and no non-reserved project exists;
- a handle bound to the reserved **`_node`** project re-binds to the first non-reserved project
  inserted through `upsert_project`. This is the v0.1 `bundle.rs` path: a fresh `Deps` binds
  `_node`, then the test registers `probe-project` and inserts its job. It applies only to
  `_node`; a real project handle never re-attaches;
- inserting a second real project on a handle bound to a real project does not re-attach; a
  project-scoped call for that second project returns `WrongProject`. Tests that need two real
  projects use two stores (or a temporary file), never two project DBs on one connection.

`Store::open` applies the same choice: with exactly one non-reserved project it attaches (and
creates the DB if the file is absent); with zero or several it is a fleet-only handle. `Store`
retains the node root for a file-backed handle (`None` for `open_memory`) so `upsert_project` can
create `projects/<name>/project.db`. The reserved `_node` literal is accepted by
`ProjectName::parse` (and only that literal; see §14.1); `project add`/`init` still refuse it as a
user project.

Routing for the renamed tables (binding):

- `insert_inbox(&InboxItem)` routes by `item.project`: `None` writes `node_inbox`, `Some(p)` writes
  the project `inbox` and returns `WrongProject` if the handle is bound elsewhere. A `job_id` with
  `project=None` is `InvalidInput`: a job item belongs to exactly one project, and letting it float
  would show it in every project's L7.
- `insert_node_inbox(&InboxItem)` is the explicit node-level writer: it always writes `node_inbox`,
  allows `project = Some(p)` for a notice about project `p`, and requires `project = Some(p)` when
  `job_id` is set. §4.2 (broken project) and §6.3 (OOM job) use this, so a node notice can name its
  project without being copied into every project's inbox. On a bound handle it still writes the
  global table; a project name that is not registered is `InvalidInput`.
- `list_inbox` on a bound handle reads the project inbox plus `node_inbox` rows whose `project` is
  `NULL` or equals the bound project; de-duplicates by id (ids are ULIDs, so a collision is
  corruption, not a merge) and sorts `created_at, id`. `inbox_item`, `answer_inbox` and
  `dismiss_inbox` look in `node_inbox` first, then the project inbox. `vowp inbox answer` on a
  fleet-wide item therefore keeps working on a bound handle.
- `append_ledger(project=None)` writes `node_ledger`; `Some(p)` writes the project `ledger`.
  `ledger_since` and `cost_totals` read both and merge; a `Some(p)` query includes the project's
  ledger and any node rows that carry that project text (there are none by construction, but the
  SQL is a union, not an assumption). The append-only triggers exist in both files.
- **The CLI→supervisor request queue is project-scoped.** v0.1 stored every request as
  `kv['req:<ULID>']`; with several projects every project's `step_requests` would drain every
  project's job request. v0.2 keys are `kv['req:<project>:<ULID>']` for a request that names a
  project job/loop/schedule, and `kv['req:node:<ULID>']` for a node-wide request (`Wake`).
  `Store::push_request` on a bound handle writes the project key **unless `kind == "wake"`**, which
  always writes the node key; on an unbound handle every request writes the node key.
  `Store::take_requests` on a bound handle atomically drains that project's keys **and** the node
  keys; on an unbound handle it drains only node keys. The `kind`/`payload` wire form is unchanged,
  so `vowp_core::requests` needs no change. A decoded project request for another project can never
  reach this handle; a test proves it.

Write order and idempotency:

1. **Session framing first (global).** `session.created`, `session.executor` attach, `turn.started`
   and `user.message` are appended and indexed before the project `runs` row is inserted. A crash
   after the append leaves a session with a turn that has no run row; reconcile does not invent a
   run (the next dispatch sees the last turn and resumes).
2. **Run row second (project).** The project row carries the `session_id` and the same `run_id` the
   ledger will use.
3. **Run end (project) then terminal events (global).** `finish_run` is written before the
   `cost`/`limit`/`error`/`turn.ended` events. A crash between them leaves a closed run with no
   `turn.ended`; `reconcile_cross_db` appends a `note` event `turn ended during crash recovery` and
   closes the session state to `idle` (or `blocked` for a limit). It never rewrites an existing
   event.
4. **Ledger last.** Ledger rows are derived and idempotent by `(kind, run_id)` for cost rows; the
   partial unique index in each ledger makes a duplicate insert a `Conflict`, which callers treat
   as success (the existing row wins). A repair row is a new `note`, never an update.
5. **All ids are supplied, never generated at read time**, so a retry can detect an existing row.
   `insert_run` returns `Conflict` on a duplicate id; callers treat that as success.

`reconcile_cross_db` runs before any project L1 tick:

- open run in `runs` and `sessions.state='running'` but the session has a `turn.ended` after the
  run's `started_at` → `finish_run(outcome='stalled', detail='reconciled after crash')`,
  `runs_closed += 1`;
- open run whose `session_id` is absent from global `sessions` → `finish_run(outcome='error')`,
  `runs_missing_session += 1`;
- a session in `running` with no open run (crash between the framing events and the run insert) →
  append a `note` event `turn interrupted during crash recovery`, set the session `idle`, and let
  the next dispatch resume; this does not create a run row; `sessions_repaired += 1`;
- a **closed** run whose session has no `turn.ended` after the run's start (crash between
  `finish_run` and the terminal events) → append `note` `run ended during crash recovery`, set the
  session `idle` (or `blocked` for a limit outcome), `terminal_events_repaired += 1`;
- finished run with no cost ledger row → append a `note` ledger row `cost missing after crash`,
  `ledgers_repaired += 1`;
- lease whose session is gone → delete, `leases_dropped += 1`.

The crash-injection test (§15) kills a child process at each of the write steps and asserts the
counts above and that no event seq is duplicated.

### 3.8 Project-scoped `Deps` (the multi-project rule)

The v0.1 `Deps` struct does not change and its existing struct literals in tests stay valid.
`Deps::open_store()` is redefined as:

```rust
impl Deps {
    /// Opens the store bound to this `Deps`'s project.
    ///
    /// Resolution (binding), in order:
    /// 1. `config.project.name` is registered -> bind it;
    /// 2. exactly one non-reserved project is registered -> bind it (the v0.1 single-project
    ///    tests and `bundle.rs` after it registers `probe-project`);
    /// 3. no non-reserved project -> ensure the reserved `_node` row and DB, bind `_node`
    ///    (fresh homes and the project-less v0.1 core tests);
    /// 4. several non-reserved projects and the config project is absent -> `NoProject`;
    ///    never guess.
    ///
    /// The injected clock is applied to both opened handles. This is what
    /// `deps_open_store_uses_injected_clock` asserts; without it the store stamps rows with
    /// `SystemClock`.
    pub fn open_store(&self) -> Result<Store, CoreError> {
        let global = Store::open_global(&self.paths)?.with_clock(self.clock.clone());
        let projects = global.list_projects()?;
        let real = projects.iter().filter(|p| p.name.as_str() != "_node").count();
        let name = if projects.iter().any(|p| p.name == self.config.project.name) {
            self.config.project.name.clone()
        } else if real == 1 {
            projects
                .iter()
                .find(|p| p.name.as_str() != "_node")
                .map(|p| p.name.clone())
                .ok_or(StoreError::NoProject)?
        } else if real == 0 {
            let node = ProjectName::parse("_node")?;
            if !projects.iter().any(|p| p.name == node) {
                // `upsert_project` creates projects/_node/project.db (idempotent).
                global.upsert_project(&Project {
                    name: node.clone(),
                    path: self.paths.root().display().to_string(),
                    remote_url: None,
                    default_branch: "main".to_owned(),
                    config: serde_json::json!({}),
                    created_at: self.clock.now(),
                    updated_at: self.clock.now(),
                })?;
            } else {
                Store::create_project_db(&self.paths, &node)?;
            }
            node
        } else {
            return Err(CoreError::Store(StoreError::NoProject));
        };
        drop(global);
        Ok(Store::open_with_project(&self.paths, &name)?.with_clock(self.clock.clone()))
    }
}
```

Consequences (binding):

- `NodeSupervisor` loads each registered project's resolved `VowpConfig` (its `vowp.toml` plus the
  node file) and builds a **per-project `Deps`** with that config, the same `paths`, `node`,
  `clock`, `log`, `git`, `cmd`, `windows` and a registry built from that config. Every runner, gate,
  capsule, promote and schedule path continues to call `deps.open_store()` and therefore gets the
  right project DB with no signature change.
- `Ctx::store()` in the CLI uses `Deps::open_store()`, the same fresh-home/`_node` fallback as the
  core. This is what lets `vowp reindex` reopen a deleted `vowp.db` when the registry is empty.
  Commands that explicitly target a named project (`project add`, `init`) create the DB and then
  open `Store::open_with_project(paths, project)`; fleet commands open the global handle explicitly.
  `Ctx::store()` never guesses a project when several are registered and the selected one is
  missing; `Deps::open_store()` returns `NoProject` in that case.
- `vowp reindex` after `vowp.db` is deleted uses the same fresh-home fallback: it opens the new
  global DB (the fallback may create the empty `_node` DB), rebuilds `sessions`/`session_events`
  from `sessions/*/log.jsonl`, and does not require the deleted project DB. The v0.1
  `reindex_after_db_delete_via_cli` test keeps passing.
- `vowp init` and `vowp project add` register the project, call
  `Store::create_project_db(paths, new_project)`, then open
  `Store::open_with_project(paths, new_project)` for the loop rows, worker slots and DB checks. A
  handle bound to project A never writes project B.
- A project-less orchestrator session uses `_node` for its run rows (below); its session, capsule
  and surface rows are global, so no project card is involved.
- `crates/vowp-core/src/deps.rs` holds this body. The signature is unchanged; the implementation
  replaces the v0.1 `Store::open` call with the resolution above (the ~35-line body, not a one-line
  call) together with the new `Store` constructors. This is the only `vowp-core` file the store
  split changes, and it is recorded in §14.1.

The v0.1 core tests below call project-scoped methods on a fresh temporary home and are the
compatibility gate for this binding. None of them is edited; the store split runs all of them:

| Test | What it does | Why it passes |
|---|---|---|
| `vowp-core/tests/deps.rs::deps_open_store_creates_the_node_database` | opens a `Deps` over an empty home | fallback 3 creates `_node`; `vowp.db` exists and `schema_version()` is 1 |
| `vowp-core/tests/deps.rs::deps_open_store_uses_injected_clock` | `push_request("wake")` after `open_store` | both `open_global` and `open_with_project` get `.with_clock(self.clock.clone())`; the wake row is a node key |
| `vowp-core/tests/session_run.rs` | project-less sessions, `run_turn` -> `insert_run` | fallback 3 binds `_node`; the run row is written there |
| `vowp-core/tests/capsule.rs` | project-less capsule sessions | fallback 3 binds `_node`; the project `ledger` rows are written there, while `capsules` stays global |
| `vowp-core/tests/switch_drain.rs` | `deps.open_store()` -> `insert_run` | fallback 3 binds `_node` |
| `vowp-core/tests/bundle.rs` | fresh home, `upsert_project(probe-project)`, `insert_job` | `open_store` binds `_node`; `upsert_project` creates `probe-project`'s DB and re-binds the `_node` handle; the later `open_store` calls see exactly one non-reserved project and bind `probe-project` |

This is the fix for the one place where the plan's "the split stays inside `vowp-store`" claim does
not survive the multi-project correction: the store can only attach one project, so the **caller**
must say which one, and `config.project.name` is the project the caller already resolved. The
caller body lives in `Deps`; no other module touches the store binding.

**Reserved `_node` project.** `init`/`setup` create one hidden registry row `_node` with
`~/.vowp/projects/_node/project.db`. It owns the `runs`/`ledger` rows of project-less sessions (an
orchestrator `vowp chat`), node-wide L6/L9 rows and their execution history. `sessions.project`
stays `NULL` for a project-less session; `SessionService` binds `_node` when it writes that
session's run, and `run_usage.project='_node'`. `vowp project ls` hides `_node` unless `--all`;
`project rm/pause/archive` refuse it. The registry row is otherwise a normal project with no jobs,
so the same `Deps`/store code path works. Concretely, the session service uses
`Store::open_with_project(paths, &ProjectName::parse("_node"))` for a project-less session's run
rows, or a `Deps` clone whose `config.project.name` is `_node`; no other code special-cases it.
`_node` is not a project card in the snapshot (§5.3); its runs appear in `capacity` and the
orchestrator/session cards.

`ProjectName::parse` accepts exactly the reserved literal `_node`; every other name still follows
the slug rule (lowercase letters, digits and inner `-`). This is a one-line exception in
`vowp-protocol/src/ids.rs`, recorded in §14.1, because the store, migration and session service
must parse the reserved name; `project add`/`init` still reject `_node` as a user project.

One explicit exception to `deps.open_store()`: `SessionService::run_turn` and `write_capsule` read
the session row first and, when `sessions.project IS NULL`, open `Store::open_with_project(paths,
"_node")` for the run/capsule project rows. Every other caller has a project already. That line
lives in `session/service.rs`.

### 3.9 Migration from v0.1 (`vowp migrate`)

The migration is a **Store function plus a CLI wrapper**, not a set of `ALTER TABLE`s that assume
one file. It is restartable. It refuses while a supervisor is running. Full steps:

1. **Preflight.** Open the v0.1 DB (`user_version=1`, project tables present) with a raw connection.
   Refuse if `~/.vowp/supervisor.lock` names a live pid, if `PRAGMA integrity_check` is not `ok`,
   or if `journal_mode` is not WAL. Read `projects`; every project row is split into its own DB.
   The **primary project** is the one with the most jobs (ties: lexicographically first); runs with
   `job_id IS NULL` (planner/keeper/capsule/probe/gate history) go there. Workers do **not** go to
   the primary by default: step 6 assigns each worker to its job's project and reports unassignable
   rows. Refuse if a job,
   loop, schedule, inbox or release names a project that has no registry row, if an inbox
   `job_id` does not resolve to a `jobs` row (it would become a job-bearing `node_inbox` item with
   no project), or if an edge's two jobs are in different projects (v0.1 never wrote those; report
   the ids). This handles a real v0.1 home that registered a second project before discovering that
   one supervisor cannot run it.
2. **Backup.** `Store::backup` the whole file to
   `~/.vowp/backups/pre-0.2-<UTC timestamp>.db`; fsync the backups directory; print the path.
3. **Manifest.** Write `~/.vowp/migration.json` (0600) with
   `{ "from": 1, "to": 2, "backup": "...", "stage": "~/.vowp/tmp/migrate-<ULID>", "state": "copying" }`.
4. **Stage.** Create `stage/global.db` with `schema/global.sql` and one
   `stage/projects/<name>/project.db` (with `schema/project.sql`) for **every** `projects` row in
   the v0.1 DB, including empty/stale rows, plus the reserved `_node` project. The registry and
   the files stay consistent. The preflight still requires every project name to be valid and no
   cross-project edges.
5. **Copy the global rows.** `ATTACH` the v0.1 file read-only as `old`; the staging file is
   `main` and already has the §3.2 schema, so the copy is a plain `INSERT … SELECT` into tables
   that **never had** the cross-file foreign keys. There is no table rebuild, no `ALTER TABLE …`
   and no moment where a foreign key points at a missing table. Copy `kv`, `nodes`, `projects`,
   `sessions`, `session_events`, `session_natives`, `capsules`, `switches`, `limits`,
   `node_inbox` and `node_ledger`. The registry copy adds the new columns:

   ```sql
   INSERT INTO main.projects (name, path, remote_url, default_branch, config_json,
                              state, weight, paused_reason, archived_at, created_at, updated_at)
   SELECT name, path, remote_url, default_branch, config_json,
          'active', 1, '', NULL, created_at, updated_at
   FROM old.projects;

   INSERT INTO main.projects (name, path, remote_url, default_branch, config_json,
                              state, weight, paused_reason, archived_at, created_at, updated_at)
   VALUES ('_node', :home_path, NULL, 'main', '{}', 'active', 1, '', NULL, :now, :now);
   ```

   The node-level rows are:

   ```sql
   INSERT INTO node_inbox (id, project, job_id, kind, title, body, options_json,
                           state, answer, answered_by, created_at, answered_at)
   SELECT i.id,
          (SELECT j.project FROM old.jobs j WHERE j.id = i.job_id),
          i.job_id, i.kind, i.title, i.body, i.options_json,
          i.state, i.answer, i.answered_by, i.created_at, i.answered_at
   FROM old.inbox i WHERE i.project IS NULL;

   INSERT INTO node_ledger (id, ts, kind, project, job_id, run_id, session_id, role,
                            provider, harness, model, usd, tokens_in, tokens_out,
                            tokens_reasoning, tokens_cache_read, tokens_cache_write,
                            cost_source, summary, detail_json)
   SELECT id, ts, kind, NULL, job_id, run_id, session_id, role,
          provider, harness, model, usd, tokens_in, tokens_out,
          tokens_reasoning, tokens_cache_read, tokens_cache_write,
          cost_source, summary, detail_json
   FROM old.ledger WHERE old.ledger.project IS NULL;
   ```

   `sessions` is copied without the v0.1 `job_id REFERENCES jobs(id)` clause, because the
   destination table (§3.2) has a plain `job_id TEXT`. Its children (`session_events`,
   `session_natives`, `capsules`, `switches`) are copied after it and keep their same-file
   references to `sessions`:

   ```sql
   INSERT INTO main.sessions (id, project, job_id, role, title, parent_session_id, node, cwd,
                              state, executor, last_seq, last_capsule_id, log_bytes,
                              created_at, updated_at)
   SELECT id, project, job_id, role, title, parent_session_id, node, cwd,
          state, executor, last_seq, last_capsule_id, log_bytes, created_at, updated_at
   FROM old.sessions;
   ```

   > If a future migration must rebuild a table **in place**, use SQLite's 12-step recipe:
   > `PRAGMA foreign_keys=OFF`, `BEGIN`, create `<table>_new` with the new DDL, `INSERT … SELECT`,
   > `DROP TABLE <table>`, `ALTER TABLE <table>_new RENAME TO <table>`, `COMMIT`,
   > `PRAGMA foreign_keys=ON`, `PRAGMA foreign_key_check`. The shipped v0.1→v0.2 migration does
   > not need it because it writes fresh files; the recipe is recorded here for a future migration
   > of a database that is already split.

6. **Copy the project rows.** For **each** registered project `p`, attach its staging DB as
   `project` on the same connection (`ATTACH DATABASE 'stage/projects/<p>/project.db' AS project`)
   and copy:

   ```sql
   INSERT INTO project.jobs          SELECT * FROM old.jobs          WHERE project = :p;
   INSERT INTO project.edges         SELECT e.* FROM old.edges e
                                     JOIN old.jobs j ON e.src = j.id  WHERE j.project = :p;
   INSERT INTO project.runs          SELECT r.* FROM old.runs r
                                     LEFT JOIN old.jobs j ON r.job_id = j.id
                                     WHERE j.project = :p
                                        OR (r.job_id IS NULL AND :p = :primary);
   INSERT INTO project.gate_results  SELECT g.* FROM old.gate_results g
                                     JOIN old.jobs j ON g.job_id = j.id WHERE j.project = :p;
   INSERT INTO project.workers
   SELECT w.* FROM old.workers w
   LEFT JOIN old.jobs j  ON w.job_id = j.id
   LEFT JOIN old.runs r  ON w.run_id = r.id
   LEFT JOIN old.jobs rj ON r.job_id = rj.id
   WHERE j.project = :p
      OR (w.job_id IS NULL AND rj.project = :p)
      OR (w.job_id IS NULL AND w.run_id IS NULL AND :p = :primary);
   INSERT INTO project.loops         SELECT * FROM old.loops
                                     WHERE project = :p OR (project IS NULL AND :p = :primary);
   INSERT INTO project.schedules     SELECT * FROM old.schedules
                                     WHERE project = :p OR (project IS NULL AND :p = :primary);
   INSERT INTO project.inbox         SELECT * FROM old.inbox         WHERE project = :p;
   INSERT INTO project.ledger        SELECT * FROM old.ledger        WHERE project = :p;
   INSERT INTO project.releases      SELECT * FROM old.releases      WHERE project = :p;
   ```

   Node-wide `loops`/`schedules` (`project IS NULL`) go to the primary project and are listed in the
   report; a v0.1 home had one running project, so this preserves the rows without duplicating a
   command into every project. **Workers follow their job's project**: a worker whose `job_id`
   belongs to project `p` is copied to `p`, so its `job_id`/`run_id` foreign keys resolve inside
   that DB. A worker with no job but a run whose run belongs to `p` also goes to `p`; an idle worker
   with neither goes to the primary. A worker row that cannot be assigned to exactly one project
   (for example a run with `job_id IS NULL`, such as a planner/keeper run) is **dropped and listed
   in the report**; worker slots are advisory and the node supervisor recreates idle slots with
   `ensure_worker_slots`. This avoids the `FOREIGN KEY constraint failed` a blind
   `INSERT INTO project.workers SELECT * ... WHERE :p = :primary` produces on a multi-project home.
   Runs with `job_id IS NULL` (planner/keeper/capsule/probe/gate history) go to the primary project;
   the report names them.

   The destination tables have the same column order as v0.1, so `SELECT *` is safe here; the
   destination DDL is the authority and a schema test compares the two column lists at build time.
   The copy runs in one transaction with `PRAGMA defer_foreign_keys=ON`, so the project's self
   references (`edges`, `gate_results`, `workers`, `releases.rolled_back_by`) are checked at
   commit rather than in row order. `DETACH DATABASE project` before attaching the next project; a
   connection never holds two project DBs at once (§3.1 rule 3).
7. **Verify.** Per project and per table, source count vs destination count (a table list is part
   of the report). Then:
   - `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on both files;
   - every `jobs.project`, `releases.project` equals `:p`;
   - every `runs.session_id` is `NULL` or exists in global `sessions`;
   - every `inbox.job_id` is `NULL` or exists in project `jobs`;
   - every `node_inbox.job_id` is `NULL` or has a non-NULL `project` that exists in `projects`;
   - every `workers.job_id`/`workers.run_id` is `NULL` or exists in that project's `jobs`/`runs`;
     the number of dropped unassignable workers equals the report's count;
   - no project table contains a `REFERENCES projects` clause (`PRAGMA foreign_key_list`);
   - `SELECT COUNT(*) FROM old.inbox WHERE project IS NOT NULL` equals the project inbox count and
     the old node count equals `node_inbox`.
   Any failure aborts, leaves the original file untouched, and prints the failing table.
8. **Swap (crash-safe).**
   - `state='swapping'` in the manifest;
   - rename `vowp.db` → `backups/pre-0.2-<ts>.db` (the backup already exists; this is the same
     bytes, and the rename is atomic on one filesystem);
   - rename `stage/global.db` → `vowp.db`;
   - if `projects/` exists, rename it aside to `backups/projects-before-0.2-<ts>/`, then rename
     `stage/projects` → `projects`;
   - fsync the home directory; set `state='done'`; remove the manifest.
9. **Resume.** On startup, if `migration.json` exists:
   - `state='copying'`: the original is intact; delete the stage and restart from step 1;
   - `state='swapping'`: if `vowp.db` has `user_version=2`, finish the directory renames; if it is
     still v1, rename the staged files into place; if both are ambiguous, restore the backup and
     report `MigrationRequired`.
10. **Rollback.** `vowp migrate --rollback <backup>` (the backup path printed by step 2) refuses if
    a supervisor runs, moves the current `vowp.db` to `backups/failed-0.2-<ts>.db`, copies the
    backup back, moves `projects/` to `backups/0.2-projects-<ts>/`, and removes `migration.json`.
    Session logs are unchanged by migration and are not rolled back. The acceptance test #6 does a
    row-count check after the rollback.

`vowp migrate --dry-run` performs step 1, computes the per-table counts and file sizes, and prints
the backup path it would create, without writing anything. `vowp up` runs the migration
automatically after step 2's backup unless `--no-migrate` is given.

### 3.10 Backup and restore units

- `vowp backup` copies the global DB and every registered project DB with SQLite's online backup,
  plus the **authoritative session content** (`sessions/<id>/log.jsonl`, `blobs/`, `capsules/`,
  `adapters.json`, `summary.md`) and the catalog. One run is one directory
  `backups/<UTC timestamp>/` containing `global.db`, `projects/<name>.db`, `sessions/<id>/...` and
  `catalog.json`, with a `manifest.json` of sha256 and row counts.
- Rotation is 7 daily + 4 weekly; a weekly copy is the newest daily of each ISO week.
- Restore (`vowp backup --restore <dir> --to-home <dir>`) writes a fresh home, runs the migration
  checks, and refuses to overwrite an existing `vowp.db` without `--force`.
- `gc` treats backups as `truth` and never deletes them; rotation is `backup`'s job.

---

## 4. One node supervisor for all projects

### 4.1 Process and lock

- `vowp up` starts `NodeSupervisor` in one process; the node lock stays
  `~/.vowp/supervisor.lock` (pid line 1, RFC 3339 start line 2). A second `vowp up` exits 4.
- The systemd unit is a **user** unit for the `vowp` user with linger on:
  `ExecStart=%h/.local/bin/vowp up --foreground`, `Restart=on-failure`,
  `Environment=VOWP_HOME=%h/.vowp`, `KillMode=mixed`. `vowp down` writes `~/.vowp/stop` and
  waits; `--force` signals the pid.
- `vowp up` on a control machine exits 5 with "this is a control machine; the home runs the
  supervisor". `vowp up` in a checkout with a `vowp.toml` registers that project (if new) and
  starts or joins the node supervisor, preserving v0.1 behaviour.

### 4.2 Tick order

`NodeSupervisor::tick` is one node pass:

```text
0. node sample        capacity::sample, surface::sweep_leases, lifecycle::tick
1. global             apply drained requests that target a project; refresh active limits (L6 data is global)
2. projects           for each active project in round-robin order:
                        project L1–L9 steps, each inside catch_unwind + fail-streak handling
                       paused projects get the maintenance subset; archived projects are skipped
3. snapshot           fleet::write_snapshot (after the project steps, so the cards are current)
4. board              write board.json only when exactly one non-reserved project is active (v0.1 compatibility)
5. heartbeat          ~/.vowp/heartbeat and kv['supervisor_pid']
```

- The round-robin start offset advances by one each tick, so no project is always first. Every
  active project gets one L1 tick per node tick; the offset changes which project gets the first
  dispatch decision when capacity is scarce.
- A **paused** project still gets a maintenance tick — requests, reconcile, harvest, stalls, states,
  gates, verify, merge and board — but skips dispatch, triggers and the work-creating schedules, so
  running work finishes and nothing new starts. An **archived** project gets no tick.
- A project step panic or error is caught; `loops['l1-supervisor'].fail_streak` increments and the
  next project runs. At 3 consecutive failures the node supervisor sets
  `projects.state='paused'`, `paused_reason='broken: <error>'`, writes a `node_inbox` `notice` via
  `Store::insert_node_inbox` with `project = Some(<that project>)`
  ("project X paused after 3 failed ticks: …"), and continues the fleet. `vowp project resume`
  clears it. One broken project never stops another.
- `vowp project pause <p>` sets `state='paused'` with reason `owner`, stops new dispatch, and lets
  running work finish. `resume` sets `active` and clears the reason.
- `vowp project archive <p>` sets `state='archived'`; the DB stays and no tick runs.

### 4.3 Public core API

```rust
pub struct NodeSupervisor {
    deps: Deps,
    global: Store,
    projects: BTreeMap<ProjectName, ProjectSupervisor>,
    order: Vec<ProjectName>,
    next: usize,
    started_at: Timestamp,
}

pub struct ProjectSupervisor {
    deps: Deps,
    project: ProjectName,
    store: Store,
    /* the v0.1 Supervisor fields: runners, ledger, schedules, sessions, wake, ... */
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeReconcileReport {
    pub projects: u32,
    pub cross_db: vowp_store::CrossDbReconcile,
    pub migration: Option<MigrationReport>,
    pub per_project: BTreeMap<ProjectName, ReconcileReport>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeTickReport {
    pub at: Timestamp,
    pub steps: Vec<StepReport>,
    pub projects: BTreeMap<ProjectName, TickReport>,
    pub skipped: Vec<(ProjectName, String)>,
}

impl NodeSupervisor {
    pub fn new(deps: Deps) -> Result<Self, CoreError>;
    pub fn startup(&mut self) -> Result<NodeReconcileReport, CoreError>;
    pub fn tick(&mut self) -> Result<NodeTickReport, CoreError>;
    pub fn run(&mut self, cancel: &CancelToken) -> Result<(), CoreError>;
    pub fn wake_handle(&self) -> WakeHandle;
}
```

`ProjectSupervisor` is the v0.1 `Supervisor` with its `project`-specific fields; the implementation
may keep the name `Supervisor` for it behind `pub(crate)` and add `NodeSupervisor` above it. Each
`ProjectSupervisor` holds the per-project `Deps` from §3.8 (its own `vowp.toml` resolved with the
node file), so `deps.open_store()` and every runner/gate/capsule path select that project's DB. The
public command `vowp up` uses `NodeSupervisor`; unit tests of one project keep using
`Supervisor::new` where they already do.

### 4.4 tmux layout

- `vowp-home` session, windows: `board` (node board, all projects), `capacity`
  (`vowp __window capacity`), `orchestrator` (the default orchestrator session, if any).
  `cmd/window.rs` accepts the `capacity` kind and renders the `capacity show` body, redrawing on a
  TTY. The file otherwise accepts only board/supervisor/planner/keeper/inbox/job.
- `vowp-<project>` session per active project, windows `board supervisor planner keeper inbox`
  exactly as v0.1. Job windows remain `job-<id>`.
- All tmux calls use the private server/socket rules of `vowp-tmux`; tests use `FakeWindows` or a
  private `TMUX_TMPDIR` + `-L`.

### 4.5 `vowp up` / `down` / `status` / `board`

- `up [--foreground|--detach] [--once] [--no-tmux] [--no-migrate]`: one node tick with `--once`.
  `--detach` re-executes with `--foreground` under `setsid`.
- `down [--force] [--kill-windows]`: stops the node supervisor, marks open runs `stalled` and
  re-queues them; worktrees stay.
- `status [--check]`: v0.1 keys plus `home`, `projects: [{name,state,running,ready,blocked}]`;
  `--check` still exits 1 on a heartbeat older than 3 minutes.
- `board`: node board with a project column; `--once` prints once. With one project the output is
  byte-compatible with v0.1 (golden test).

### 4.6 Project registry commands

| Command | Behaviour |
|---|---|
| `vowp project add <path> [--name <n>] [--weight <1-100>]` | Validates a git checkout with `vowp.toml`; registers the project; creates `~/.vowp/projects/<n>/project.db`, loop rows and worker slots; creates the tmux session on the next `up`. Idempotent for the same path/name. Exit 4 on a name conflict, 2 on a bad path or config. `_node` is reserved and refused. |
| `project ls` | `NAME STATE WEIGHT PATH UPDATED`; JSON `[ProjectEntry]`. The reserved `_node` project is hidden unless `--all`. |
| `project pause <n>` / `resume <n>` | Sets `active`/`paused`; resume refuses `archived` (exit 4). |
| `project archive <n>` | Sets `archived`, removes the tmux session, keeps the DB. |
| `project rm <n> --yes` | Archives the DB directory to `~/.vowp/archive/projects/<n>-<ts>/`, deletes the registry row, keeps catalog production/truth rows, removes the tmux session. Refuses without `--yes` (exit 5), if the project is running (exit 4), and for `_node`. `sessions.project` keeps the text. |
| `project weight <n> <w>` | Sets the fairness weight (`1..=100`). |

`vowp project add` does not start a supervisor; the next `up` picks it up. `vowp init` in a
checkout is the same registry write plus framework files.

---

## 5. Read model: `snapshot.json`, `fleet` and `schema snapshot`

### 5.1 Format and guarantees

- Path `~/.vowp/snapshot.json`; one JSON document, pretty-printed for humans, UTF-8, trailing
  newline. Written every tick to `~/.vowp/tmp/snapshot.json.tmp`, fsynced, then renamed. A reader
  never sees a partial file.
- `schema` is the literal `vowp-snapshot/1`; `version` is `1`; readers reject a newer `version` and
  ignore unknown fields within a known version.
- The snapshot is **derived**: deleting it loses nothing. `fleet status` recomputes from the DB
  when the file is missing or older than 3 ticks, and `--refresh` always recomputes.
- It never contains a secret value, a lease token or a full event payload. Titles, job ids and
  questions are included; bodies are truncated to 200 chars. `ProjectSnapshot.path` is the
  registered checkout path and stays in the home/control cache; the public examples use `/srv/vowp`.
- `vowp schema snapshot` prints the JSON Schema of `Snapshot` (plus `schema`/`version`).

### 5.2 Types (`vowp-protocol/src/snapshot.rs`)

```rust
pub const SNAPSHOT_FORMAT: &str = "vowp-snapshot/1";
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub schema: String,                    // SNAPSHOT_FORMAT
    pub version: u32,                      // SNAPSHOT_VERSION
    pub sequence: u64,                     // increments every write
    pub generated_at: Timestamp,
    pub home: NodeName,
    pub supervisor: SupervisorSnapshot,
    pub projects: Vec<ProjectSnapshot>,
    pub capacity: CapacitySnapshot,
    pub limits: Vec<Limit>,
    pub inbox: Vec<InboxSummary>,
    pub sessions: Vec<SessionSurfaceCard>,
    pub orchestrators: Vec<OrchestratorCard>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SupervisorSnapshot {
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub last_tick: Option<Timestamp>,
    pub tick_secs: u64,
    pub paused: bool,
    pub fail_streak: u32,
    pub version: String,                   // binary version, e.g. "0.2.0"
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectSnapshot {
    pub name: ProjectName,
    pub path: String,
    pub state: ProjectState,
    pub weight: u8,
    pub paused_reason: String,
    pub jobs_by_state: BTreeMap<String, u32>,
    pub running: Vec<RunningJobCard>,
    pub blocked: Vec<BlockedJobCard>,
    pub queue: Vec<QueueJobCard>,
    pub last_merge: Option<MergeCard>,
    pub gate_failures: Vec<GateFailureCard>,
    pub cost_today_usd: Option<f64>,       // None = unknown, never 0 for unknown
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningJobCard {
    pub job: JobId,
    pub kind: JobKind,
    pub role: Role,
    pub executor: Option<String>,
    pub node: NodeName,
    pub age_secs: u64,
    pub last_event_age_secs: Option<u64>,
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BlockedJobCard {
    pub job: JobId,
    pub state: JobState,
    pub reason: String,
    pub detail: String,                    // <= 200 chars, redacted
    pub since: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueueJobCard {
    pub job: JobId,
    pub title: String,
    pub kind: JobKind,
    pub role: Role,
    pub priority: i32,
    pub state: JobState,
    pub age_secs: u64,
    pub waiting_reason: Option<String>,    // "waiting: RAM 1.2 GB short", ...
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MergeCard {
    pub job: JobId,
    pub at: Timestamp,
    pub commit: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GateFailureCard {
    pub job: JobId,
    pub gate: String,
    pub at: Timestamp,
    pub evidence: String,                  // one redacted line, <= 200 chars
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboxSummary {
    pub id: InboxId,
    pub project: Option<ProjectName>,
    pub job_id: Option<JobId>,
    pub kind: InboxKind,
    pub title: String,
    pub created_at: Timestamp,
    pub age_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSurfaceCard {
    pub id: SessionId,
    pub project: Option<ProjectName>,
    pub job_id: Option<JobId>,
    pub role: Option<SessionRole>,
    pub title: String,
    pub state: SessionState,
    pub executor: Option<String>,
    pub node: NodeName,
    pub active_surface: Option<SurfaceRef>,
    pub last_event_at: Timestamp,
}

```

`CapacitySnapshot` is defined in §6.1; `SurfaceRef` is defined in `surface.rs` (§7.3) and used by the session card; `OrchestratorCard` and `JoinInstructions` in §7.6;
`SurfaceId`/`SurfaceKind`/`Lease` in §7.3.

### 5.3 What each card contains (binding order)

- `projects`: registry order `name` ascending, all states, excluding the reserved `_node` project.
  `archived` projects have an empty `running`/`queue` and only their last recorded cost.
- `queue`: at most 200 rows per project, sorted `priority DESC, created_at ASC`; jobs in
  `pending|ready|parked|stalled`; `waiting_reason` is the last admission/brake reason, else
  `None`.
- `running`: jobs whose state holds the lock, sorted job id; `age_secs` from the open run's
  `started_at` or the state change.
- `blocked`: `parked` and `failed` jobs with the parked reason and the last note (truncated).
- `gate_failures`: the last 10 failed gate results across the project, newest first.
- `last_merge`: the newest `merge` ledger row, or `None`.
- `inbox`: open items only, node and project merged, oldest first, at most 100.
- `sessions`: sessions updated in the last 7 days, newest first, at most 200; `active_surface` from
  a lease whose heartbeat is younger than 60 s.
- `orchestrators`: one card per `~/.vowp/orchestrators/<name>/orchestrator.json`.

### 5.4 Example (abridged)

```json
{
  "schema": "vowp-snapshot/1",
  "version": 1,
  "sequence": 42,
  "generated_at": "2026-11-10T14:02:11.000Z",
  "home": "vowp-home",
  "supervisor": {"running": true, "pid": 1234, "started_at": "2026-11-10T09:00:00.000Z",
                 "last_tick": "2026-11-10T14:02:10.000Z", "tick_secs": 60, "paused": false,
                 "fail_streak": 0, "version": "0.2.0"},
  "projects": [
    {"name": "vowp", "path": "/srv/vowp", "state": "active", "weight": 1, "paused_reason": "",
     "jobs_by_state": {"running": 2, "ready": 3, "done": 41},
     "running": [{"job": "docs-pass", "kind": "docs", "role": "worker",
                  "executor": "opencode:deepseek-chat", "node": "laptop",
                  "age_secs": 123, "last_event_age_secs": 4, "session_id": "01KA1V0Z7A0000000000000000"}],
     "blocked": [], "queue": [{"job": "interfaces", "title": "Document the interfaces", "kind": "build",
                               "role": "worker", "priority": 99, "state": "ready", "age_secs": 60,
                               "waiting_reason": "waiting: RAM 1.2 GB short"}],
     "last_merge": {"job": "release-notes", "at": "2026-11-10T12:00:00.000Z",
                    "commit": "abc1234", "branch": "job/release-notes"},
     "gate_failures": [], "cost_today_usd": 1.23}
  ],
  "capacity": {"measured": {"at": "2026-11-10T14:02:10.000Z", "mem_total_bytes": 8589934592,
                            "mem_available_bytes": 3221225472, "swap_used_bytes": 0,
                            "load1": 1.2, "load5": 1.1, "load15": 0.9, "cpu_count": 2,
                            "cpu_quota_cores": 2.0, "disk_free_home_bytes": 53687091200,
                            "disk_free_worktrees_bytes": 53687091200,
                            "pressure_cpu_avg10": 2.0, "pressure_mem_avg10": 0.0,
                            "pressure_io_avg10": 0.0, "source": "proc"},
                "running": {"runs": 2, "ram_p90_bytes": 3221225472, "cpu_p90_cores": 1.4},
                "target": {"mode": "auto", "target_workers": 2, "advised_workers": 2,
                           "is_override": false, "set_by": "supervisor", "set_at": "2026-11-10T13:00:00.000Z",
                           "reason": ""},
                "advised_target": 2, "brake": {"active": false, "reasons": [], "since": null},
                "reserve_ram_bytes": 1073741824, "min_disk_bytes": 5368709120},
  "limits": [], "inbox": [], "sessions": [], "orchestrators": []
}
```

### 5.5 `fleet` commands

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FleetStatus {
    pub snapshot: Snapshot,
    pub source: String,          // "local" | "remote" | "cache"
    pub fetched_at: Timestamp,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FleetQueueRow {
    pub project: ProjectName,
    pub job: JobId,
    pub title: String,
    pub kind: JobKind,
    pub role: Role,
    pub priority: i32,
    pub state: JobState,
    pub age_secs: u64,
    pub waiting_reason: Option<String>,
}
```

The snapshot `sequence` is read from `kv['snapshot_sequence']`, incremented, and stored with
`kv_set('snapshot_sequence', …)` before the tmp file is renamed. A crash may leave a gap in the
sequence; it never repeats a value (the snapshot is derived, and readers do not require
consecutive numbers).

| Command | Behaviour |
|---|---|
| `vowp fleet status [--json] [--refresh] [--cached]` | Node summary: supervisor, projects, capacity, limits, open inbox. On the home it reads `snapshot.json` when fresh (< 3 ticks) else recomputes; on a control machine it SSHes to the home, caches the result and prints "as of HH:MM" when the home is unreachable. `--json` prints `FleetStatus` (`{snapshot, source, fetched_at, stale}`); `snapshot` is the §5.2 document. |
| `fleet queue [--project <p>] [--state <s>] [--json]` | Flattened `QueueJobCard` rows with `project`; JSON `[FleetQueueRow]` where `FleetQueueRow = {project, ..QueueJobCard}`. |
| `fleet inbox [--project <p>] [--all] [--json]` | Merged `InboxItem` rows (`node_inbox` + project inboxes), oldest first; `--all` includes answered. JSON `[InboxItem]`. |
| `fleet capacity [--json]` | Alias of `capacity show` (§6.5). |

`vowp status` remains the compact human/JSON view (v0.1 keys + `home` and `projects`), and `vowp
board` the refreshing terminal view. Both accept every project.

---

## 6. Capacity: measured, admitted, braked, advised

### 6.1 Measure

- **Sampler thread**, every 5 s while the node supervisor runs:
  - Linux `/proc/meminfo` (`MemTotal`, `MemAvailable`, `SwapTotal`, `SwapFree`);
  - load from `/proc/loadavg`, CPU count from `sysconf` (or `/proc/cpuinfo`), cgroup v2 quota from
    `/sys/fs/cgroup/cpu.max` when present;
  - disk free from `statvfs` on the home and worktree filesystems;
  - `/proc/pressure/{cpu,memory,io}` `avg10` when present;
  - on macOS or without `/proc`: only what `statvfs` and `sysctl` provide; the rest is `None` and
    `source='fallback'`. All Linux-only code is `#[cfg(target_os = "linux")]` or behind a
    `ProcFs` trait faked in tests so the macOS build keeps working.
  - store one `host_samples` row; prune 5 s rows older than 24 h and keep 1-min averages for 30
    days.
- **Per-run usage.** Every adapter run and every command gate gets a scope. Measurements on the
  target VPS proved the cgroup v2 path and fixed the measurement fields:
  - preferred: a systemd user scope per run (`systemd-run --user --scope --unit
    vowp-run-<run-id>`), reading `memory.peak`, `memory.stat` (`anon`, `file`, `kernel`) at the
    peak sample, `cpu.stat` `usage_usec` and `memory.events` (`oom_kill`) from cgroup v2;
    `source='cgroup'`. `memory.peak` is cumulative and the user cannot reset it, so per-phase
    peaks use child cgroups or separate scopes.
  - fallback: walk the process tree from `AdapterRun::pid()` (`/proc/<pid>/task/*/children`
    recursively) every second, sum RSS and CPU deltas; `source='proc_tree'`. Measurements show the
    walk overcounts warm RSS by ~31 % (shared pages counted once per process) and undercounts CPU
    by 15–36 % (short-lived processes exit between samples); it also cannot see a build running
    in a Docker container. It is for keeping a node limping, not for estimates.
  - if neither is available: `source='default'`, `peak_rss_bytes = 1_500_000_000`,
    `mean_cpu_cores = 1.0`, labelled "default".
  - The capacity sampler records one `run_usage` row per finished run with `peak_rss_bytes`, the sampled
    `anon_peak_bytes`/`file_peak_bytes`/`kernel_peak_bytes` (NULL when not sampled), `cpu_usec`,
    `mean_cpu_cores`, `max_cpu_cores`, `build_mode` (`cold`/`warm`/`incremental`/`unknown`,
    inferred from the job target directory), `source` (`cgroup`/`proc_tree`/`default`),
    `oom_killed` from the cgroup `memory.events` `oom_kill` delta (fallback: outcome `killed` + a
    memory-heavy estimate) and `exit_code`.
- **Measurements behind the defaults and reserve.** A cold `cargo test --workspace` (debug info
  off, incremental off) peaked at **2.45 GiB** and used **300 s CPU**; warm runs peaked at
  **0.37 GiB** and **56–60 s CPU**. The build was 91.6 % of cold peak RAM and 99.6 % of cold CPU;
  on warm runs 44 % of peak RAM and 98 % of CPU. The plan's `1.5 GiB / 1 core` default is
  plausible for a warm gate run and too low for a cold Rust build; admission replaces it with p90
  estimates from real runs as soon as they exist. Disk is a first-class dimension: the 290 GB test disk was 98–100 % full
  and builds failed with `No space left on device` / `sqlite: disk I/O error`, so keep the
  5 GB / 10 % reserve and add a per-job target-dir budget/cleanup. `MemoryHigh` is a working soft
  throttle (a 96 MiB allocation under `MemoryHigh=64M` produced 486 `high` events, no OOM kill);
  do not use it as a hard build cap until estimates exist. `MemoryMax` is writable but was not
  used because killing a build is worse than throttling it.
- **No `/proc` (macOS or a minimal container).** The sampler writes `source='fallback'` samples with
  the fields it can read. Admission skips the RAM/CPU checks and uses the owner's `fixed` target
  when one is set, else `1`; the disk check still runs. The board and `capacity show` print
  `capacity: fallback (no /proc), target <n>` so the degradation is visible, exactly as the plan
  requires. `auto` never silently claims a measured target on such a host.
- **Estimates.** One `usage_estimates` row per exact `(project, job_kind, harness, model,
  build_mode)` from the last 50 `run_usage` rows. With fewer than 5 samples, pool in this order
  until a pool has 5: exact → same `(project, job_kind, harness, build_mode)` → same
  `(project, job_kind, build_mode)` → same `(job_kind, build_mode)` across projects →
  `('*','*','','','unknown')` constant. A `cold` estimate never pools into `warm`/`incremental`
  before the constant; measurements show a 6.6× RAM and 5× CPU gap between cold and warm. `source`
  records `measured`, `pooled` or `default`. Percentiles are nearest-rank p50/p90; integer bytes
  and milliseconds; CPU in cores as `f64`.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostSample {
    pub at: Timestamp,
    pub interval_secs: u64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    pub swap_used_bytes: u64,
    pub load1: f64, pub load5: f64, pub load15: f64,
    pub cpu_count: u32,
    pub cpu_quota_cores: Option<f64>,
    pub disk_free_home_bytes: u64,
    pub disk_free_worktrees_bytes: Option<u64>,
    pub pressure_cpu_avg10: Option<f64>,
    pub pressure_mem_avg10: Option<f64>,
    pub pressure_io_avg10: Option<f64>,
    pub source: SampleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource { Proc, Fallback }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunUsage {
    pub run_id: RunId,
    pub project: ProjectName,
    pub job_id: Option<JobId>,
    pub job_kind: Option<JobKind>,
    pub role: RunRole,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub node: NodeName,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    pub duration_ms: u64,
    pub peak_rss_bytes: u64,
    pub anon_peak_bytes: Option<u64>,
    pub file_peak_bytes: Option<u64>,
    pub kernel_peak_bytes: Option<u64>,
    pub cpu_usec: u64,
    pub mean_cpu_cores: f64,
    pub max_cpu_cores: f64,
    pub build_mode: BuildMode,
    pub source: UsageSource,
    pub oom_killed: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource { Cgroup, ProcTree, Default }

/// Inference from the job's target directory; `unknown` when it cannot be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode { Cold, Warm, Incremental, #[default] Unknown }

/// What a remote `__exec` measured on the executor node; `ingest` fills the rest of RunUsage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteUsage {
    pub peak_rss_bytes: u64,
    pub anon_peak_bytes: Option<u64>,
    pub file_peak_bytes: Option<u64>,
    pub kernel_peak_bytes: Option<u64>,
    pub cpu_usec: u64,
    pub mean_cpu_cores: f64,
    pub max_cpu_cores: f64,
    pub build_mode: BuildMode,
    pub oom_killed: bool,
    pub source: UsageSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageEstimate {
    pub project: Option<ProjectName>,      // None = the '' pooled key
    pub job_kind: Option<JobKind>,         // None = the '' pooled key
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub build_mode: Option<BuildMode>,
    pub samples: u32,
    pub ram_p50_bytes: u64,
    pub ram_p90_bytes: u64,
    pub cpu_p50_cores: f64,
    pub cpu_p90_cores: f64,
    pub duration_p50_ms: u64,
    pub duration_p90_ms: u64,
    pub source: EstimateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EstimateSource { Measured, Pooled, Default }

#[derive(Debug, Clone, Default)]
pub struct UsageQuery {
    pub project: Option<ProjectName>,
    pub job_kind: Option<JobKind>,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub build_mode: Option<BuildMode>,
    pub since: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstimateKey {
    pub project: Option<ProjectName>,      // None = the '' pooled key
    pub job_kind: Option<JobKind>,         // None = the '' pooled key
    pub harness: Option<HarnessKind>,      // None means the pooled '' key
    pub model: Option<String>,             // None means the pooled '' key
    pub build_mode: Option<BuildMode>,     // None means the pooled 'unknown' key
}
```

```rust
pub struct SamplerHandle { /* thread + cancel */ }
impl SamplerHandle {
    /// Starts one sampler thread (5 s) writing host_samples until cancel.
    pub fn start(deps: &Deps) -> Result<SamplerHandle, CoreError>;
    pub fn stop(self) -> Result<(), CoreError>;
}

pub struct RunScope { /* cgroup path or root pid + baseline */ }
impl RunScope {
    pub fn begin(deps: &Deps, run: &RunId, root_pid: u32) -> Result<RunScope, CoreError>;
    pub fn sample(&self) -> Result<ScopeSample, CoreError>;
    /// Stops sampling and writes the run_usage row; `build_mode` is inferred before this call.
    pub fn finish(self, store: &Store, build_mode: BuildMode) -> Result<RunUsage, CoreError>;
}
pub struct ScopeSample {
    pub peak_rss_bytes: u64,
    pub anon_peak_bytes: Option<u64>,
    pub file_peak_bytes: Option<u64>,
    pub kernel_peak_bytes: Option<u64>,
    pub cpu_usec: u64,
    pub mean_cpu_cores: f64,
    pub max_cpu_cores: f64,
    pub oom_killed: bool,
}

/// The Linux/macOS reads the sampler and scope need; `FakeProcFs` replays fixtures in tests.
pub trait ProcFs: Send + Sync {
    fn read(&self, path: &str) -> Result<String, CoreError>;
    fn children(&self, pid: u32) -> Result<Vec<u32>, CoreError>;
    fn disk_free(&self, path: &std::path::Path) -> Result<u64, CoreError>;
}
pub struct SystemProcFs;
pub struct FakeProcFs { /* fixture map */ }
```

`CapacitySnapshot` (used in §5.2):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacitySnapshot {
    pub measured: Option<HostSample>,
    pub running: RunningCapacity,
    pub target: CapacityTarget,
    pub advised_target: Option<u32>,
    pub brake: BrakeState,
    pub reserve_ram_bytes: u64,
    pub min_disk_bytes: u64,
}
```

### 6.2 Admit

Before any worker/verifier/planner/keeper/merge run:

```rust
pub struct AdmitRequest {
    pub project: ProjectName,
    pub job: Option<JobId>,
    pub kind: JobKind,
    pub role: RunRole,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub node: NodeName,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdmitDecision {
    Admit,
    Wait(WaitReason),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WaitReason {
    pub code: WaitCode,
    pub detail: String,        // one human sentence, e.g. "RAM 1.2 GB short"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitCode { Ram, Cpu, Disk, Provider, Brake, Target }
```

Rules (deterministic; every constant is named in `capacity/admit.rs`):

1. **Brake first.** If `BrakeState.active` → `Wait(Brake)` with the first brake reason.
2. **Provider.** If the run's provider has an active limit → `Wait(Provider)`.
3. **Target.** If `running_runs >= target_workers` → `Wait(Target)`; `target_workers` is the
   current `CapacityTarget` (owner `fixed` or measured `auto`). This is a concurrency limit, not a
   hard-coded maximum: the owner can raise it.
4. **RAM.** `mem_available - estimate.ram_p90_bytes >= reserve_ram_bytes`, where
   `reserve_ram_bytes = max(1 GiB, 10% of MemTotal)`. Otherwise `Wait(Ram)`.
5. **CPU.** `(running_cpu_cores + estimate.cpu_p90_cores) / cpu_quota_cores <= 0.85`. Otherwise
   `Wait(Cpu)`.
6. **Disk.** `disk_free_home >= max(5 GiB, 10% of the filesystem)` and, when known, worktree disk
   above the same minimum. Otherwise `Wait(Disk)`.
7. **Fairness is ordering, not a veto.** When several projects have ready jobs and capacity is
   scarce, the supervisor orders candidates by `weight` descending, then the project admitted least
   recently (`kv['capacity:last_project']`), then job priority. The round-robin start offset rotates
   each tick, so no project is permanently first. Admission has no fairness wait; it only reports
   the resource reasons above.
8. `max_workers` in `vowp.toml` is **a preference, not a cap**: it seeds the advisor's suggestion
   and `ensure_worker_slots`, but never blocks admission. The owner's `capacity set` and the RAM/CPU
   checks are the real limits. The dispatch caps passed to `plan_dispatch` are derived from
   `target_workers - running_runs` (and the verifier share), **not** from the idle `workers` rows;
   the `workers` table stays a board/tmux bookkeeping view and cannot veto an admitted run.
   `ensure_worker_slots` grows to `max(config.node.max_workers, target_workers)` when the target is
   raised, so the board still shows a slot per run.

`Wait` leaves the job `ready` (or `pending`), writes the reason to `jobs.note`
(`waiting: RAM 1.2 GB short`) and to the snapshot queue card. It never changes job state.

Admission is **node-wide**: the manager opens each active project DB in turn (never two at once),
sums the open runs and their estimated CPU, and counts `running_runs` across projects against the
node target. A per-project handle never decides the node's capacity from its own runs only.

### 6.3 Brakes

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrakeState {
    pub active: bool,
    pub reasons: Vec<BrakeReason>,
    pub since: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrakeReason {
    pub code: BrakeCode,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrakeCode { MemoryPressure, SwapGrowth, DiskLow, Provider }
```

- `MemoryPressure`: `/proc/pressure/memory` `avg10 > 10.0` for two consecutive samples.
- `SwapGrowth`: `swap_used` grew by more than 64 MiB/min over the last three samples.
- `DiskLow`: home or worktree disk below the §6.2 minimum.
- `Provider`: every provider of the project's next run is limited.
- Brakes clear automatically when the condition clears for two samples. Running work is never
  signalled. A kernel OOM kill is recorded (`run_usage.oom_killed=1`), the job is parked with a
  `node_inbox` notice via `Store::insert_node_inbox` with `project = Some(<job project>)`, and its
  estimate's p90 is raised by 25% for the next admission.
- The board shows `brake: memory pressure (avg10 14.2)` and the owner override line
  `owner override: 8 (advised 5)`.

### 6.4 Targets and advice

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode { Auto, Fixed }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityTarget {
    pub mode: TargetMode,
    pub target_workers: u32,
    pub advised_workers: Option<u32>,
    pub is_override: bool,
    pub set_by: String,        // "owner" | "supervisor" | "setup"
    pub set_at: Timestamp,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityAdvice {
    pub id: String,            // ULID
    pub created_at: Timestamp,
    pub current_target: u32,
    pub proposed_target: u32,
    pub mode: TargetMode,
    pub reasons: Vec<String>,
    pub expected_effect: String,
    pub inbox_id: Option<InboxId>,
    pub state: AdviceState,
    pub applied_at: Option<Timestamp>,
    pub applied_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdviceState { Open, Applied, Dismissed }
```

- `auto` (default): `advised_workers = clamp(min(ram_fit, cpu_fit, provider_cap), 1, 64)` where
  `ram_fit = floor((mem_available - reserve) / pooled_ram_p90)`,
  `cpu_fit = floor(cpu_quota * 0.85 / pooled_cpu_p90)`, `provider_cap = min(max_concurrent)`.
  The value is written as a `capacity_targets` row with `set_by='supervisor'` when it changes by
  more than one worker, and appears as `advised_target` in the snapshot. With no row at all,
  `current` returns `mode=auto`, `target_workers=advised (at least 1)`, `set_by='supervisor'`,
  `set_at=now`.
- `fixed N`: the owner's number. `N > advised` requires `--override`; it is honoured and shown as
  `owner override: N (advised A)`. Brakes and per-run checks still apply.
- Advice: every 30 minutes and on events (new project, brake, OOM, sustained idle), a
  keeper-profile turn reads `vowp capacity report --json` and writes one `capacity_advice` row and
  one `node_inbox` `decision` item titled `Capacity: 5 workers (advised 5)` with options
  `["apply","keep"]`. The AI never changes the target by itself. Applying is
  `vowp capacity set --target N` (the MCP/chat actor records `applied_by`).

### 6.5 Capacity commands and core API

```rust
pub struct CapacityReport {
    pub generated_at: Timestamp,
    pub host: Option<HostSample>,
    pub target: CapacityTarget,
    pub running: RunningCapacity,
    pub brakes: BrakeState,
    pub reserve_ram_bytes: u64,
    pub min_disk_bytes: u64,
    pub estimates: Vec<UsageEstimate>,
    pub waiting: Vec<WaitingJob>,
    pub limits: Vec<Limit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningCapacity { pub runs: u32, pub ram_p90_bytes: u64, pub cpu_p90_cores: f64 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WaitingJob {
    pub project: ProjectName,
    pub job: JobId,
    pub reason: WaitReason,
    pub since: Timestamp,
}
```

```rust
impl CapacityManager {
    pub fn new(deps: Deps) -> Self;
    pub fn sample(&self, store: &Store, now: Timestamp) -> Result<Option<HostSample>, CoreError>;
    pub fn report(&self, store: &Store, now: Timestamp) -> Result<CapacityReport, CoreError>;
    pub fn admit(&self, store: &Store, req: &AdmitRequest, now: Timestamp) -> Result<AdmitDecision, CoreError>;
    pub fn set_target(&self, store: &Store, mode: TargetMode, target: u32,
                      is_override: bool, by: &str, reason: &str) -> Result<CapacityTarget, CoreError>;
    pub fn advise(&self, store: &Store, now: Timestamp, trigger: AdviceTrigger) -> Result<Option<CapacityAdvice>, CoreError>;
    pub fn recompute_estimates(&self, store: &Store, now: Timestamp) -> Result<u32, CoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceTrigger { Timer, NewProject, Brake, Oom, Idle }
```

| Command | Behaviour |
|---|---|
| `capacity show [--json]` | `CapacityReport`; human: host, running, target/advised, brake, top estimates. |
| `capacity report [--json] [--since <dur\|date>] [--limit <n>]` | Same document with `estimates` limited and a `since` filter on samples; used by the advisor. |
| `capacity set --target <n> [--mode auto\|fixed] [--override] [--reason <t>] [--by <actor>]` | Writes one `capacity_targets` row and returns `CapacityTarget`. `--mode fixed` (the default) requires `--target`; `--mode auto` ignores it and recomputes. `--target` above the advice without `--override` exits 5 with the advised number. |

---

## 7. Sessions, surfaces, remote execution and the apps

The v0.1 session model is reused unchanged: one log, `seq`, capsules, switch, export/import.
v0.2 adds a transport, a lease and app hosts. The core feature is: **any surface can continue the
same session; only the lease holder writes.**

### 7.1 Session roles and the orchestrator

- `SessionRole` gains `Orchestrator` (wire `orchestrator`). An orchestrator session is project-less
  (fleet-wide) or bound to one project. It is an ordinary session with ordinary events.
- No second session type, no second log, no parallel transcript store. An app's transcript is
  mirrored into the same log by `surface/mirror.rs` (§7.5).

### 7.2 Remote execution (`vowp __exec`, `vowp session ingest`)

**Flow for a turn whose executor is the surface's own machine** (laptop):

1. The surface asks the home to begin a turn:
   `vowp session turn-begin <session> --executor <exec> --purpose <p> --node
   <executor-node> --request <file|->` (hidden; used by `chat` and the app host). The `--request`
   document carries the lease token, the prompt and the executor; it is read from stdin/file, never
   argv. The home:
   - validates the lease;
   - resolves the adapter/profile on the home? No: the executor node owns the harness binary, but
     the home owns the session. The home resolves the `Executor` string and the purpose, writes
     `session.executor{attach}`, `turn.started`, `user.message` (the prompt, seeded from the capsule
     when the native session is absent), inserts the project `runs` row with `node=<executor-node>`,
     and returns a grant;
   - prints `TurnGrant { run_id, session_id, executor, purpose, node, cwd, prompt,
     resume_native, native_hint, env_names, lease_token, tail_seq }` as JSON on stdout. The client
     captures it, never logs it, and writes it to a 0600 grant file on the executor machine for
     `__exec`. The token is never in argv and never on disk on the home.
2. The surface runs `vowp __exec --grant <file>` (or the equivalent flags) on its own machine. It
   starts the adapter exactly as `run_turn` would (same `StartInput`), resolving `env_names` from
   the surface's own vault/environment; a missing name is a clear error naming the variable, never
   a silent empty value. The executor node owns the harness binary and any secret it injects; the
   home never sends a value over the wire. It writes every adapter event to its local spool
   **before** putting a `RemoteLine` JSON object on stdout.
3. `vowp __exec` pipes those lines into `ssh <home> vowp session ingest <session> --run
   <run-id>`. The first line is `RemoteLine::Hello`, which carries the lease token from the grant;
   `ingest` validates it before accepting any event, so the token is never in argv or `ps`. It
   assigns `seq`, appends with the supplied event id and timestamp, updates the index, and acks
   `{event_id, seq}`. It also heartbeats the lease. The client keeps a **64-event in-flight
   window** and consumes acks asynchronously; it does not wait for one ack per event across a WAN
   (measured: ~9 events/s with a one-event window at 80 ms RTT, versus ~230–260 events/s with a
   64-event window after setup). When `surface::sweep_leases` detects a takeover, `ingest` pushes
   `IngestAck::LeaseRevoked` on the same stdout stream; the client stops input immediately and
   follows read-only (measured at 99.6–265 ms over live SSH, bound to < 2 s; §7.3).
4. The executor node is registered once with `vowp node add laptop -` (no inbound ssh target); the run row and snapshot name it. On the harness summary, `__exec` calls `vault::redact_job_artifacts` on the executor node
   (the report and raw mirror live in the executor's worktree), sends `RemoteLine::Summary`; `ingest`
   writes `cost`, `limit` or `error`, `turn.ended`, `session.executor{detach}`, finishes the project
   `runs` row, writes the ledger rows, writes the `run_usage` row from `usage` (or a default row
   with `source='default'` when the executor could not measure), and acks `{run_id, done:true}`.
   The home is now the only writer and the turn is complete.

**Spool, ack window and replay.** `__exec` keeps the adapter running and
appends each event to `<spool>/<session>/<run>/events.jsonl`, fsyncing it **before** the send; an
event that was not durably spooled is never sent. Acks are written to `acked.json` by event id. On
reconnect it starts `ingest --replay` and resends every event from the first unacked one; `ingest`
is idempotent by event id (`session_events.id` unique; `Store::event_row_by_id` returns the
existing seq). A known id with different content is `duplicate-conflict` and is not appended. The
spool survives a killed client, not just a killed SSH: a new process reads `events.jsonl` +
`acked.json` and finishes the turn. A torn final line is truncated; corruption before the last
complete line is an error. `control_master` is an optimisation for reconnects and probes, never a
correctness requirement: measurements show a handshake drop from 810–953 ms to 243–256 ms at 80 ms RTT
and a reconnect drop from 292–409 ms to 82–92 ms loopback, but per-event RTT is unchanged. A dead
master falls back to a cold connection and the turn continues. The test kills the fake SSH process
mid-stream (including an ack lost after append and a cut mid-line) and asserts no gap and no
duplicate.

**Wire types** (`vowp-protocol/src/surface.rs`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLine {
    Hello { session_id: SessionId, run_id: RunId, executor: String, node: NodeName,
            spool_id: String, lease: String },
    Event { event: RemoteEvent },
    Heartbeat { at: Timestamp },
    Summary { exit_code: Option<i32>, outcome: RunOutcome, native_session_id: Option<String>,
              cost: Cost, limit: Option<LimitEvent>, result_text: Option<String>,
              stop_reason: StopReason, usage: Option<RemoteUsage> },
}

/// The serializable part of one event, without the home-assigned `session_id`/`seq`.
///
/// `EventBody` itself has no serde implementation (it is the typed view of an envelope's
/// `payload`); `__exec` builds this from `EventBody::event_type()` + `EventBody::to_value()`,
/// and `ingest` rebuilds the body with `EventBody::from_parts` before `append_with_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteEvent {
    pub id: EventId,
    pub ts: Timestamp,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub actor: Actor,
    pub source: EventSource,
    pub payload: serde_json::Value,
    pub raw: Option<serde_json::Value>,
    pub replayable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IngestAck {
    Ack { event_id: EventId, seq: u64 },
    Duplicate { event_id: EventId, seq: u64 },
    Summary { run_id: RunId, done: bool },
    /// Pushed by the home when a takeover revokes this run's lease. The client stops
    /// sending events immediately; it does not wait for its next heartbeat.
    LeaseRevoked { reason: String, by: String, at: Timestamp, last_seq: u64 },
    Error { kind: ErrorKind, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnGrant {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub executor: String,
    pub purpose: RunPurpose,
    pub node: NodeName,
    pub cwd: String,
    pub prompt: String,
    pub resume_native: Option<String>,
    pub native_hint: Option<String>,
    pub env_names: Vec<String>,
    pub lease_token: String,
    pub tail_seq: u64,
}
```

`RemoteLine`/`IngestAck` are JSON lines, one per line, on stdout/stdin. They never carry a secret
value; `env_names` is names only. `lease_token` is in the grant file (0600), not in argv (argv is
visible in `ps`); the CLI accepts `--grant <file>` and reads it with mode 0600.

**Core API** (`vowp-core/src/session/remote.rs`):

```rust
pub struct RemoteTurn { pub grant: TurnGrant, pub spool_dir: PathBuf }
pub fn begin_turn(deps: &Deps, store: &Store, req: &BeginTurn) -> Result<TurnGrant, CoreError>;
pub fn ingest_lines(deps: &Deps, store: &Store, grant: &IngestGrant, reader: &mut dyn BufRead,
                    ack: &mut dyn Write) -> Result<IngestReport, CoreError>;
pub struct BeginTurn { pub session: SessionId, pub lease: String, pub executor: ExecutorRef,
                       pub purpose: RunPurpose, pub node: NodeName, pub prompt: String }
pub struct IngestGrant { pub session: SessionId, pub lease: String, pub run: RunId }
pub struct IngestReport { pub events: u64, pub duplicates: u64, pub done: bool }
```

`begin_turn` is the only place that writes the framing events for a remote turn; `ingest_lines`
writes nothing outside the session log, the session index, the project run row and the ledger.

`ingest` assigns `seq` from the home log and uses a supplied event id, so it needs one store method
beyond v0.1:

```rust
impl SessionLog {
    /// Like `append`, but the caller supplies the event id (remote ingest). Applies the same
    /// blob-spill, seq and tombstone rules; the caller has already checked `session_events.id`.
    pub fn append_with_id(&self, ev: NewEvent, id: EventId) -> Result<AppendedEvent, StoreError>;
}
```

### 7.3 Surfaces, leases and takeover

**Tables** are in §3.2. One lease per session. A surface registers once and heartbeats every 5 s.

```rust
impl Store {
    pub fn upsert_surface(&self, s: &Surface) -> Result<(), StoreError>;
    pub fn surface(&self, id: &SurfaceId) -> Result<Option<Surface>, StoreError>;
    pub fn delete_surface(&self, id: &SurfaceId) -> Result<bool, StoreError>;
    pub fn lease(&self, session: &SessionId) -> Result<Option<Lease>, StoreError>;
    pub fn take_lease(&self, l: &Lease) -> Result<Option<Lease>, StoreError>;   // atomic; returns the previous holder
    pub fn heartbeat_lease(&self, session: &SessionId, token: &str, at: Timestamp) -> Result<bool, StoreError>;
    pub fn release_lease(&self, session: &SessionId, token: &str) -> Result<bool, StoreError>;
    pub fn stale_leases(&self, before: Timestamp) -> Result<Vec<Lease>, StoreError>;
    pub fn list_leases(&self) -> Result<Vec<Lease>, StoreError>;
}
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceId(pub String);          // ULID, 26 chars
impl SurfaceId { pub fn new() -> Self; }
impl std::fmt::Display for SurfaceId { /* writes the inner string */ }

/// Two fresh ULIDs concatenated = 52 chars, 160 random bits. Never logged.
pub fn new_lease_token() -> String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind { Chat, Opencode, ClaudeCode, Mcp, Mirror, Api }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Surface {
    pub id: SurfaceId, pub kind: SurfaceKind, pub node: NodeName, pub holder: String,
    pub app: Option<String>, pub cwd: Option<String>, pub metadata: serde_json::Value,
    pub created_at: Timestamp, pub last_seen_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Lease {
    pub session_id: SessionId, pub surface_id: Option<SurfaceId>, pub surface_kind: SurfaceKind,
    pub node: NodeName, pub holder: String, pub token: String,
    pub acquired_at: Timestamp, pub heartbeat_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceRef {
    pub surface_id: SurfaceId,
    pub kind: SurfaceKind,
    pub holder: String,
    pub node: NodeName,
    pub acquired_at: Timestamp,
    pub heartbeat_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LeaseStatus {
    pub held: bool,
    pub holder: Option<String>,
    pub kind: Option<SurfaceKind>,
    pub node: Option<NodeName>,
    pub acquired_at: Option<Timestamp>,
    pub heartbeat_at: Option<Timestamp>,
    pub token: Option<String>,      // Some only in the response to --acquire
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TakeoverReport {
    pub previous: Option<String>,
    pub drained: bool,
    pub capsule_id: Option<CapsuleId>,
}
```

Rules:

1. **Acquire.** `chat`, an app host or an MCP server asks for the lease. If none exists, it takes it
   and appends a `note` event `surface <kind> (<holder>) attached` (no token, no path).
   If a lease exists and its heartbeat is older than 60 s, it is stale: the new surface takes it and
   records `stale surface <holder> replaced`. If it is live, the caller asks the owner to take over
   (interactive) or returns `Conflict` with the holder text.
2. **Takeover.** One answer takes it: `take_lease` atomically replaces the row, updates the surface,
   appends a `note` event `taken over by <holder> at <HH:MM>`, and—when the session already has an
   executor or a live run—writes a `switch` row/event with `reason='takeover'` and the target
   executor. A pre-attach session (`sessions.executor IS NULL`, for example a fresh orchestrator
   holding the lease) has no valid `to_executor`, so it gets the note only; `switches.to_executor`
   stays non-null. **The home pushes `IngestAck::LeaseRevoked` on the active ingest channel** when
   the takeover commits; the notice arrives in 99.6–265 ms over live SSH and below 2 s in the
   hermetic poll, so the old surface does not wait for its next heartbeat. `surface::sweep_leases()`
   signals the ingest writer through an in-process channel/condvar, with a ≤250 ms poll as the
   fallback; never poll SQLite per event. The old surface's every write check also
   fails with `Refused("this surface is read-only; taken over by …")`. The old surface prints
   exactly:
   `Taken over by phone at 14:05. This surface is now read-only (follow mode).`
   and keeps reading new events.
3. **Drain.** A turn running on the old surface is drained with the existing v0.1 switch drain: up to
   5 s, then `interrupt`; events already appended stay; a capsule is written; the new surface
   continues from the capsule. The old surface's ingest gets `Error{kind: Refused}` and stops
   appending. The home writes `turn.ended` for the interrupted run via the normal run path/reconcile.
4. **Heartbeat.** Every 5 s. A lease without a heartbeat for 60 s is free and is deleted by
   `surface::sweep_leases` in the node tick. A surface that exits calls `release_lease`.
5. **Check.** `surface::check_lease(deps, store, session)` reads `VOWP_LEASE_TOKEN` (set by `chat`
   and `ingest` from the grant) and compares it. No lease → `Ok(())` (job sessions). A lease exists
   and the token is missing or wrong → `CoreError::Refused`. It is called before every write tool and
   before every interactive turn.

**Core API** (`vowp-core/src/surface/{lease,takeover,follow}.rs`):

```rust
pub struct LeaseGuard { deps: Deps, session: SessionId, token: String, thread: Option<JoinHandle<()>> }
impl LeaseGuard {
    pub fn acquire(deps: &Deps, store: &Store, req: &AcquireLease) -> Result<LeaseGuard, CoreError>;
    pub fn token(&self) -> &str;
    pub fn release(self) -> Result<(), CoreError>;
}
pub struct AcquireLease { pub session: SessionId, pub surface: Surface, pub takeover: bool }
// Defined in `vowp-protocol/src/surface.rs`; core re-exports it, never redefines it.
pub use vowp_protocol::TakeoverReport;
pub fn takeover(deps: &Deps, store: &Store, session: &SessionId, target: &Surface,
                now: Timestamp) -> Result<TakeoverReport, CoreError>;
pub fn sweep_leases(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError>;
pub fn check_lease(deps: &Deps, store: &Store, session: &SessionId) -> Result<(), CoreError>;
/// Explicit-token variant for `vowp mcp` and `session ingest`; `None` means "no lease required".
pub fn check_lease_with(store: &Store, session: &SessionId, token: Option<&str>) -> Result<(), CoreError>;
pub fn follow(store: &Store, session: &SessionId, after_seq: u64, cancel: &CancelToken) -> Result<u64, CoreError>;
```

`surface/takeover.rs` calls the existing `SessionService::switch` with
`SwitchReason::Takeover` (added additively to the enum) when the old surface has a live run.

### 7.4 `vowp chat` (fallback surface)

```rust
pub struct ChatOptions {
    pub session: SessionId,
    pub executor: Option<ExecutorRef>,
    pub node: Option<NodeName>,       // executor node; default: this surface's machine
    pub follow: bool,
    pub takeover: bool,
}
pub fn run(ctx: &mut Ctx, io: &mut dyn IoStreams, opts: &ChatOptions) -> Result<i32, CliError>;
```

- On start: acquire the lease (or ask "Active on laptop since 13:02. Take over? [y/N]"; non-TTY
  defaults to no and exits 4). On home-local it opens the session log through `SessionService`; on a
  control machine it uses `turn-begin`/`__exec`/`ingest` over SSH (§7.2) and follows the home log in
  a second stream (or the same SSH connection with ControlMaster) so the user sees events as they
  are appended. The executor stream and the display stream are separate; the home remains the only
  writer.
- Slash commands: `/switch <exec> [--node n]`, `/status`, `/takeover`, `/follow`, `/capsule`,
  `/quit`. Each maps to the same core path as the CLI command.
- Terminal rules: no colour required, width-aware wrapping at 80 columns, no wide tables. Reads
  turns until EOF (scriptable). Exit 6 on an unswitched limit, with the hint to `/switch`.
- Follow mode: prints `#seq type summary` lines from the current `last_seq`, refuses every write with
  the takeover notice, and exits 0 on Ctrl-C.

### 7.5 App surfaces: opencode, Claude, mirror and resume

Three layers, exactly as plan §7.4:

1. **Same app, several devices = one conversation (the app's feature).** The orchestrator runs in
   an opencode server on the home; the desktop joins through a VOWP tunnel and `opencode pair`; the
   phone joins with an SSH app running `opencode --server …` against the same server. Claude Code
   runs on the home with Remote Control where a claude.ai login exists. No public port.
2. **VOWP tools inside the app:** `vowp mcp` (stdio) with the tools in §7.7.
3. **Changing app/harness/model = VOWP handoff:** the app transcript is mirrored into the session
   log; `session_resume` returns the capsule + last turns.

**Transcript mirror.** `surface/mirror.rs` consumes opencode's `/api/event` SSE (not the
`/session` web page and not the session-log endpoint) and backfills from
`/api/experimental/session/<id>/export` or `opencode session export`; Claude uses the transcript
importer. The cursor is stored in `~/.vowp/orchestrators/<name>/mirror.json`:

```rust
pub struct MirrorCursor { pub last_native_id: Option<String>, pub updated_at: Timestamp }
pub struct MirrorReport { pub events: u64, pub duplicates: u64, pub cursor: MirrorCursor }
pub fn mirror_once(deps: &Deps, store: &Store, session: &SessionId, app: &AppHandle,
                   cursor: &mut MirrorCursor) -> Result<MirrorReport, CoreError>;
```

Mapping (the table verified against opencode v2.0.28): `session.inbox.enqueued` with `item.type=user` → `user.message`;
`session.text.ended` (the full text; `text.started`/`text.delta` are UI-only) →
`assistant.message`, coalesced by `assistantMessageID`; `session.tool.called` → `tool.call`,
`session.tool.success`/`session.tool.failed` → `tool.result`; `session.usage.updated` → `cost`
(tokens and USD; no durable seq, so never use it for ordering); `session.execution.started` →
`turn.started`; `session.execution.succeeded`/`interrupted` → `turn.ended`;
`session.execution.failed` → `error` then `turn.ended`; `session.step.failed` → `error`;
`session.model.selected` → `switch` only when the executor/model actually changes, else `note`;
`session.instructions.updated` → `note` or nothing; `session.step.*`, `session.tool.input.*`,
`session.inbox.delivered`, `server.connected`, `project.updated` and `mcp.*` are not session
content. Each native id is inserted into `native_events` in the same transaction as the event
index row; a duplicate is skipped. The mirror never writes a second assistant message for a
chunked stream: it accumulates chunks and writes one message at completion, the same v0.1 rule.

**App host trait.** Tests use a fake; no real app or network in the test suite.

```rust
pub trait AppHost: Send + Sync {
    fn kind(&self) -> OrchestratorApp;
    fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError>;
    fn stop(&self, handle: &AppHandle) -> Result<(), CoreError>;
    fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError>;
    fn join_instructions(&self, handle: &AppHandle) -> Result<JoinInstructions, CoreError>;
    fn transcript_since(&self, handle: &AppHandle, cursor: &MirrorCursor)
        -> Result<TranscriptBatch, CoreError>;
}

pub struct HostRequest { pub name: String, pub session: SessionId, pub port: Option<u16>,
                         pub workdir: PathBuf, pub config: PathBuf }
pub struct AppHandle { pub kind: OrchestratorApp, pub pid: u32, pub port: Option<u16>,
                       pub started_at: Timestamp, pub workdir: PathBuf }
pub struct HostHealth { pub ok: bool, pub detail: String }
pub struct TranscriptBatch { pub entries: Vec<TranscriptEntry>, pub cursor: MirrorCursor }
pub struct TranscriptEntry { pub native_id: String, pub kind: TranscriptKind, pub ts: Timestamp,
                             pub text: Option<String>, pub tool: Option<String>,
                             pub input: Option<serde_json::Value>, pub output: Option<String>,
                             pub path: Option<String> }
```

`opencode_host.rs` targets the **opencode v2 contract** (2.0.28): the HTTP API lives
under `/api/...` (`/api/event` SSE with a `server.connected` hello, `/api/mcp?location[directory]=…`,
`/api/session`, `/api/session/<id>/message|prompt`; `/session` is the web app, not the JSON API).
Auth is HTTP **Basic** `opencode:<token>` (a Bearer token is rejected). The preferred host is the
managed service:

```sh
opencode service set port <p>     # private XDG_STATE_HOME
opencode service start            # binds 127.0.0.1, prints the URL
opencode pair                     # single-use link, expires in 5 minutes
```

`opencode pair` discovers `$XDG_STATE_HOME/opencode/service.json`, so a foreground
`opencode serve --hostname 127.0.0.1 --port <p>` host must write a matching `service.json`
(`id`, `version`, `url`, `pid`, `password`) or VOWP manages the pair link itself. The host binds
`127.0.0.1` only; `opencode service set hostname 0.0.0.0` is forbidden. Every host command runs
with a private `XDG_STATE_HOME`/`XDG_DATA_HOME` under the orchestrator directory, never the
owner's global opencode state (a bare CLI attaches to the global background service and can
conflict with another job). The pair link is shown with the
`ssh -L <local>:127.0.0.1:<p> <home>` command, treated as a secret, and never logged. The
managed service's default location is `$HOME`, so `orchestrator` must pass
`location.directory = <orchestrator-dir>` on `POST /api/session` (or make the app open that
project). `host.json` (0600) records pid/port plus the service.json path; stop with SIGTERM (or
`opencode service stop`). Health is a successful `/api/event` `server.connected` or an authenticated
`GET /api/session`, not `/global/health`.

`claude_remote.rs` starts Claude Remote Control only when a claude.ai login is present (probe checks
for the login directory, never reads it); otherwise `start` returns
`CoreError::Refused("Claude Remote Control needs a claude.ai login on the home")` and the setup
report says so. Claude Code's project-local `.mcp.json` is project-scoped and needs one-time owner
approval; non-interactive `claude mcp list` reports `Pending approval` until then. Claude Code
sends `server/discover` before `initialize`, so the VOWP MCP server must implement it or return a
valid `method not found` (§7.7).

**opencode MCP exposure is not proven.** opencode v2.0.28 loaded the project-local
`opencode.json`, `opencode mcp list` and `/api/mcp` reported the server `connected`, and the dummy
server received `initialize` + `tools/list`; but the model request's `tools` array contained only
the built-in tools, and the model never called the MCP tool. `connected` is proof of configuration,
not of tool availability. The opencode host must re-test with a real provider and find the permission/config
flag (or an upstream fix) before promising that the opencode orchestrator can call VOWP tools.
Until then the orchestrator's tool path is `vowp chat` or Claude Code, and the opencode surface
still has the mirrored transcript and lease. This is open assumption §16.20.

`session_resume` context (implemented in `surface/mirror.rs`):

```rust
pub struct ResumeContext { pub session: Session, pub capsule: Option<Capsule>,
                           pub tail: Vec<EventEnvelope>, pub lease: Option<Lease> }
pub fn resume_context(deps: &Deps, store: &Store, session: &SessionId, tail_turns: u32)
    -> Result<ResumeContext, CoreError>;
```

### 7.6 Orchestrators

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorApp { Opencode, ClaudeCode, Chat }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorCard {
    pub name: String,
    pub session_id: SessionId,
    pub project: Option<ProjectName>,
    pub app: OrchestratorApp,
    pub state: String,                 // "running" | "stopped" | "degraded"
    pub join: JoinInstructions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JoinInstructions {
    pub desktop: String,
    pub phone: String,
    pub chat: String,
}

pub struct NewOrchestrator {
    pub name: String,
    pub project: Option<ProjectName>,
    pub app: OrchestratorApp,
    pub executor: Option<ExecutorRef>,
}
pub struct OrchestratorInfo {
    pub card: OrchestratorCard,
    pub dir: PathBuf,
    pub session: SessionId,
    pub host: Option<AppHandle>,
}
pub struct Orchestrator { /* deps + store */ }
impl Orchestrator {
    pub fn new(deps: Deps) -> Self;
    pub fn create(&self, req: &NewOrchestrator) -> Result<OrchestratorInfo, CoreError>;
    pub fn list(&self) -> Result<Vec<OrchestratorInfo>, CoreError>;
    pub fn info(&self, name: &str) -> Result<OrchestratorInfo, CoreError>;
    pub fn stop(&self, name: &str) -> Result<(), CoreError>;
    pub fn resume(&self, name: &str) -> Result<OrchestratorInfo, CoreError>;
    /// Called by NodeSupervisor::startup: starts every orchestrator whose state is running.
    pub fn resume_all(&self) -> Result<Vec<OrchestratorInfo>, CoreError>;
}
```

`vowp orchestrator new [--name <n>] [--project <p>] --app opencode|claude-code|chat
[--executor <exec>]`:

1. Creates a project-local directory `~/.vowp/orchestrators/<name>/` (never the user's global app
   config) with:
   - `orchestrator.json` — `{name, app, session_id, project, executor, state, created_at}`
     (0600), where `state` is `running`, `stopped` or `degraded`; `stop` writes `stopped`,
     `resume` writes `running`, and a failed host start writes `degraded`;
   - `opencode.json` for opencode — `{"mcp":{"vowp":{"type":"local","command":["vowp","mcp",
     "--session",<id>,"--surface",<surface>,"--token-file","<dir>/lease.token"],"enabled":true}}}`;
   - `.mcp.json` for Claude Code — `{"mcpServers":{"vowp":{"command":"vowp","args":[ ... ]}}}`;
   - `AGENTS.md` — the instructions template (read the snapshot, report plainly with times, propose
     and ask, apply only after the owner's yes through VOWP tools, never open or edit a database,
     never read secrets, open a tunnel when the owner wants to see something);
   - `lease.token` (0600) written by the lease guard;
   - `host.json` and `mirror.json` when running.
2. Creates the session (role `orchestrator`, project-less or bound), starts the app host, acquires
   the lease, and prints join instructions for the desktop, the phone and `vowp chat`. The
   instructions use generic placeholders (`<home-ssh-target>`, `<session>`); `vowp setup control`
   and `vowp tunnel` on the control machine print the concrete target, so the home never needs to
   know an owner hostname. On opencode, the MCP server may be configured and `connected` but not
   exposed to the model in v2.0.28 (§16.20); the `AGENTS.md` template and join instructions must
   not promise tool calls until the host is re-tested with a real provider, and the tool-capable
   fallbacks are Claude Code (after the owner approves `.mcp.json`) and `vowp chat`.
3. `orchestrator ls` reads every `orchestrator.json` and returns `[OrchestratorCard]`;
   `orchestrator stop <name>` stops the host and releases the lease (the session stays);
   `orchestrator resume <name>` starts the host again. `NodeSupervisor::startup` calls
   `Orchestrator::resume_all`; a host that cannot start is marked `degraded` and does not fail the
   node startup.
4. A migration of the current DeepSeek orchestrator into this session is seeded from the handoff
   file and a capsule; that is an operational step, not new code.

### 7.7 `vowp mcp`

Plain threads, stdio, newline-delimited JSON-RPC 2.0. No async runtime. Invocation:

```
vowp mcp --session <id> --surface <surface-id> --token-file <path>
```

`vowp mcp` uses `NodeCtx` like every command: on the home it reads the DB directly; on a control
machine it routes reads to the home (snapshot/CLI JSON) and writes to the home over SSH. Tools that
need local processes (`tunnel_open`, `tunnel_close`) are available only on a control machine; on the
home they return `Refused("tunnels are opened by the control machine")`. This is the honest limit:
when the app host runs on the home, the orchestrator cannot start a process on the owner's laptop;
the join instructions contain the exact `vowp tunnel open` command and the orchestrator asks the
owner (or `vowp setup control`) to run it. The server reads the lease token from the file on every
write (never caches a stale one), calls `surface::check_lease`, and records the actor as
`session:<id>/surface:<surface-id>` in the ledger detail. Read tools work in follow mode; write
tools refuse with the takeover notice.

| Tool | Kind | Input | Output |
|---|---|---|---|
| `fleet_status` | read | `{project?}` | `FleetStatus` (snapshot wrapper) |
| `fleet_queue` | read | `{project?}` | `[FleetQueueRow]` |
| `inbox_list` | read | `{project?, all?}` | `[InboxItem]` |
| `capacity_report` | read | `{}` | `CapacityReport` |
| `session_tail` | read | `{session, after_seq?, limit?}` | `[EventEnvelope]` (redacted already) |
| `inbox_answer` | write | `{id, answer}` | `InboxItem` |
| `job_add` | write | `NewJob` JSON | `Job` |
| `job_pause` | write | `{job, reason?}` | `Job` |
| `job_retry` | write | `{job}` | `Job` |
| `capacity_set` | write | `{target, mode?, override?, reason?}` | `CapacityTarget` |
| `tunnel_open` | write | `{name, remote_port, local_port?}` | `Tunnel` |
| `tunnel_close` | write | `{name}` | `{closed: bool}` |
| `secret_request` | write | `{name}` | `SecretRequest` (URL, never the value) |
| `session_handoff` | write | `{session, reason?}` | `{capsule_id, at_seq}` |
| `session_resume` | write | `{session, surface?, takeover?}` | `{session, capsule, tail, lease}` |

`initialize`, `notifications/initialized`, `tools/list`, `tools/call` and `ping` are implemented;
unknown methods return JSON-RPC `-32601`. `server/discover` is answered with a valid empty
capability or `-32601`; Claude Code tolerates the error. A tool error is
`{"isError":true,"content":[{"type":"text","text":"<kind>: <message>"}]}`. `secret_request` returns
a one-time URL and expiry; it never returns a value. `session_resume` with a live lease and
`takeover=false` returns the holder notice; with `takeover=true` it calls `surface::takeover`.

**Availability caveat (open).** A server can be loaded and report `connected` while the opencode
v2.0.28 model request does not include its tools (a test MCP server was never in the `tools` array
and was never called). `vowp mcp` therefore implements and tests the protocol, and the config/host
writes the project-local server entry, but v0.2 does not promise that an opencode orchestrator can
call those tools until the host is re-tested with a real provider (open assumption §16.20). Claude
Code's path is proven after the one-time `.mcp.json` approval; `vowp chat` is the tool-free
fallback.

### 7.8 What a surface may write

| Surface | Read | Write | Lease |
|---|---|---|---|
| `vowp chat` | yes | turn input, slash commands | required |
| opencode app via MCP | yes | MCP write tools only | required; write tools check each call |
| Claude app via MCP | yes | MCP write tools only | same |
| `vowp fleet/status/...` CLI | yes | fleet commands go to the home | no session lease |
| `vowp session ingest` | yes | event append for one run | token in the grant |
| `vowp session tail --follow` | yes | no | not required |

---

## 8. Control machine

### 8.1 Layout and config

`~/.vowp-control/` is described in §2.2. `control.json` (0600, serde JSON):

```json
{
  "role": "control",
  "home": "vowp-home",
  "ssh_target": "vowp@home.example",
  "remote_home": "/home/vowp/.vowp",
  "created_at": "2026-11-10T12:00:00.000Z"
}
```

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlConfig {
    pub role: String,              // "control"
    pub home: String,              // node name of the home
    pub ssh_target: String,
    pub remote_home: String,       // absolute path on the home, never "~"
    pub created_at: Timestamp,
}
```

`vowp control init [--name <home-name>] <ssh-target>` probes `$HOME` on the home and stores the
absolute remote home path (never `~`, which the quoted ssh argv would not expand). It writes
`control.json` (refusing to overwrite without `--force`), creates `spool/` and `surfaces.json`,
probes `ssh -o BatchMode=yes <target> vowp --version` (a failure is a warning unless `--strict`),
and prints the next step (`vowp setup control` if this is a fresh machine). `vowp control status`
prints `ControlConfig` and the cache metadata without requiring SSH. `VOWP_CONTROL_HOME` overrides
the directory in tests.

### 8.2 Routing

`NodeCtx::discover` precedence:

1. `--home <dir>` → **home** role at that directory.
2. `VOWP_CONTROL_HOME` (or `~/.vowp-control/control.json` when it exists) → **control** role.
3. `VOWP_HOME` (or `~/.vowp/vowp.db` when it exists) → **home** role.
4. otherwise **uninitialised**: project/fleet commands fail with `NotFound("run vowp setup")`.

On a control machine:

- every project/fleet/capacity/orchestrator command defaults to the home: it runs
  `ssh -o BatchMode=yes <target> vowp --home <remote_home> <args> --json`, parses the JSON and
  renders it locally; the remote exit code is returned unchanged (255 → exit 7);
- `vowp up`, `vowp down` and `vowp board` (any mode) refuse (exit 5) because they need local truth;
- `vowp status` / `fleet status` read the cache first and print `as of HH:MM` if the home is
  unreachable; `--refresh` requires SSH;
- writes (`project add/pause/...`, `inbox answer`, `capacity set`, `job add/pause/retry`) always
  go to the home; a write never succeeds against the cache. A `<path>` given to `project add` is a
  path **on the home**, not on the control machine.

```rust
pub enum NodeRole { Home, Control, Uninitialised }
pub struct NodeCtx { role: NodeRole, paths: NodePaths, control: Option<ControlHome>,
                     target: Option<RemoteHome> }
pub struct ControlHome { root: PathBuf, config: ControlConfig }
pub struct RemoteHome { name: String, ssh_target: String, remote_home: String }
pub struct CachedSnapshot { pub snapshot: Snapshot, pub fetched_at: Timestamp, pub source: String }
impl NodeCtx {
    pub fn discover(global: &Global) -> Result<NodeCtx, CliError>;
    pub fn require_home(&self) -> Result<&NodePaths, CliError>;
    pub fn require_control(&self) -> Result<&ControlHome, CliError>;
    pub fn remote(&self) -> Option<&RemoteHome>;
    pub fn cache_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError>;
    pub fn cached_snapshot(&self) -> Result<Option<CachedSnapshot>, CliError>;
    pub fn route_json(&self, args: &[String]) -> Result<String, CliError>;
}
```

`Ctx::build` uses `NodeCtx`: a home keeps the v0.1 behaviour; a control machine builds a
**remote** context (no local store) and a command that needs a store routes instead. The local
`Ctx::store()` opens the home DB on the home through `Deps::open_store()` (the fresh-home/`_node`
fallback in §3.8), so `reindex` still works after `vowp.db` is deleted. Fleet commands (`status`,
`board`, `fleet`, `capacity`, `gc`, `backup`) use a **fleet context** that loads only the node file
and opens `Store::open_global`; they do not require a `vowp.toml` in the cwd. Project commands open
`Store::open_with_project` for the selected project after `project add`/`init` created its DB.
`vowp init`/`project add` start on the global handle and auto-bind the new project as described in
§3.7.

### 8.3 Tunnels

```
vowp tunnel open <name> --remote-port <n> [--local-port <n>] [--host 127.0.0.1]
vowp tunnel ls [--json]
vowp tunnel close <name>
```

- Spawns `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15
  -L 127.0.0.1:<local>:<host>:<remote> <ssh_target>` in its own process group; records
  `{name, local_port, remote_port, host, pid, started_at}` in `tunnels.json` (0600).
- `--local-port` default: a free port found by binding `127.0.0.1:0`; the chosen port is printed.
- Refuses a port `< 1024` without `--allow-privileged` (exit 2). Refuses a duplicate name (exit 4).
- `tunnel ls` removes a record whose pid is dead and shows age; `close` sends SIGTERM to the
  process group and removes the record. `vowp tunnel close --all` is not provided; the owner closes
  what they opened.
- The orchestrator opens a tunnel for the owner (MCP tool `tunnel_open`); ports and pids are local
  to the control machine and never written to the snapshot or the session log. On a home,
  `vowp tunnel` exits 5: a tunnel is opened from the machine that will use it.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tunnel {
    pub name: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub host: String,              // "127.0.0.1" by default
    pub pid: u32,
    pub started_at: Timestamp,
}
```

### 8.4 Phone

The phone needs an SSH app and the owner's key. It runs `vowp chat` on the home over SSH; no VOWP
install, no browser, no public port. The docs guide (`docs/guides/phone.md`) covers the phone
setup; the behaviour is fixed here: the chat surface detects a narrow terminal, disables colour,
wraps at 80 columns, and supports `/takeover` and `/follow`.

---

## 9. Secrets

### 9.1 Modes and storage

| Mode | When | Storage | Owner |
|---|---|---|---|
| **vault user** | the `vowp-vault` Unix user exists and the helper is allowed by sudoers | `/home/vowp-vault/secrets/<NAME>` mode 0600 | `vowp-vault` |
| **basic** | no sudo or no vault user (common) | `~/.vowp/secrets/<NAME>` mode 0600 | `vowp` |

`~/.vowp/secrets/mode` contains `vault` when vault mode is active (0600); basic mode is the default
when the file is absent, so the no-sudo path writes no marker. Only `vowp secret upgrade` prints the
mode line (`secrets: vault user`, or `secrets: vault user (switched from basic mode)` after a
switch); `vowp doctor` and the board print no secrets-mode line. The honest limit is in the public
docs:
*any agent that can run arbitrary code in a process that holds a secret can print it; this design
keeps secrets out of files the agent can read, out of transcripts and out of git, but it cannot make
a secret unreadable to code that is given it.*

### 9.2 Commands

```
vowp secret set <NAME>              # hidden prompt; never an argument, never echoed
vowp secret ls [--json]             # name + last4 only
vowp secret rm <NAME> [--yes]
vowp secret upgrade                 # basic -> vault user; prints the one sudo command
vowp secret request <NAME>          # one-time 127.0.0.1 paste page; URL + expiry
```

- `set` validates the name (`^[A-Z][A-Z0-9_]{0,63}$`), reads the value from a hidden prompt (no
  TTY → exit 5 with "run it in a terminal or use `vowp secret request`"; echo is disabled with
  `stty -echo` and restored on every exit path, so no new dependency is needed), writes it atomically
  (tmp + fsync + rename, 0600), and upserts `secret_index(name,last4)`. The value is never logged,
  never in argv, never in an event.
- `ls` prints `NAME LAST4 UPDATED`; `--json` `[SecretInfo]`.
- `rm` deletes the file and the index row; `--yes` required.
- `upgrade`: if basic, prints the exact sudo command that creates the vault user and moves the
  files (a one-line helper invocation); after the owner runs it, re-running `upgrade` switches
  `mode` and re-checks. If sudo is unavailable, it prints why and exits 5 (nothing destructive).
- `request`: binds `127.0.0.1:0`, generates a 32-byte random token from `/dev/urandom` (tests inject
  a fixed token), prints
  `http://127.0.0.1:<port>/s/<token>` and an expiry (10 minutes), accepts one POST with the value,
  writes it through the same `set` path, returns a page that only says `saved` and the last4, and
  closes. The AI sees only `DEEPSEEK_API_KEY saved, ends in …4f2a`. The page is reachable through a
  tunnel for a desktop owner; the phone uses `secret set`. No TLS: the listener is loopback-only and
  the tunnel is SSH. When the request is issued from a control machine, the MCP/CLI wrapper opens
  (or reuses) a local tunnel and returns the **local** URL, so the owner's browser never needs the
  home's port number; the home's raw URL is never printed to the owner.

### 9.3 Injection and redaction

- **Secrets are home state.** On a control machine, `vowp secret set/rm/upgrade/ls` route to the
  home over SSH. `set` uses `ssh -t` so the home's hidden prompt has a TTY; the value crosses the
  encrypted SSH channel and is never in argv. `secret request` opens the local tunnel as described
  in §9.2. On the home, `vowp secret upgrade` prints `secrets: vault user` when the vault is active.
- **Injection.** A harness run's `env_pass` is a list of **names**. `vault::env_for` resolves exactly
  those names for that harness (plus names listed by `[env.*]` deploy/promote commands) and returns
  `(name, value)` pairs to the adapter; a missing name is a warning, not a hard failure (unless the
  config marks it required in v0.3). `vowp __vault-exec` is the hidden read/inject helper:
  `vowp __vault-exec --env NAME -- <cmd…>` reads only `NAME` and execs the command; the `sudo -u
  vowp-vault` wrapper in the sudoers line chooses the user, so the helper has no `--user` flag.
  In basic mode it is a plain exec with the same argv. The same root-owned helper has the
  **only write path** in vault mode: `vowp __vault-admin set <NAME>` reads the value from stdin
  (never argv), validates the name and writes the file 0600 as `vowp-vault`; `vowp __vault-admin rm
  <NAME>` unlinks it. The one sudoers line allows only these fixed subcommands, never an arbitrary
  command. A root-owned helper plus one sudoers line (`vowp ALL=(vowp-vault) NOPASSWD: <helper>`) is
  the vault mode; without it, basic.
- **Redaction.** `vault::Redactor` loads the exact values (longest first) and replaces every exact
  occurrence with `[REDACTED:<NAME>]`. It is applied:
  - to every event payload and `raw` before `SessionLog::append` (`vault::redact_event` in
    `append_event`/`append_harness`);
  - to a capsule body after the capsule writer returns and before the caller sees it
    (`redact_capsule`, which rewrites the file and updates `capsules.body_json`; the capsule draft
    is normally already redacted because `CapsuleWriter` reads the last assistant text from the
    redacted log, and this hook is the final guard for the `result_text` fallback);
  - to job artifacts after each turn (`redact_job_artifacts`: `report.md` and the run raw mirror);
  - to the serialized snapshot before writing (`fleet::write_snapshot`);
  - to gate evidence and ledger `detail_json` before insert.
  The redactor never logs a value or its prefix; it logs only the name. The existing secret-scan
  gate stays and runs on the public tree.

### 9.4 Types

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretName(pub String);          // validated

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecretInfo { pub name: SecretName, pub last4: String,
                        pub created_at: Timestamp, pub updated_at: Timestamp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VaultMode { VaultUser, Basic }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecretRequest { pub name: SecretName, pub url: String,
                           pub expires_at: Timestamp, pub single_use: bool }

pub struct SecretRequestServer { /* listener, token, expiry */ }
impl SecretRequestServer {
    /// Binds 127.0.0.1:0, serves one POST and exits. Tests inject a fixed token and clock.
    pub fn start(deps: &Deps, name: &SecretName, now: Timestamp) -> Result<SecretRequest, CoreError>;
    pub fn wait(self, timeout: std::time::Duration) -> Result<SecretInfo, CoreError>;
}
```

---

## 10. Lifecycle, GC and backup

### 10.1 Classes

| Class | Examples | Default | Derived from |
|---|---|---|---|
| **production** | releases, rollback snapshots, serving banks, `releases.log`, prod pointers | never auto-deleted | `releases` rows, `[env.*]` paths, catalog |
| **truth** | global + project DBs, capsules, passing gate evidence, decisions/ledger | never auto-deleted; backed up | DB files, `capsules/`, `gate_results` |
| **open_work** | worktrees of unmerged/unpushed jobs, active/open sessions, orchestrator sessions | never auto-deleted | `jobs` state/worktree/branch, `sessions` state, `orchestrators/` |
| **secrets** | `secrets/`, vault files | never auto-deleted, never backed up in plain text | secrets dir, mode file |
| **closed_session** | full `log.jsonl` + blobs of a session whose job is done | kept 30 days after close, then compacted to capsule + summary | `sessions.state='closed'` or job `done` and `updated_at` |
| **run_output** | `runs/*.jsonl`, `*.stderr` | 14 days; gzip after 1 day | `runs/` mtime |
| **worktree** | merged jobs | removed after merge (v0.1); failed/cancelled kept 7 days | job state, worktree path |
| **sample** | `host_samples` | 24 h full rate, 30 days as 1-min averages | table rows |
| **cache** | control snapshot cache, `tmp/` | disposable | control dirs, `tmp/` |

Classes for the big objects are **derived from DB state**, so no registration hook is needed for
runs, sessions, worktrees or samples. The `catalog` adds rows for files that are not derivable
(backups, release banks, archived project dirs) and for `vowp gc` reporting. `vowp gc` merges both
sources and de-duplicates by path.

### 10.2 `vowp gc`

```
vowp gc [--apply] [--class <class>] [--json] [--now <RFC3339>]   # --now for tests only
```

- Dry run by default: lists files per class, bytes, and a 5-path sample; changes nothing.
- `--apply`: deletes only `closed_session` (after compaction), `run_output`, `worktree` (per rule),
  `sample` and `cache` entries whose `expires_at` is in the past. It never deletes production,
  truth, open work or secrets. A property test enumerates every protected path before/after and
  asserts it still exists.
- `closed_session` compaction: write one final capsule (`trigger='job_end'` if none), write
  `sessions/<id>/summary.md` (capsule markdown + first/last turn times + event count), set
  `sessions.compacted_at`, keep the `sessions`, `session_events`, `capsules` rows (the index is the
  history), delete `log.jsonl`, `blobs/` and `.lock`. `reindex` skips a session with no log and
  reports it, never deletes the row. `session tail` and `session show --events` on a compacted
  session exit 3 with `session compacted to capsule <id>; use 'session show --capsule'`.
  `session show --capsule`, `session resume` and `session export` keep working: export writes the
  capsule with an empty event tail and says so in the bundle. There is no full-log replay after
  compaction; that is the point of the class.
- `worktree`: a merged job's worktree is removed after merge (v0.1 behaviour). A failed or cancelled
  job's worktree is kept 7 days after its terminal state, then removed; the branch is kept.
- `run_output`: delete after 14 days; gzip `.jsonl`/`.stderr` after 1 day (gzip in place, keep the
  name + `.gz` and a catalog row).
- `sample`: delete 5 s rows older than 24 h; replace minute rows older than 30 days.
- Nightly: lifecycle GC creates an L9 schedule `gc-nightly` with `every 24h`, action
  `command ["vowp","gc","--apply"]`, bound to the project; the node supervisor runs it once per day.
- `--json` `GcReport`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcReport {
    pub generated_at: Timestamp,
    pub dry_run: bool,
    pub classes: Vec<GcClassReport>,
    pub deleted_files: u64,
    pub freed_bytes: u64,
    pub compacted_sessions: u64,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcClassReport { pub class: CatalogClass, pub files: u64, pub bytes: u64,
                           pub sample_paths: Vec<String> }
```

```rust
#[derive(Debug, Clone, Default)]
pub struct GcOptions { pub apply: bool, pub class: Option<CatalogClass>, pub now: Option<Timestamp> }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport { pub session: Option<SessionId>, pub bytes_freed: u64,
                           pub capsule_id: Option<CapsuleId>, pub skipped: bool }

#[derive(Debug, Clone, Default)]
pub struct BackupOptions { pub to: Option<String>, pub offsite: Option<String>,
                           pub encrypt: Option<String>,
                           pub restore: Option<std::path::PathBuf>,
                           pub to_home: Option<std::path::PathBuf>, pub force: bool }

#[derive(Debug, Clone, Default)]
pub struct CatalogFilter { pub class: Option<CatalogClass>, pub project: Option<ProjectName>,
                           pub expired_before: Option<Timestamp>, pub limit: Option<u32> }
```

### 10.3 `vowp backup`

```
vowp backup [--to <path|rsync-target|git-remote>] [--offsite <target>] [--encrypt <age-recipient>] [--json]
vowp backup --restore <dir> --to-home <dir> [--force]
```

- Backs up the global DB, every project DB, the session content (`sessions/<id>/log.jsonl`,
  `blobs/`, `capsules/`, `adapters.json`, `summary.md`) and the catalog into `backups/<UTC>/`;
  `manifest.json` has sha256 and row counts. Rotation 7 daily + 4 weekly. Without the session
  content a restore would keep the index rows but lose every conversation, so it is never optional.
- `--to` keeps the v0.1 file form: a path ending in `.db` is a single global-DB copy. A path ending
  in `/` (or an existing directory) is a full backup directory; `user@host:path` and
  `rsync://…` are offsite; `git:<remote>` pushes the backup directory. `--offsite <target>` is an
  explicit alias for the last two. An offsite copy uses `rsync`/`git` argv (no shell); a private
  GitHub repo is recommended, never required.
- `--encrypt` runs the external `age` binary; gzip uses the external `gzip`; offsite copy uses the
  external `rsync`/`git`. No new Rust dependency is added for these. A missing binary is a clear
  exit 5 with the command it needs, never a silent unencrypted copy.
- The legacy single-file form (`--to *.db`) still writes the **global** DB only, including
  `projects`. Opening it with `Store::open_path` gives an unbound global handle;
  `Store::schema_version()` reports the compatibility 1 while `global_schema_version()` reports 2,
  and `list_projects()` returns the registered projects. This is what keeps
  `crates/vowp-cli/tests/maint.rs::backup_creates_openable_db` proves this.
- `secrets/` is never backed up in plain text; the manifest says it was skipped, and `--encrypt`
  does not change that rule (a secret is restored by `vowp secret set`, not from a backup).
- Restore writes a fresh home, runs `integrity_check` and the migration row-count checks, and
  refuses to overwrite without `--force`. The acceptance test #6 includes a restore drill.

### 10.4 Types

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatalogClass { Production, Truth, OpenWork, Secrets, ClosedSession, RunOutput, Worktree, Sample, Cache }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogEntry {
    pub id: String, pub path: String, pub class: CatalogClass,
    pub project: Option<ProjectName>, pub environment: String,
    pub job_id: Option<JobId>, pub session_id: Option<SessionId>,
    pub bytes: Option<u64>, pub expires_at: Option<Timestamp>,
    pub note: String, pub created_at: Timestamp, pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackupReport {
    pub at: Timestamp, pub directory: String, pub files: Vec<BackupFile>,
    pub bytes: u64, pub offsite: Option<String>, pub encrypted: bool, pub skipped: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackupFile { pub path: String, pub bytes: u64, pub kind: String }
```

---

## 11. AI-first setup

### 11.1 The README prompt contract

The README's first section is one copyable prompt, version-pinned:

```text
Set up VOWP <version> for me. Read
https://raw.githubusercontent.com/syver777/vowp/v<version>/docs/setup/agent-setup.md
and follow it exactly. Ask me only what it tells you to ask, show me the full plan once,
and run it after I approve. Never ask me to paste a secret into this chat.
```

`docs/setup/agent-setup.md` is the contract every harness follows: ask once; check without
changing; show the whole plan once; run `vowp setup home --target <ssh-target> --apply`, then
`vowp setup control --apply`; stop for the human hand-offs; `vowp setup verify`; report. The commands
do the real work, so the result does not depend on which harness runs the prompt.

### 11.2 Commands

```
vowp setup plan --target <ssh-target> [--json]
vowp setup home --target <ssh-target> --apply [--json] [--yes]
vowp setup control --apply [--json]
vowp setup verify [--json]
```

- `plan` prints every phase, every command (argv, no shell interpolation) and every file it will
  touch, as text and JSON. **It changes nothing** (no writes, no sudo, no network beyond read-only
  SSH probes). The probes check the OS and architecture, the login user and home, free disk, the
  tools (`git`, `tmux`, `curl`, `systemctl`, `loginctl`), the harness binaries and whether the
  chosen harnesses appear signed in, an existing `vowp`, and the sudo mode. It prints only the
  plan; a failed requirement becomes a hand-off. A missing `vowp` on the remote is a phase
  ("install"), not an error.
- `home --apply` is idempotent. Phases:
  1. **preflight** — SSH key login, OS/arch, disk, `git`, `tmux`, `curl`; never reads a secret.
  2. **user** — create the non-root `vowp` user if sudo is available; otherwise run as the current
     user and record the no-sudo mode.
  3. **install** — `scripts/install.sh --download --no-service --no-cron --no-watchdog` (curl stays
     the primitive), verify the version.
  4. **home** — `~/.vowp/`, global DB, node registry, `vowp doctor`.
  5. **service** — user systemd unit `vowp.service`, linger on, start.
  6. **vault** — vault user + helper when sudo allows, else basic mode; never a secret value.
  7. **projects** — register the repositories the owner named (each must already have `vowp.toml`;
     `vowp init` is offered, not forced).
  8. **tunnels** — none by default; the screenshot/browser case is a later owner request.
  A required hand-off (sudo password, harness login, secret value) stops at that phase and exits 5
  with `{"status":"handoff","phase":"…","handoff":{…}}`. Re-running continues from the same phase.
- `control --apply` writes `~/.vowp-control/` (`control init`), the snapshot cache and the
  tunnel record; it does not install a supervisor.
- `verify` runs a fixture project on the home: a job is added, a fake/stock harness runs one turn,
  gates pass, the merge queue merges, `snapshot.json` is fetched from the control machine with an
  `as of` time, and an orchestrator session answers through `vowp chat`. It prints each check with
  `ok|warn|fail` and exits 1 on a fail, 0 otherwise.

### 11.3 Types (CLI-local `Serialize`)

```rust
pub struct SetupPlan { pub target: String, pub version: String, pub phases: Vec<SetupPhase>,
                       pub handoffs: Vec<SetupHandoff> }
pub struct SetupPhase { pub id: String, pub title: String, pub commands: Vec<Vec<String>>,
                        pub files: Vec<String>, pub requires: Vec<String>, pub idempotent: bool }
pub struct SetupHandoff { pub phase: String, pub kind: String, pub prompt: String }
pub struct SetupOutcome { pub status: String, pub phase: String, pub changed: Vec<String>,
                          pub handoff: Option<SetupHandoff>, pub checks: Vec<SetupCheck> }
pub struct SetupCheck { pub name: String, pub status: String, pub detail: String }
```

The plan never contains a secret value, a token or a private key. `requires` entries are
`sudo`, `network`, `login:<harness>`, `secret:<NAME>` (name only) and `owner-answer:<question>`.

---

## 12. CLI: every new command, arguments and `--json` shape

Global flags, exit codes, id prefixes and error JSON are unchanged from `cli.md` §1. New commands
follow the same rules: `--json` prints one document (lists print arrays), errors are
`{"error":{"kind","message","hint?}}` and exit codes are the v0.1 table. A command that cannot run
exits **1** with `not implemented in this build`; no release ships a stub, and the
`no_command_returns_not_implemented` test covers it. `CliError::NotImplemented` is already mapped
to `ErrorKind::Internal`/exit 1 by `crates/vowp-cli/src/errors.rs`, and
`crates/vowp-cli/tests/core.rs` asserts that; this spec follows the code and does not change the
exit-mapping assertion. `V0_2_COMMANDS` in §13.5 is the command list, and `tests/core.rs` asserts
it.

### 12.1 New top-level commands

| Command | Args / subcommands | JSON |
|---|---|---|
| `project` | `add <path> [--name] [--weight]`, `ls [--all]`, `pause <n>`, `resume <n>`, `archive <n>`, `rm <n> --yes`, `weight <n> <w>` | `ProjectEntry` or `[ProjectEntry]` |
| `fleet` | `status [--refresh] [--cached]`, `queue [--project] [--state]`, `inbox [--project] [--all]`, `capacity` | §5.5 |
| `capacity` | `show`, `report [--since] [--limit]`, `set [--target <n>] [--mode auto\|fixed] [--override] [--reason] [--by]` | §6.5 |
| `control` | `init [--name <home-name>] <ssh-target> [--strict] [--force]`, `status` | `ControlConfig` / `FleetStatus` |
| `tunnel` | `open <name> --remote-port <n> [--local-port] [--host] [--allow-privileged]`, `ls`, `close <name>` | `Tunnel` / `[Tunnel]` |
| `secret` | `set <NAME>`, `ls`, `rm <NAME> [--yes]`, `upgrade`, `request <NAME>` | `[SecretInfo]` / `SecretRequest` |
| `gc` | `[--apply] [--class] [--now]` | `GcReport` |
| `setup` | `plan --target <t>`, `home --target <t> --apply`, `control --apply`, `verify` | §11.3 |
| `orchestrator` | `new [--name] [--project] --app <a> [--executor]`, `ls`, `stop <n>`, `resume <n>` | `OrchestratorCard` / `[OrchestratorCard]` |
| `migrate` | `[--dry-run] [--rollback <backup>] [--yes]` | `MigrationReport` |
| `mcp` | `--session <id> --surface <id> --token-file <path>` | stdio MCP, no `--json` |

Hidden commands (not in `--help`): `__exec`, `__vault-exec`, `__vault-admin`,
`__window capacity`, `session turn-begin`, `session ingest`, `session lease`, `session takeover`.

### 12.2 Existing commands that change

| Command | Change |
|---|---|
| `init` | Also creates `~/.vowp/projects/<name>/project.db`; registers through `project add`; still writes framework files. |
| `up` / `down` | Node supervisor; `--no-migrate`; `--once` = one node tick; refuses on control. |
| `status` | Adds `home` and `projects`; keeps v0.1 keys and `--check`. |
| `board` | Adds a project column; `--once` still one screen; `--json` keeps v0.1 keys for one project. |
| `session` | Adds hidden `turn-begin`, `ingest`, `lease`, `takeover`; `chat` uses leases; `tail --follow` reads without a lease. |
| `backup` | New `--to` directory/offsite forms, `--offsite`, `--encrypt`, `--restore`; the old `--to <file>.db` still writes one DB copy. |
| `schema` | Adds `snapshot`; `vowp schema` lists it. |
| `doctor` | Adds `secrets` mode, `projects` DBs, `snapshot` writability, `vowp-home` tmux window, migration state. |
| `loops` | Loop rows are per project. `loops`/`loops pause/resume` use the selected project; with more than one project and no `--project`, exit 2 listing them. `loops --all` lists every project's rows with a project column. Provider `limits` stay global, so L6 state is per project but the limit row it writes is shared. |
| `schedule` | Schedules are per project. `schedule add/ls/pause/resume/rm` use the selected project; `--all` lists every project's rows. A node-wide `[[schedules]]` is not supported in v0.2; the node-level L9 is the `gc-nightly` command schedule in each project. |
| `node` | On control, reads `nodes.json`; on home, unchanged plus `host`-aware rows. `node add <name> -` registers a surface-only executor node (a laptop that dials the home; the home never dials it). |
| `chat` | Changed, not new: v0.1 already has `chat` as an alias of `session chat`. v0.2 adds `--executor/--node/--follow/--takeover` and the lease/remote handler; `chat` stays in `V0_1_COMMANDS`, not `V0_2_COMMANDS`. |

### 12.3 `--json` shapes for the complex commands

**`fleet status`** (wrapper):

```json
{"snapshot": { /* §5.2 */ }, "source": "local|remote|cache", "fetched_at": "2026-11-10T14:02:12.000Z", "stale": false}
```

**`fleet queue`** row:

```json
{"project":"vowp","job":"interfaces","title":"Document the interfaces","kind":"build","role":"worker","priority":99,"state":"ready","age_secs":60,"waiting_reason":"waiting: RAM 1.2 GB short"}
```

**`project ls`** row is `ProjectEntry`; **`project add/pause/resume/archive/weight`** print the
same object. **`project rm`** prints `{"name":"vowp","archive_path":".../archive/projects/vowp-<ts>","removed":true}`.
**`status --json`** adds `projects: [StatusProject]` where
`StatusProject {name, state, running, ready, blocked, paused_reason}` (the reserved `_node` is
excluded); it is defined next to `status.rs` like the other CLI-local summaries (`cli.md` §3 rule 2)
and derives `Serialize, JsonSchema`. **`project rm`** returns
`ProjectRemoved {name, archive_path, removed}`.

**`capacity set`** returns `CapacityTarget`; **`capacity show/report`** return `CapacityReport`.

**`secret request`** returns `SecretRequest` (`name`, `url`, `expires_at`, `single_use`).

**`tunnel open`** returns:

```json
{"name":"opencode","local_port":41234,"remote_port":4096,"host":"127.0.0.1","pid":12345,"started_at":"2026-11-10T14:02:12.000Z"}
```

**`gc`** returns `GcReport`; **`backup`** returns `BackupReport`.

**`migrate`** returns:

```json
{"from_version":1,"to_version":2,"backup":"/home/vowp/.vowp/backups/pre-0.2-20261110T140212Z.db","projects":["vowp"],"tables":{"jobs":{"before":41,"after":41},"ledger":{"before":120,"after":120}},"verified":true,"dry_run":false}
```

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MigrationReport {
    pub from_version: i64,
    pub to_version: i64,
    pub backup: String,
    pub projects: Vec<ProjectName>,
    pub tables: BTreeMap<String, MigrationCount>,
    pub verified: bool,
    pub dry_run: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MigrationCount { pub before: u64, pub after: u64 }
```

**`orchestrator new`** returns `OrchestratorCard` with `join.desktop`, `join.phone`, `join.chat`.

### 12.4 Exit codes for new failures

| Code | Case |
|---|---|
| 2 | bad args/port/name, invalid `--mode`, `--target` missing, plan with an unparsable config, `WrongProject`/`NoProject` (add `--project`) |
| 3 | unknown project/session/secret/tunnel/orchestrator |
| 4 | name conflict, project running, lease held (no takeover), duplicate tunnel, session already switched, `MigrationRequired` (hint `vowp migrate`) |
| 5 | guard/refusal: control machine cannot `up`, no takeover confirmation, `--target` above advice without `--override`, missing `--yes`, hand-off needed, no TTY for `secret set`, setup plan cannot proceed |
| 6 | provider limited on the target of a switch/secret injection |
| 7 | remote/ssh failure on a control machine |

### 12.5 Hidden commands (arguments and JSON lines)

These are implementation interfaces, not help-listed user commands. They still accept `--json`
where a document is printed; the streaming ones print one JSON object per line.

```
vowp __exec --grant <file> [--spool-dir <dir>]
    # starts the adapter on this machine, prints RemoteLine JSON lines, spools unacked lines
vowp __vault-exec --env <NAME> -- <cmd> [args…]
    # the vault-user read/inject helper; in basic mode it is a plain exec
vowp __vault-admin set <NAME>   # reads the value from stdin, writes as the vault user
vowp __vault-admin rm <NAME>    # unlinks as the vault user
vowp session turn-begin <session> --executor <exec> --purpose <p> --node <n>
    --request <file|-> [--json]
    # request = {lease, prompt, executor, purpose, node}; writes framing + run row;
    # prints TurnGrant on stdout (the client captures it, never logs it)
vowp session ingest <session> --run <run-id> [--replay] [--json]
    # reads RemoteLine JSON lines on stdin (Hello carries the lease), prints IngestAck lines
vowp session lease <session> [--json] [--acquire <kind> --holder <h> --node <n>]
    # prints LeaseStatus: {held, holder, kind, node, acquired_at, heartbeat_at, token?}
    # the token is included only on acquire, never on a plain show
vowp session takeover <session> --kind <kind> --holder <h> --node <n> [--json]
    # takes the lease, drains, writes the capsule, prints TakeoverReport
vowp __window capacity
    # renders the capacity screen for the vowp-home tmux window
```

`__exec` requires the grant file to be mode 0600 and refuses a token on argv. `session ingest`
accepts `--replay` only together with a run id; replay itself is idempotent so the flag is a hint,
not a different code path. `session lease --acquire` prints the token once; a plain show prints
`token: null` even to the owner, so a log cannot capture it.

---

## 13. Frozen interfaces and exact hook points

The interfaces below are frozen for v0.2. The module names, types, signatures and hook points are
binding; an implementation may not rename them. A command that is not implemented yet returns
`CliError::NotImplemented("...")` and exit 1; no module fakes a passing test.

### 13.1 Protocol modules

- `vowp-protocol/src/capacity.rs` — all types in §6.1–§6.5 (`HostSample`, `SampleSource`,
  `RunUsage`, `UsageSource`, `UsageEstimate`, `EstimateSource`, `AdmitRequest`, `AdmitDecision`,
  `WaitReason`, `WaitCode`, `BrakeState`, `BrakeReason`, `BrakeCode`, `TargetMode`,
  `CapacityTarget`, `CapacityAdvice`, `AdviceState`, `CapacityReport`, `RunningCapacity`,
  `WaitingJob`, `RemoteUsage`).
- `vowp-protocol/src/surface.rs` — `SurfaceId`, `SurfaceKind`, `Surface`, `Lease`, `SurfaceRef`,
  `LeaseStatus`, `TakeoverReport`, `RemoteEvent`, `RemoteLine`, `IngestAck`, `TurnGrant`,
  `OrchestratorApp`, `OrchestratorCard`, `JoinInstructions`.
- `vowp-protocol/src/snapshot.rs` — `SNAPSHOT_FORMAT`, `SNAPSHOT_VERSION`, `ProjectState`,
  `ProjectEntry`, `Snapshot`, `SupervisorSnapshot`, `ProjectSnapshot`, `RunningJobCard`,
  `BlockedJobCard`, `QueueJobCard`, `MergeCard`, `GateFailureCard`, `InboxSummary`,
  `SessionSurfaceCard`, `CapacitySnapshot`.
- `vowp-protocol/src/catalog.rs` — `CatalogClass`, `CatalogEntry`, `GcReport`, `GcClassReport`,
  `BackupReport`, `BackupFile`, `CatalogFilter`.
- `vowp-protocol/src/vault.rs` — `SecretName`, `SecretInfo`, `VaultMode`, `SecretRequest`.
- `vowp-protocol/src/ops.rs` — control-plane DTOs shared by CLI, MCP and core: `FleetStatus`,
  `FleetQueueRow`, `Tunnel`, `MigrationReport`, `MigrationCount`.
- `vowp-protocol/src/lib.rs` — `pub mod` lines and re-exports.
- `vowp-protocol/src/event.rs` — `SessionRole::Orchestrator` and `SwitchReason::Takeover`
  (additive; update the wire-name tests). Both enums are defined in `event.rs` (lines around 40
  and 714); `session.rs` only re-exports `SessionRole` and imports `SwitchReason`, so do **not**
  edit `session.rs` for these variants.
- `vowp-protocol/src/schema.rs` — add the new types to `all_schemas()`; add `snapshot`.

Core-only helper types have explicit homes: `SamplerHandle` in `capacity/sampler.rs`,
`ScopeSample`/`ProcFs` in `capacity/scope.rs`, `AdviceTrigger` in `capacity/advisor.rs`;
`UsageQuery`/`EstimateKey` are store types in `vowp-store/src/repo_capacity.rs`.

All new **wire** types derive `Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema`; enums
use `#[serde(rename_all = "snake_case")]`; timestamps are `Timestamp`. Internal decision/context
types (`AdmitRequest`, `AdmitDecision`, `SamplerHandle`, `ProcFs`, `RunScope`, `AcquireLease`) derive
only the traits they need; they are not JSON payloads. No new dependency is needed.
This includes every `--json` payload named in §12: `FleetStatus`, `FleetQueueRow`, `CapacityReport`,
`CapacityTarget`, `CapacityAdvice`, `GcReport`, `GcClassReport`, `BackupReport`, `BackupFile`,
`MigrationReport`, `MigrationCount`, `ProjectEntry`, `Tunnel`, `LeaseStatus`, `OrchestratorCard`,
`SecretInfo`, `SecretRequest` and `TakeoverReport` (the last lives in `surface.rs`, not core, so it
can derive the wire traits).

`vowp-protocol/src/lib.rs` gains `pub mod capacity; pub mod catalog; pub mod ops; pub mod snapshot;
pub mod surface; pub mod vault;` and re-exports the common types at the crate root (the CLI and core use
`vowp_protocol::Snapshot` etc.). `vowp-core/src/lib.rs` gains `pub mod capacity; pub mod fleet; pub
mod lifecycle; pub mod surface; pub mod vault;` plus re-exports; `vowp-core/src/session/mod.rs`
gains `pub mod remote;`. These `lib.rs`/`mod.rs` lines are the single declaration point for the
modules.

### 13.2 Core modules

```
vowp-core/src/
  capacity/mod.rs            pub mod {sampler,usage,estimates,scope,admit,brakes,targets,fair,advisor};
                             pub use admit::{CapacityManager, admit}; pub use ...
    sampler.rs               pub fn sample(deps, store, now) -> Result<Option<HostSample>, CoreError>;
                             pub fn sample_host(deps) -> Result<HostSample, CoreError>;
    usage.rs                 pub fn record(store, usage: &RunUsage) -> Result<(), CoreError>;
    estimates.rs             pub fn recompute(store, now) -> Result<u32, CoreError>;
                             pub fn estimate_for(store, req: &AdmitRequest) -> Result<UsageEstimate, CoreError>;
    scope.rs                 pub struct RunScope; pub fn begin(...); impl RunScope { sample, finish }
    admit.rs                 pub fn admit(deps, store, req) -> Result<AdmitDecision, CoreError>;
                             pub fn request_for(deps, store, job: &Job) -> Result<AdmitRequest, CoreError>;
                             pub struct CapacityManager;
    brakes.rs                pub fn evaluate(store, now) -> Result<BrakeState, CoreError>;
    targets.rs               pub fn current(store) -> Result<CapacityTarget, CoreError>;
                             pub fn set(store, mode, target, override_, by, reason, now) -> Result<CapacityTarget, CoreError>;
    fair.rs                  pub fn order(projects: &mut Vec<ProjectEntry>, last: Option<&ProjectName>);
    advisor.rs               pub fn maybe_advise(deps, store, now, trigger) -> Result<Option<CapacityAdvice>, CoreError>;
  surface/mod.rs             pub mod {lease,takeover,follow,orchestrator,native,opencode_host,claude_remote,mirror};
                             pub use lease::{LeaseGuard, check_lease}; pub use takeover::takeover;
                             pub fn sweep_leases(deps, store, now) -> Result<u32, CoreError>;
    lease.rs                 LeaseGuard, AcquireLease, new_lease_token
    takeover.rs              takeover; returns vowp_protocol::TakeoverReport (defined in surface.rs)
    follow.rs                follow
    orchestrator.rs          Orchestrator, NewOrchestrator, OrchestratorInfo
    native.rs                AppHost, HostRequest, AppHandle, HostHealth, TranscriptBatch, TranscriptEntry
    opencode_host.rs         OpencodeHost
    claude_remote.rs         ClaudeRemoteHost
    mirror.rs                MirrorCursor, MirrorReport, mirror_once
  fleet/mod.rs               Snapshot types, build_snapshot, write_snapshot, read_snapshot, node_tick
  lifecycle/mod.rs           pub mod {classes,gc,compact,backup}; pub fn tick(...)
    classes.rs               classify
    gc.rs                    GcOptions, gc
    compact.rs               compact_session
    backup.rs                backup, restore
  vault/mod.rs               env_for, redact_event, redact_text, redact_capsule,
                             redact_job_artifacts, Redactor, mode
    request.rs               request_secret, SecretRequestServer
  session/remote.rs          begin_turn, ingest_lines, BeginTurn, IngestGrant, IngestReport
  session/mod.rs             pub mod remote; re-export ResumeContext
```

The exact signatures:

```rust
// capacity
pub fn sample(deps: &Deps, store: &Store, now: Timestamp) -> Result<Option<HostSample>, CoreError>;
pub fn admit(deps: &Deps, store: &Store, req: &AdmitRequest) -> Result<AdmitDecision, CoreError>;
pub fn request_for(deps: &Deps, store: &Store, job: &Job) -> Result<AdmitRequest, CoreError>;
pub fn recompute(store: &Store, now: Timestamp) -> Result<u32, CoreError>;
pub fn evaluate(store: &Store, now: Timestamp) -> Result<BrakeState, CoreError>;
pub fn current(store: &Store) -> Result<CapacityTarget, CoreError>;

// surface
pub fn sweep_leases(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError>;
pub fn check_lease(deps: &Deps, store: &Store, session: &SessionId) -> Result<(), CoreError>;
/// Explicit-token variant for `vowp mcp` and `session ingest`; `None` means "no lease required".
pub fn check_lease_with(store: &Store, session: &SessionId, token: Option<&str>) -> Result<(), CoreError>;
pub fn takeover(deps: &Deps, store: &Store, session: &SessionId, target: &Surface,
                now: Timestamp) -> Result<TakeoverReport, CoreError>;

// fleet
pub fn build_snapshot(deps: &Deps, store: &Store, now: Timestamp) -> Result<Snapshot, CoreError>;
pub fn write_snapshot(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError>;
pub fn read_snapshot(paths: &NodePaths) -> Result<Snapshot, CoreError>;
pub fn node_tick(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError>;

// lifecycle
pub fn tick(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError>;
pub fn classify(deps: &Deps, store: &Store, now: Timestamp) -> Result<Vec<ClassifiedFile>, CoreError>;
pub fn gc(deps: &Deps, store: &Store, opts: &GcOptions) -> Result<GcReport, CoreError>;
pub fn compact_session(deps: &Deps, store: &Store, session: &SessionId, now: Timestamp)
    -> Result<CompactReport, CoreError>;
pub fn backup(deps: &Deps, store: &Store, opts: &BackupOptions) -> Result<BackupReport, CoreError>;

pub struct ClassifiedFile { pub path: std::path::PathBuf, pub class: CatalogClass,
                           pub project: Option<ProjectName>, pub bytes: u64, pub reason: String }

// vault
pub fn mode(deps: &Deps) -> Result<VaultMode, CoreError>;
pub fn env_for(deps: &Deps, req: &TurnRequest) -> Result<Vec<(String, String)>, CoreError>;
pub fn write_secret(deps: &Deps, name: &SecretName, value: &str) -> Result<SecretInfo, CoreError>;
pub fn remove_secret(deps: &Deps, name: &SecretName) -> Result<bool, CoreError>;
pub fn redact_event(deps: &Deps, ev: NewEvent) -> Result<NewEvent, CoreError>;
pub fn redact_text(deps: &Deps, text: &str) -> String;
pub fn redact_capsule(deps: &Deps, store: &Store, capsule: &Capsule) -> Result<Capsule, CoreError>;
pub fn redact_job_artifacts(deps: &Deps, store: &Store, job: Option<&JobId>) -> Result<(), CoreError>;
pub fn request_secret(deps: &Deps, name: &SecretName, now: Timestamp) -> Result<SecretRequest, CoreError>;
pub struct Redactor { /* exact values, longest first */ }
impl Redactor {
    pub fn load(deps: &Deps) -> Result<Redactor, CoreError>;
    pub fn redact(&self, text: &str) -> String;
    pub fn is_empty(&self) -> bool;
}
```

### 13.3 Store repository API

The store exposes these repository methods. The schema files and tables are described in §3; the
methods below are the supported read/write surface:

```rust
// repo_projects.rs
pub fn project_entry(&self, name: &ProjectName) -> Result<Option<ProjectEntry>, StoreError>;
pub fn list_project_entries(&self) -> Result<Vec<ProjectEntry>, StoreError>;
pub fn project_by_path(&self, path: &std::path::Path) -> Result<Option<ProjectEntry>, StoreError>;
pub fn set_project_state(&self, name: &ProjectName, state: ProjectState, reason: &str)
    -> Result<ProjectEntry, StoreError>;
pub fn set_project_weight(&self, name: &ProjectName, weight: u8) -> Result<(), StoreError>;
pub fn remove_project_record(&self, name: &ProjectName) -> Result<(), StoreError>;

// repo_capacity.rs
pub fn insert_host_sample(&self, s: &HostSample) -> Result<(), StoreError>;
pub fn host_samples_since(&self, since: Timestamp) -> Result<Vec<HostSample>, StoreError>;
pub fn prune_host_samples(&self, before: Timestamp, minute_before: Timestamp) -> Result<u64, StoreError>;
pub fn upsert_run_usage(&self, u: &RunUsage) -> Result<(), StoreError>;
pub fn run_usage(&self, run: &RunId) -> Result<Option<RunUsage>, StoreError>;
pub fn run_usage_for_group(&self, q: &UsageQuery, limit: u32) -> Result<Vec<RunUsage>, StoreError>;
pub fn upsert_usage_estimate(&self, e: &UsageEstimate) -> Result<(), StoreError>;
pub fn usage_estimate(&self, k: &EstimateKey) -> Result<Option<UsageEstimate>, StoreError>;
pub fn list_usage_estimates(&self, project: Option<&ProjectName>) -> Result<Vec<UsageEstimate>, StoreError>;
pub fn insert_capacity_target(&self, t: &CapacityTarget) -> Result<i64, StoreError>;
pub fn latest_capacity_target(&self) -> Result<Option<CapacityTarget>, StoreError>;
pub fn insert_capacity_advice(&self, a: &CapacityAdvice) -> Result<(), StoreError>;
pub fn capacity_advice(&self, id: &str) -> Result<Option<CapacityAdvice>, StoreError>;
pub fn latest_open_advice(&self) -> Result<Option<CapacityAdvice>, StoreError>;
pub fn finish_capacity_advice(&self, id: &str, state: AdviceState, by: &str, now: Timestamp)
    -> Result<(), StoreError>;

// repo_surfaces.rs
pub fn upsert_surface(&self, s: &Surface) -> Result<(), StoreError>;
pub fn surface(&self, id: &SurfaceId) -> Result<Option<Surface>, StoreError>;
pub fn delete_surface(&self, id: &SurfaceId) -> Result<bool, StoreError>;
pub fn lease(&self, session: &SessionId) -> Result<Option<Lease>, StoreError>;
pub fn take_lease(&self, l: &Lease) -> Result<Option<Lease>, StoreError>;
pub fn heartbeat_lease(&self, session: &SessionId, token: &str, at: Timestamp) -> Result<bool, StoreError>;
pub fn release_lease(&self, session: &SessionId, token: &str) -> Result<bool, StoreError>;
pub fn stale_leases(&self, before: Timestamp) -> Result<Vec<Lease>, StoreError>;
pub fn list_leases(&self) -> Result<Vec<Lease>, StoreError>;

// repo_catalog.rs
pub fn upsert_catalog(&self, e: &CatalogEntry) -> Result<(), StoreError>;
pub fn catalog_entry(&self, path: &str) -> Result<Option<CatalogEntry>, StoreError>;
pub fn list_catalog(&self, f: &CatalogFilter) -> Result<Vec<CatalogEntry>, StoreError>;
pub fn mark_catalog_deleted(&self, id: &str, at: Timestamp) -> Result<(), StoreError>;
pub fn delete_catalog(&self, id: &str) -> Result<bool, StoreError>;
```

Plus in the existing repositories:

```rust
// session_log.rs (v0.1 file, one additive method)
impl SessionLog {
    pub fn append_with_id(&self, ev: NewEvent, id: EventId) -> Result<AppendedEvent, StoreError>;
}
// existing v0.1 method used by ingest for idempotency; no signature change:
// Store::event_row_by_id(&EventId) -> Result<Option<EventIndexRow>, StoreError>

// repo_sessions.rs
pub fn update_capsule_body(&self, c: &Capsule) -> Result<(), StoreError>;
pub fn insert_native_event(&self, session: &SessionId, harness: HarnessKind, native_id: &str,
                           event_id: &EventId, at: Timestamp) -> Result<bool, StoreError>; // false = duplicate
pub fn native_event(&self, session: &SessionId, harness: HarnessKind, native_id: &str)
    -> Result<Option<EventId>, StoreError>;
pub fn set_session_compacted(&self, session: &SessionId, at: Timestamp) -> Result<(), StoreError>;

// repo_misc.rs (inbox + secrets)
pub fn insert_node_inbox(&self, i: &InboxItem) -> Result<(), StoreError>;
pub fn upsert_secret_index(&self, name: &SecretName, last4: &str, now: Timestamp) -> Result<(), StoreError>;
pub fn list_secret_index(&self) -> Result<Vec<SecretInfo>, StoreError>;
pub fn remove_secret_index(&self, name: &SecretName) -> Result<bool, StoreError>;
```

### 13.4 Exact hook points

These are the only lines the v0.2 features add to files the v0.1 code already owns. Each hook is a
no-op when its feature is disabled, so the v0.1 path behaves exactly as before.

1. **`crates/vowp-core/src/loops/supervisor.rs`, `Supervisor::tick`** — immediately after
   `let paused = self.store.kv_get("paused")?...;` and before `let mut steps = Vec::new();`:

   ```rust
   // Node-level measure, lease sweep and lifecycle tick.
   let _ = crate::capacity::sample(&self.deps, &self.store, now);
   let _ = crate::surface::sweep_leases(&self.deps, &self.store, now);
   let _ = crate::lifecycle::tick(&self.deps, &self.store, now);
   ```

   `sweep_leases` also signals the active ingest writer with `IngestAck::LeaseRevoked` when a
   takeover commits. The no-op leaves v0.1 unchanged until the push is enabled.

2. **`crates/vowp-core/src/loops/supervisor.rs`, `Supervisor::step_dispatch`** — after
   `let job = self.store.require_job(&choice.job_id)?;` and before `apply_with(...)`:

   ```rust
   // Capacity admission.
   if !dispatch::admit(&self.deps, &self.store, &job)? {
       continue;
   }
   ```

   **`crates/vowp-core/src/loops/dispatch.rs`** gains the hook:

   ```rust
   /// Returns false when the job must wait; writes the waiting reason to jobs.note.
   /// The no-op returns true, which keeps v0.1 dispatch unchanged.
   pub(crate) fn admit(deps: &Deps, store: &Store, job: &Job) -> Result<bool, CoreError>;
   ```

3. **`crates/vowp-core/src/session/service.rs`, `run_turn`** — immediately before
   `let start_input = vowp_protocol::StartInput {`:

   ```rust
   // Inject exactly the configured secret names.
   let mut start_env = self.start_env(&executor);
   start_env.extend(crate::vault::env_for(&self.deps, &req)?);
   ```

   The `StartInput.env` field changes from `env: self.start_env(&executor)` to `env: start_env`.

4. **`crates/vowp-core/src/session/service.rs`, `append_event`, `append_harness` and
   `append_capsule_event`** — immediately before `let appended = log.append(NewEvent {`:

   ```rust
   // Exact-value redaction before the log line exists.
   let new_event = crate::vault::redact_event(&self.deps, NewEvent { /* the event */ })?;
   ```

   The append call uses `new_event`; the index row is built from the returned envelope as today.
   In the free function `append_capsule_event(deps, store, capsule)` the same line uses `deps` (no
   `self`), before its `log.append` call.

5. **`crates/vowp-core/src/session/service.rs`, end of `run_turn`** — after the run row/ledger are
   finished and before `Ok(TurnResult { .. })`:

   ```rust
   // Redact job artifacts written by the harness (report.md, raw mirror).
   let _ = crate::vault::redact_job_artifacts(&self.deps, &self.store, req.job_id.as_ref());
   ```

   `session/remote.rs::ingest_lines` calls the same hook after a `Summary` line, because the
   remote harness may have written a report in the shared worktree on the executor node; the home
   redacts the copy it can see and the executor surface redacts its own before spooling.

6. **`crates/vowp-core/src/session/service.rs`, `SessionService::write_capsule`** — after
   `CapsuleWriter::new(...).write(...)` returns:

   ```rust
   // Redact the capsule file and body row.
   let capsule = crate::vault::redact_capsule(&self.deps, &self.store, &capsule)?;
   ```

7. **`crates/vowp-core/src/session/service.rs`, `run_turn` start and `session/remote.rs`,
   `ingest_lines`** — before any interactive input is accepted:

   ```rust
   // A live lease must match VOWP_LEASE_TOKEN.
   crate::surface::check_lease(&self.deps, &self.store, &req.session_id)?;
   ```

   A job session has no lease and passes; an interactive surface sets the env var from the grant.
   `ingest_lines` uses `surface::check_lease_with(store, &grant.session, Some(&grant.lease))`
   instead, because its token comes from `RemoteLine::Hello`, not the environment.

8. **`crates/vowp-core/src/loops/supervisor.rs`, `Supervisor::tick`** — the fourth node hook,
   immediately before `steps.push(self.step("board", |this| this.step_board(now)));`:

   ```rust
   // Node snapshot after the project steps.
   let _ = crate::fleet::write_snapshot(&self.deps, &self.store, now);
   ```

   `fleet::node_tick` (§13.2) is a convenience wrapper with the same four calls in the same order
   (three at the top, snapshot at the end); a caller may use it once instead of the four lines.
   The hook bodies are no-ops until their features are enabled.

### 13.5 CLI command surface and the `cli.rs` freeze

`crates/vowp-cli/src/cli.rs` is the one place where every command variant and every flag is
declared. It adds:

- the new top-level commands `project`, `fleet`, `capacity`, `secret`, `gc`, `tunnel`, `setup`,
  `control`, `orchestrator`, `migrate`, `mcp`, and the hidden `__exec`, `__vault-exec`,
  `__vault-admin` (handlers in `cmd/{exec,secret}.rs`);
- the hidden `SessionCommand` variants `TurnBegin` and `Ingest` (handlers in `cmd/session.rs`)
  and `Lease` and `Takeover` (handlers in the new `cmd/lease.rs`);
- the new/changed flags on existing commands: `ChatArgs` (`--executor/--node/--follow/--takeover`),
  `BackupArgs` (`--to/--offsite/--encrypt/--restore/--to-home`), `SchemaArgs` (`snapshot`),
  `LoopsArgs` (`--all`), `ScheduleArgs` (`--all`), `InitArgs`/`DoctorArgs` (project/doctor checks),
  and `WindowArgs` (the `capacity` kind);
- a new `cmd/schema.rs` and its dispatch, moving the `schema` command out of `cmd/maint.rs`, so
  `vowp schema snapshot` has its own file (`maint.rs` keeps `backup`/`reindex`);
- `all_schemas()` in `vowp-protocol/src/schema.rs`.

`V0_2_COMMANDS` is the exact list of new **top-level** clap names, hidden ones included because
`Cli::command().get_subcommands()` enumerates hidden subcommands (the v0.1 `__window` already proves
this). `chat` is not in it because `chat` already exists in `V0_1_COMMANDS`; `__window capacity` is
a new kind of the existing `__window`, not a new top-level name:

```rust
// cli.rs, next to V0_1_COMMANDS
pub const V0_2_COMMANDS: &[&str] = &[
    "project", "fleet", "capacity", "secret", "gc", "tunnel", "setup", "control",
    "orchestrator", "migrate", "mcp", "__exec", "__vault-exec", "__vault-admin",
];
```

`crates/vowp-cli/tests/core.rs::help_lists_every_v0_1_command` asserts that the set of clap
subcommands equals `V0_1_COMMANDS`. v0.2 extends the expectation to `V0_1_COMMANDS ∪
V0_2_COMMANDS`; the help-text loop skips names starting with `__` (instead of only `__window`):

```rust
let mut expected: Vec<String> = V0_1_COMMANDS
    .iter()
    .chain(V0_2_COMMANDS.iter())
    .map(|name| (*name).to_owned())
    .collect();
// ... compare sorted expected with sorted `Cli::command().get_subcommands()` ...
for command in V0_1_COMMANDS
    .iter()
    .chain(V0_2_COMMANDS.iter())
    .filter(|name| !name.starts_with("__"))
{
    assert!(help.contains(command), "help does not mention {command}:\n{help}");
}
```

The test keeps its name `help_lists_every_v0_1_command` (renaming is optional); the cross-cutting
`help_lists_every_command` in §15 is the same assertion over the full command set. No other test
enumerates the whole command list: `vowp-cli/tests/maint.rs` iterates only the v0.1 dry-invocation
list and is unaffected.

The command files:

```text
cmd/project.rs      ProjectArgs { command: ProjectCommand }
cmd/fleet.rs        FleetArgs { command: FleetCommand }
cmd/capacity.rs     CapacityArgs { command: CapacityCommand }
cmd/secret.rs       SecretArgs { command: SecretCommand }
cmd/gc.rs           GcArgs { apply, class, now }
cmd/tunnel.rs       TunnelArgs { command: TunnelCommand }
cmd/setup.rs        SetupArgs { command: SetupCommand }
cmd/control.rs      ControlArgs { command: ControlCommand }
cmd/orchestrator.rs OrchestratorArgs { command: OrchestratorCommand }
cmd/migrate.rs      MigrateArgs { dry_run, rollback, yes }
cmd/exec.rs         ExecArgs { grant: PathBuf, spool_dir: Option<PathBuf> }
cmd/mcp.rs          McpArgs { session, surface, token_file }
cmd/lease.rs        LeaseArgs/TakeoverArgs (hidden)
cmd/schema.rs       SchemaArgs { command }
```

`cmd/session.rs` holds the dispatch and reason arms. The file's `SessionCommand::run` match is
exhaustive over the enum, so adding the four variants requires updating it. It adds
exactly these dispatch arms (plus the `reason_name` arm below):

```rust
SessionCommand::TurnBegin(_) | SessionCommand::Ingest(_) => {
    Err(CliError::NotImplemented("session turn-begin/ingest"))
}
SessionCommand::Lease(args) => crate::cmd::lease::lease(ctx, io, args),
SessionCommand::Takeover(args) => crate::cmd::lease::takeover(ctx, io, args),
```

`cmd/lease.rs` declares `pub fn lease(..)` and `pub fn takeover(..)`; until the handlers exist they
return `NotImplemented`. `cmd/session.rs` also adds the missing
`SwitchReason::Takeover => "takeover"` arm to the private `reason_name` match
(`cmd/session.rs:421-430`), which has no wildcard. `parse_reason` is a string match and needs no
change; `takeover` is not a user-supplied `session switch --reason`.

Every unimplemented handler returns `CliError::NotImplemented("...")` and **exit 1** (the current
`errors.rs` mapping). A later change fills the existing file; it never re-adds a command.

### 13.6 CLI support types

```rust
// node_ctx.rs
pub enum NodeRole { Home, Control, Uninitialised }
pub struct NodeCtx { /* role, paths, control, target */ }
pub struct ControlHome { pub root: PathBuf, pub config: ControlConfig }
pub struct RemoteHome { pub name: String, pub ssh_target: String, pub remote_home: String }
pub struct CachedSnapshot { pub snapshot: Snapshot, pub fetched_at: Timestamp, pub source: String }

// ssh_stream.rs
pub const INGEST_ACK_WINDOW: usize = 64;

pub struct SshTarget { pub node: NodeName, pub ssh_target: String, pub remote_home: String }
pub struct IngestClient { /* ssh child, spool path, acked ids, in-flight window, revoked notice */ }
impl IngestClient {
    pub fn new(target: SshTarget, session: SessionId, run: RunId, lease: String, spool: PathBuf) -> Self;
    pub fn send(&mut self, line: &RemoteLine) -> Result<Option<IngestAck>, CliError>;
    pub fn replay(&mut self) -> Result<u32, CliError>;
    pub fn revoked(&self) -> Option<&IngestAck>;
    pub fn finish(&mut self) -> Result<(), CliError>;
}

// chat/mod.rs
pub struct ChatOptions { pub session: SessionId, pub executor: Option<ExecutorRef>,
                         pub node: Option<NodeName>, pub follow: bool, pub takeover: bool }
pub fn run(ctx: &mut Ctx, io: &mut dyn IoStreams, opts: &ChatOptions) -> Result<i32, CliError>;

// mcp/mod.rs
pub struct McpOptions { pub session: SessionId, pub surface: SurfaceId, pub token_file: PathBuf }
pub fn serve(io: &mut dyn IoStreams, opts: &McpOptions) -> Result<i32, CliError>;
```

`SshTarget` is built from `NodeCtx::remote()` plus the executor node row. `IngestClient` owns the
spool files (`events.jsonl`, `acked.json`); it appends and fsyncs an event to `events.jsonl`
**before** `send` writes it to ssh, records each ack in `acked.json`, and keeps at most
`INGEST_ACK_WINDOW` (64) unacked events in flight. A failed `send` leaves the event spooled and
returns `Ok(None)`; the next `send`/`replay` reconnects and resends from the first unacked event.
`replay` returns the number of events resent. `send` can return `IngestAck::LeaseRevoked`; `__exec`
then stops reading stdin, prints the read-only notice once and exits 75 without sending more
events, while the surface follows the log read-only. `Hello` is never spooled (it carries the lease
token); it is re-sent from the grant on every reconnect. ControlMaster is optional (`ssh -M -N -S
<private socket>` under `~/.vowp/tmp/`, mode 0700); a dead master falls back to a cold connection.
It never spawns a shell; the argv is built by `remote::build_ssh_argv` with the private quoting
helper.

### 13.7 `SessionRole`, `SwitchReason` and test literals

The two additive enum variants are in `crates/vowp-protocol/src/event.rs`:
`SessionRole::Orchestrator` and `SwitchReason::Takeover`. They require updating exhaustive matches
in `vowp-core` and tests. The `event.rs` edit and
`crates/vowp-core/src/session/switch.rs` (recorded in §14.1) update the match arms the compiler
points at. The new arms are behaviour-preserving (`Orchestrator` behaves like `User` for session
selection; `Takeover` maps to the existing takeover switch path and is rejected by `session switch`
unless the surface holds the lease). Known exhaustive sites: `crates/vowp-core/src/session/switch.rs::run_role`
(add `Orchestrator` to the `Worker | User` arm) and `crates/vowp-cli/src/cmd/session.rs::reason_name`
(add `SwitchReason::Takeover => "takeover"`). Both files are in the §14.1 ownership map for exactly
these arms.

---

## 14. Module map

The table records where each part of the v0.2 design lives. The listed files are the module home
for that area; a change to one of them changes the contract described in this document.

| Area | Files |
|---|---|
| Protocol types and CLI surface | `crates/vowp-protocol/src/{capacity,catalog,event,ops,schema,snapshot,surface,vault}.rs`; `crates/vowp-cli/src/cli.rs`, `cmd/mod.rs`, `cmd/schema.rs`, `cmd/session.rs` (dispatch/reason arms), `cmd/window.rs` (the `capacity` arm), `tests/core.rs` |
| Node supervisor and project registry | `crates/vowp-core/src/loops/{supervisor,mod,reconcile}.rs`; `crates/vowp-cli/src/cmd/{up,project,migrate,init,loops,schedule}.rs`, `node_ctx.rs`; `scripts/vowp.service`, `framework/templates/scripts/vowp.service`; tests `loops_supervisor.rs`, `node_supervisor.rs`, `up.rs` |
| Sessions and remote execution | `crates/vowp-core/src/session/{service,switch,remote}.rs`; `crates/vowp-cli/src/cmd/{exec,session}.rs`, `ssh_stream.rs`; tests `session_run.rs`, `switch.rs`, `remote_exec.rs`, `session.rs` |
| Capacity measurement | `crates/vowp-core/src/capacity/{sampler,usage,estimates,scope}.rs`, `crates/vowp-store/src/repo_capacity.rs`, `crates/vowp-adapters/src/proc/mod.rs`, `crates/vowp-core/src/gates/cmd.rs`; tests `capacity_measure.rs` |
| Snapshot and fleet | `crates/vowp-core/src/fleet/**`, `crates/vowp-core/src/ledger/board.rs`; `crates/vowp-cli/src/cmd/{fleet,status,board,doctor,schema}.rs`; tests `fleet_snapshot.rs`, `status.rs`, `board.rs` |
| Control machine and tunnels | `crates/vowp-cli/src/{ctx,remote}.rs`, `cmd/{control,tunnel,node}.rs`; tests `remote.rs`, `control.rs` |
| Secrets and vault | `crates/vowp-core/src/vault/**`; `crates/vowp-cli/src/cmd/secret.rs`; tests `vault.rs` |
| Lifecycle and GC | `crates/vowp-core/src/lifecycle/{mod,classes,gc,compact}.rs`, `crates/vowp-store/src/repo_catalog.rs`; `crates/vowp-cli/src/cmd/gc.rs`; tests `lifecycle.rs` |
| Backup and restore | `crates/vowp-core/src/lifecycle/backup.rs`; `crates/vowp-cli/src/cmd/maint.rs` (backup/reindex; `schema` lives in `cmd/schema.rs`); tests `maint.rs` |
| Surfaces, leases and takeover | `crates/vowp-core/src/surface/{lease,takeover}.rs`, `crates/vowp-store/src/repo_surfaces.rs`; `crates/vowp-cli/src/cmd/lease.rs`; tests `surface_lease.rs` |
| Capacity admission and brakes | `crates/vowp-core/src/capacity/{admit,brakes,targets,fair}.rs`, `crates/vowp-core/src/loops/dispatch.rs`; `crates/vowp-cli/src/cmd/capacity.rs`; tests `capacity_admit.rs` |
| App hosts and mirror | `crates/vowp-core/src/surface/{native,opencode_host,claude_remote,mirror}.rs`, `crates/vowp-core/src/session/importer.rs`; tests `app_hosts.rs`, `mirror.rs` |
| MCP server | `crates/vowp-cli/src/cmd/mcp.rs`, `crates/vowp-cli/src/mcp/**`; tests `mcp.rs` |
| Chat fallback | `crates/vowp-cli/src/cmd/session.rs`, `crates/vowp-cli/src/chat/**`, `crates/vowp-core/src/surface/follow.rs`; tests `session.rs`, `chat.rs` |
| Capacity advisor | `crates/vowp-core/src/capacity/advisor.rs`, `framework/skills/vowp-capacity-advisor/**`; tests `framework.rs`, `capacity_advisor.rs` |
| Orchestrators | `crates/vowp-core/src/surface/orchestrator.rs`; `crates/vowp-cli/src/cmd/orchestrator.rs`, `framework/skills/vowp-orchestrator/**`, `framework/templates/ORCHESTRATOR.md`; tests `framework.rs`, `orchestrator.rs` |
| Setup | `crates/vowp-cli/src/cmd/setup.rs`, `crates/vowp-cli/src/setup/**`, `scripts/install.sh`; tests `setup.rs` |
| Setup verification | `crates/vowp-cli/src/setup/verify.rs`; tests `setup_verify.rs` |

### 14.1 File ownership map

The table records where each frozen interface lives. A change to a path below changes the contract
in this section; keep the edit in that file and update this document with it.

| Path | Area | Why |
|---|---|---|
| `crates/vowp-cli/src/cli.rs` | CLI surface | Every new top-level variant and every changed `*Args` flag, including the hidden `SessionCommand::{TurnBegin,Ingest,Lease,Takeover}`, `__vault-exec` and `__vault-admin`. |
| `crates/vowp-cli/src/cmd/mod.rs` | CLI modules | Declares `schema`, `lease` and the other new modules. |
| `crates/vowp-cli/src/cmd/schema.rs` | CLI schema | `vowp schema snapshot`; kept out of `cmd/maint.rs` so maintenance and schema changes do not share a file. |
| `crates/vowp-protocol/src/schema.rs` | Protocol schema | `all_schemas()` must include `Snapshot` and the new types. |
| `crates/vowp-protocol/src/ops.rs` | Control-plane DTOs | `FleetStatus`, `FleetQueueRow`, `Tunnel`, `MigrationReport`, `MigrationCount`; one module home for the control-plane DTOs. |
| `crates/vowp-protocol/src/event.rs` | Protocol events | `SessionRole::Orchestrator` and `SwitchReason::Takeover` are defined here, not in `session.rs`. |
| `crates/vowp-cli/src/cmd/session.rs` | Session CLI | `SessionCommand::run` is an exhaustive match over the enum: the four arms from §13.5 (TurnBegin/Ingest return NotImplemented; Lease/Takeover call the `cmd/lease.rs` functions) and the missing `SwitchReason::Takeover => "takeover"` arm in `reason_name`. The match must stay exhaustive. |
| `crates/vowp-cli/src/cmd/window.rs` | tmux windows | `vowp __window capacity` is required by §4.4/§12.5 but the v0.1 match rejects every kind except board/supervisor/planner/keeper/inbox/job. It adds `"capacity"` as an accepted kind and renders `capacity show`. |
| `crates/vowp-cli/tests/core.rs` | CLI tests | `help_lists_every_v0_1_command` enumerates every clap subcommand and compares it to `V0_1_COMMANDS`; v0.2 extends the expectation to `V0_1_COMMANDS ∪ V0_2_COMMANDS` (§13.5) and filters hidden names from the help-text loop. Without this the 14 new top-level names make the test fail. |
| `crates/vowp-core/src/deps.rs` | Store binding | The `open_store()` binding in §3.8. The signature does not change; the body needs the new `Store` constructors and the `_node` fallback. |
| `crates/vowp-protocol/src/ids.rs` | Project names | `ProjectName::parse` accepts exactly the reserved literal `_node`; every other leading underscore stays invalid. Needed by the `_node` DB and the migration; `project add`/`init` still refuse `_node`. |
| `crates/vowp-core/src/session/switch.rs` | Switch protocol | The exhaustive `SessionRole::Orchestrator` match arm required by the additive enum. |
| `crates/vowp-cli/src/cmd/init.rs` | Project registry | `init` creates the project DB and `_node`, and registers the project. |
| `crates/vowp-cli/src/cmd/loops.rs` | Project loops | Loop rows are per project; `--all` and the project selection. |
| `crates/vowp-cli/src/cmd/schedule.rs` | Project schedules | Schedules are per project; `--all` and the project selection. |
| `crates/vowp-cli/src/cmd/doctor.rs` | Diagnostics | Projects, snapshot and schema checks; `vault::mode` supplies the secrets mode, so `doctor` itself does not read a secret. |
| `crates/vowp-cli/src/cmd/lease.rs` | Session CLI | `session lease` and `session takeover` handlers (`lease(..)`, `takeover(..)`); the dispatch arms in `cmd/session.rs` call these functions. |
| `crates/vowp-cli/src/cmd/maint.rs` | Maintenance | Keeps `backup`/`reindex`; the `schema` handler lives in `cmd/schema.rs`. |

`crates/vowp-cli/src/errors.rs` is **not** extended: unimplemented handlers exit 1 through the
existing `NotImplemented` mapping, which `tests/core.rs` already asserts. `tests/core.rs` itself
changes only for the command-list expectation above; no other test edit is needed.

---

## 15. Named tests per area

Every test is hermetic: temporary `VOWP_HOME`, private `TMUX_TMPDIR` + `tmux -L`, fake adapters,
fake SSH, fake apps; no network, no real harness. A test that cannot fail if the feature is wrong
is not allowed.

**Store split and migration**
`global_schema_loads_and_matches_enums`, `project_schema_loads_and_matches_enums`,
`global_and_project_schemas_share_no_application_table_names`,
`store_binds_global_plus_exactly_one_project`, `two_project_handles_never_shadow_each_other`,
`node_inbox_and_node_ledger_resolve_explicitly`, `inbox_list_merges_node_and_project_rows`,
`ledger_since_and_cost_totals_merge_both_files`, `project_column_is_validated_not_foreign_keyed`,
`every_sql_statement_names_its_database`, `v01_database_migrates_with_matching_row_counts`,
`migration_refuses_while_supervisor_runs`, `migration_is_restartable_at_each_step`,
`migration_rollback_restores_v01_home`, `migration_multi_project_split`, `migration_node_rows_go_to_primary`,
`migration_workers_follow_their_job_project`, `migration_unassignable_workers_dropped_and_reported`,
`crash_between_global_and_project_write_is_repaired`, `cost_ledger_replay_does_not_duplicate`,
`cross_db_reconcile_is_idempotent`, `request_queue_is_project_scoped`,
`schema_version_compat_shim_reports_project_one`, `global_schema_version_reports_two`,
`schema_versions_are_global_2_project_1`, `upsert_project_creates_the_project_db`,
`fresh_home_deps_open_store_binds_node_project`, `node_bound_handle_rebinds_once_to_first_real_project`,
`real_bound_handle_never_rebinds`. The v0.1 compatibility gate is the six test files/tests in §3.8,
run unmodified: `cargo test -p vowp-core --test deps --test session_run --test capsule --test
switch_drain --test bundle`. `crates/vowp-store/tests/misc.rs::fk_cascade_project_delete` is
replaced by `project_delete_keeps_other_projects_rows` (the old test asserts a cross-project
`ON DELETE CASCADE` that no longer exists). `misc.rs::backup_produces_openable_copy` covers the
split backup unit (the legacy `--to *.db` form is a global-only copy by §10.3, so it asserts the
global rows there and the job/ledger rows in the copied project DB). No other store test assumes
one DB for two projects. The store-test fixtures that write project-scoped rows without a project
row in v0.1 add the `upsert_project` call the other fixtures already have
(`vowp-store/tests/loops.rs`), or are rewritten for the one-bound-project rule:
`vowp-store/tests/ledger.rs` writes `proj`, `alpha` and `beta` on one store, so it opens one store
per project (or writes `node_ledger` with `project = None`) and updates the expected totals.
`vowp-store/tests/migrate.rs::open_creates_schema_version_1` asserted the 19 v0.1 tables in
`vowp.db` and expected `insert_run` to raise an FK error on an unbound store; the v0.2 test
asserts the global v2 schema in `vowp.db`, the project v1 schema in a created project DB, and an
`insert_run` failure that is `NoProject` on the unbound handle. No `vowp-core` or `vowp-cli` test
outside the §3.8 table needs an edit: their fixtures either `upsert_project` (`TestEnv`,
`gates_support`, `promote_support`, the scheduler fixture) or rely on the `_node` fallback.

**Node supervisor and projects**
`one_supervisor_runs_two_projects_round_robin`, `deps_open_store_uses_the_project_config`,
`broken_project_pauses_alone_and_others_tick`,
`project_pause_lets_running_work_finish`, `project_resume_clears_broken_reason`,
`project_add_ls_pause_resume_archive_rm`, `project_rm_archives_db_and_keeps_production_records`, `project_less_session_uses_node_project`,
`loops_and_schedule_require_project_when_several`,
`up_once_runs_one_node_tick`, `up_refuses_on_control_machine`, `up_migrates_with_backup_first`,
`tmux_home_and_project_layout`, `status_and_board_show_every_project`,
`node_supervisor_restart_reconciles_cross_db`.

**Snapshot and fleet**
`snapshot_json_validates_against_exported_schema`, `snapshot_written_atomically_every_tick`,
`snapshot_reader_never_sees_partial_json`, `fleet_status_reads_fresh_snapshot_or_recomputes`,
`fleet_status_json_keeps_v01_status_keys`, `fleet_queue_includes_waiting_reason`,
`fleet_inbox_merges_node_and_project`, `board_json_compat_with_one_project`,
`schema_snapshot_exports_versioned_schema`.

**Control machine and tunnels**
`control_init_writes_control_json`, `control_routes_status_over_fake_ssh`,
`control_cache_serves_as_of_when_home_down`, `control_write_never_uses_cache`,
`up_refuses_on_control`, `tunnel_open_records_pid_and_port`, `tunnel_close_kills_group`,
`tunnel_refuses_privileged_port_without_flag`, `tunnel_list_cleans_dead_pid`.

**Capacity measurement**
`sampler_parses_proc_meminfo_load_and_pressure`, `sampler_degrades_on_non_linux`,
`scope_reads_cgroup_memory_peak`, `scope_records_anon_file_kernel_and_cpu_usec`,
`scope_falls_back_to_process_tree`, `run_usage_written_for_adapter_and_gate`,
`build_mode_cold_warm_split`, `estimates_use_exact_group_then_pool_then_default`,
`estimates_need_five_samples`, `oom_kill_marks_run_usage`, `sampler_prunes_old_rows`.

**Capacity admission and brakes**
`admit_blocks_when_ram_would_drop_below_reserve`, `admit_blocks_on_cpu_and_disk`,
`admit_blocks_on_provider_limit`, `admit_blocks_at_target`, `brake_trips_on_memory_pressure`,
`brake_clears_after_two_clean_samples`, `brake_never_signals_running_work`,
`owner_override_above_advice_is_honoured_and_shown`, `capacity_set_history_and_json`,
`fairness_round_robins_by_weight`, `waiting_reason_on_board_and_queue`,
`auto_target_uses_measured_limit`.

**Capacity advisor**
`advisor_writes_capacity_advice_on_timer_and_events`,
`advisor_creates_decision_inbox_item_with_apply_keep`,
`advisor_never_changes_target_itself`, `advisor_applies_only_after_owner_yes`,
`advisor_report_json_includes_estimates_and_waiting`,
`advisor_respects_the_thirty_minute_gap`.

**Remote execution**
`turn_begin_writes_framing_events_and_returns_grant`,
`remote_exec_streams_events_and_home_assigns_seq`, `remote_ingest_is_idempotent_by_event_id`,
`exec_spools_and_fsyncs_before_send`, `ingest_ack_window_is_64_and_async`,
`ssh_drop_spools_and_replays_without_gap_or_duplicate`, `client_crash_resumes_from_spool`,
`remote_ingest_rejects_bad_lease`, `remote_summary_finishes_run_and_terminal_events`,
`remote_probe_uses_remote_node_and_switch`, `remote_summary_writes_run_usage`,
`lease_token_never_in_argv_or_logs`.

**Surfaces and leases**
`lease_acquire_is_one_holder`, `heartbeat_keeps_lease_live`, `stale_lease_free_after_sixty_seconds`,
`takeover_cuts_old_surface_within_two_seconds`, `lease_revoked_is_pushed_within_2s`,
`takeover_drains_and_writes_capsule`,
`takeover_before_first_executor_writes_note_only`,
`follow_mode_prints_new_events_and_refuses_writes`, `lease_check_rejects_wrong_token`,
`lease_sweep_drops_dead_surface`.

**Chat fallback**
`chat_takes_over_and_prints_notice`, `chat_follow_mode_is_read_only`,
`chat_slash_switch_status_capsule`, `chat_remote_executor_uses_surface_machine`,
`chat_narrow_terminal_no_colour_or_wide_tables`, `chat_non_tty_reads_turns_until_eof`.

**Orchestrators**
`orchestrator_new_writes_only_project_local_config`,
`orchestrator_registers_mcp_in_opencode_and_claude_configs`,
`orchestrator_join_instructions_desktop_and_phone`, `orchestrator_role_session_is_project_less`,
`orchestrator_new_refuses_existing_name_without_force`, `orchestrator_stop_releases_lease`,
`orchestrator_state_survives_supervisor_restart`.

**App hosts and transcript mirror**
`opencode_host_starts_and_stops_with_fake_server`, `opencode_v2_service_pair_basic_auth`,
`opencode_mirror_maps_messages_to_events`, `mirror_reads_api_event_and_backfills_export`,
`mirror_is_incremental_and_idempotent`, `claude_importer_maps_transcript`,
`claude_remote_refuses_without_login`, `resume_context_builds_capsule_and_tail`,
`app_host_fake_never_opens_a_socket`. The real-provider MCP tool-exposure re-test is an
acceptance check (§16.20), not a hermetic unit test.

**MCP server**
`mcp_initialize_and_tools_list`, `mcp_server_discover_answered`, `mcp_read_tools_return_snapshot_data`,
`mcp_write_tool_checks_lease_and_records_actor`, `mcp_old_app_write_gets_read_only_notice`,
`mcp_secret_request_never_returns_value`, `mcp_unknown_method_is_jsonrpc_error`,
`mcp_works_over_scripted_stdio_client`.

**Secrets and vault**
`secret_set_hidden_prompt_never_echoes`, `secret_ls_shows_last4_only`,
`secret_request_page_single_use_and_expiry`, `vault_basic_mode_and_upgrade`,
`env_for_injects_only_configured_names`, `redaction_removes_value_from_events_capsules_reports_snapshot`, `vault_admin_writes_through_helper`,
`secret_set_without_tty_exits_5`,
`redaction_never_logs_value_or_prefix`, `secret_scan_gate_still_passes`.

**Lifecycle and GC**
`gc_dry_run_lists_by_class_and_changes_nothing`, `gc_apply_never_deletes_protected_classes`,
`gc_removes_run_output_after_fourteen_days`, `gc_compacts_closed_session_to_capsule_and_summary`,
`gc_removes_merged_worktree_and_keeps_failed_seven_days`,
`gc_prunes_host_samples_24h_full_30d_minute`, `nightly_gc_schedule_created`,
`compacted_session_tail_and_export_are_explicit`.

**Backup and restore**
`backup_global_and_all_project_dbs_are_openable`, `backup_rotation_seven_daily_four_weekly`,
`backup_to_rsync_uses_argv_not_shell`, `backup_age_encryption_when_configured`,
`restore_drill_reopens_and_counts_match`, `backup_includes_session_logs_and_blobs`.

**Setup**
`setup_plan_lists_every_phase_command_and_file`, `setup_plan_changes_nothing`,
`setup_plan_json_is_stable`, `setup_home_is_idempotent`, `setup_home_no_sudo_path`,
`setup_home_stops_at_handoff_with_exit_5`, `setup_control_writes_control_home`,
`install_sh_download_still_works`.

**Setup verification**
`setup_verify_fixture_job_merges`, `setup_verify_snapshot_readable_from_control`,
`setup_verify_orchestrator_answers_chat`.

**Cross-cutting**
`help_lists_every_command` (the extended `help_lists_every_v0_1_command`), `no_v02_command_returns_not_implemented`,
`json_flag_outputs_valid_json_for_every_new_listing_command`, `macos_ci_has_no_linux_only_code`,
`private_tree_scrub_finds_no_home_paths_or_hostnames`.

---

## 16. Open assumptions and deviations

These are the items the design resolves in the direction stated; anything else is a follow-up
spec amendment.

1. **Capacity measurement.** cgroup v2 user scopes and the `/proc` fallback were proven on the
   target VPS. This spec uses the scope when available and records
   `memory.peak`, sampled `anon`/`file`/`kernel` at the peak, `cpu.stat usage_usec` and a
   `build_mode` (`cold`/`warm`/`incremental`/`unknown`); `source='proc_tree'` is a fallback only.
   Measurements show the process-tree fallback overcounts warm RSS by ~31 % and undercounts CPU by
   15–36 %, and misses a Docker build entirely. `memory.peak` is cumulative and not
   user-resettable, so per-phase peaks use child cgroups or separate scopes. Disk is a first-class
   dimension: the 290 GB test disk was 98–100 % full and builds failed on ENOSPC, so the 5 GB /
   10 % reserve and a per-job target-dir budget stay. `MemoryHigh` is a safe soft throttle;
   `MemoryMax` is not used. The cold/warm gap is large (2.45 GiB / 300 s CPU vs 0.37 GiB /
   56–60 s), so the `1.5 GiB / 1 core` default is warm-only and estimates are split by
   `build_mode`. The only interface growth is in §6.1 (`RunUsage`, `UsageEstimate`, `ScopeSample`,
   `usage_estimates`).
2. **Remote execution transport.** The transport is SSH with one `session ingest`
   process per run. The client writes and fsyncs every event to its spool **before** sending,
   keeps a 64-event in-flight ack window, and replays from the first unacked event; acks are
   written only after append, duplicate ids are idempotent, and a known id with different content
   is `duplicate-conflict`. A killed client resumes from `events.jsonl` + `acked.json`. ControlMaster
   is an optimisation for handshake/reconnect latency, never a correctness dependency; a dead
   master falls back to a cold connection. The home pushes `IngestAck::LeaseRevoked` on the same
   channel (99.6–265 ms live SSH, <2 s hermetic); `surface::sweep_leases()` signals the writer via
   channel/condvar with a ≤250 ms poll fallback. Per-event fsync is not required. The only wire
   growth is `IngestAck::LeaseRevoked` and `INGEST_ACK_WINDOW` in §7.2/§13.6.
3. **App-surface contract.** opencode v2.0.28 is the proven contract: HTTP API
   under `/api/...` (`/api/event` SSE, `/api/mcp`, `/api/session`, `/api/session/<id>/message|prompt`),
   HTTP Basic `opencode:<token>`, the managed `opencode service`/`opencode pair` path with
   `$XDG_STATE_HOME/opencode/service.json`, loopback-only bind, single-use 5-minute pair links, and
   `location.directory` on session creation (the managed service defaults to `$HOME`). The mirror
   consumes `/api/event` and backfills with export. Claude Code's `.mcp.json` is project-scoped and
   needs one-time approval; Claude Code sends `server/discover` first. Claude Remote Control still
   needs a claude.ai login; on a home without one, `claude_remote.rs` ships the refusal path and a
   five-minute owner check. The Mac desktop app click-through and a real Claude Remote Control join
   are the two remaining owner checks; the pair/tunnel/API path is proven. The opencode host pins
   or version-checks 2.0.28. See item 20 for the MCP tool-exposure gap.
4. **Capacity inbox kind.** The plan says an inbox item of kind `capacity`. v0.2 does **not** add
   `InboxKind::Capacity` (it would force edits to the report renderer outside the capacity
   modules); advice is a `decision` item linked by `capacity_advice.inbox_id`. If the owner wants
   a distinct filter, v0.3 adds the enum variant additively. Acceptance tests and CLI wording must
   therefore look for a `decision` item whose `capacity_advice.inbox_id` matches, not for an
   `InboxKind::Capacity` that does not exist.
5. **`max_workers` is a preference.** v0.1 treats it as a cap. v0.2 admission ignores it as a hard
   limit; the owner target and the measured checks decide. `ensure_worker_slots` still creates that
   many project slots for tmux/visible bookkeeping.
6. **Worker slots are per project.** The `workers` table moves to the project DB. The node
   capacity manager is the only node-wide concurrency authority. A project's slot numbers are not
   comparable across projects.
7. **`vowp project rm` archives, never destroys.** Production records (releases, banks) live in the
   project DB; `rm` moves the DB directory to `~/.vowp/archive/projects/<name>-<ts>/` and removes
   only the registry row. `sessions.project` keeps its text so history still says which project it
   was. This is the concrete reading of plan §3 rule 5 and §10 "keeps production records".
8. **The v0.1 query claim is withdrawn.** Plan §3 rule 1 said v0.1 queries keep working unchanged.
   They do not, because of the `inbox`/`ledger` collision; §3.1 documents the rename and the store
   audits every statement. The public `Store` API is source-compatible, which is what the rest of
   the workspace needs.
9. **`board.json` compatibility.** The node supervisor writes `board.json` only when exactly one
   project is active, so v0.1 tests and scripts that read it keep working. Multi-project homes read
   `snapshot.json`.
10. **`vowp setup` no-sudo mode.** Without passwordless sudo the dedicated `vowp` user cannot be
    created; the setup runs as the current user and says so. The vault user is optional; basic
    secret mode is the documented fallback. This is honest, not a hidden degradation.
11. **Snapshot size.** Queue and session cards are capped (§5.3) so a 1000-job project cannot
    produce a multi-megabyte snapshot. If the cap is hit, the snapshot is still valid; the CLI
    recomputes details from the DB on the home.
12. **Public examples.** This spec contains no owner hostname, path or key; the examples use
    `home.example` and `vowp@home.example`.
13. **Claude Remote Control.** If a claude.ai plan login cannot be used on a VPS, the Claude surface
    is limited to Claude Code with the transcript importer; the opencode surface and `vowp chat`
    remain the complete v0.2 story. The release notes state this honestly; it does not block v0.2.
14. **No new `vowp.toml` keys.** Capacity constants are compiled, not configured, in v0.2. If the
    owner wants to tune the reserve, that is a v0.2.x additive config key with a default.
15. **Flag names that avoid the global `--home`.** The plan writes `vowp setup plan --home <target>`
    and `vowp control init --home <name>`, but `--home` is already the global node-home directory
    flag. v0.2 uses `vowp setup plan|home --target <ssh-target>` and
    `vowp control init --name <home-name> <ssh-target>`; `vowp backup --restore <dir> --to-home
    <dir>`. The behaviour and JSON are exactly the plan's; only the flag spelling avoids a clap
    conflict. `docs/setup/agent-setup.md` documents the concrete commands.
16. **Reserved `_node` project.** The plan allows project-less orchestrator sessions and says `runs`
    live in project DBs. Those two rules need a home for a project-less run, so v0.2 reserves the
    hidden `_node` project and DB. It is not a user project, is hidden from `project ls`/snapshot
    cards, and cannot be removed. `ProjectName::parse` accepts exactly this literal (a one-line
    exception in `vowp-protocol/src/ids.rs`); `project add`/`init` still refuse it. The alternative
    (a duplicate `node_runs` table) is worse.
17. **Session content backup is mandatory.** The plan's `vowp backup` sentence mentions the DBs and
    capsules; this spec adds `sessions/<id>/log.jsonl`, blobs and adapters because a DB-only restore
    loses every conversation. This is a clarification, not a new feature.
18. **Vault admin helper names.** The plan writes the write path as `vowp __vault-exec store|rm
    NAME`; this spec freezes `vowp __vault-admin set|rm NAME` (and keeps `__vault-exec` read-only
    for injection). The behaviour is the same; the CLI uses the spec names. This is recorded
    because it is an interface change, not a cosmetic rename.
19. **Command-list test.** `help_lists_every_v0_1_command` asserts the full clap command set;
    v0.2 extends its expectation to `V0_1_COMMANDS ∪ V0_2_COMMANDS` and filters hidden names from
    the help-text loop. The test keeps its name.
20. **opencode MCP tool exposure (open).** opencode v2.0.28 loads a project-local MCP server,
    `opencode mcp list`/`/api/mcp` reports it `connected`, and the server receives
    `initialize`/`tools/list`, but the model request's `tools` array contained only the built-ins
    and the model never called the MCP tool. v0.2 therefore writes the config and implements the
    server (§7.7) but does **not** promise that the opencode orchestrator can call VOWP tools until
    the host is re-tested with a real provider and finds the permission/config flag or an upstream
    fix.
    Claude Code's path is proven after the one-time `.mcp.json` approval; `vowp chat` is the
    tool-free fallback. The `AGENTS.md` template and join instructions must not promise the tools
    before that re-test.

### 16.1 Questions for the owner (none block this spec)

- Should the nightly `gc --apply` schedule be enabled by default on every new project, or only after
  the owner sees one dry run? The spec defaults to **enabled**, because disk safety matters more
  than a surprise; `vowp gc --dry-run` is the first thing `setup verify` prints.
- Should a capacity advice item require a typed `apply` (not just a click) in the app? v0.2 records
  the actor and the target history; the UI decision belongs to the MCP server and the orchestrator.

---

## 17. Specification completeness

The specification is complete when it contains, and a reviewer can point to:

1. the full DDL of the global and project schemas, including every new table (§3.2–§3.3);
2. the orchestrator correction: `node_inbox`/`node_ledger`, no cross-file FKs, validation, and the
   migration (§3.1, §3.6, §3.9);
3. Rust signatures for every new trait, type and interface (§6, §7, §13);
4. the exact hook points and their no-op behaviour (§13.4);
5. every new CLI command with arguments and `--json` shapes (§12);
6. the versioned snapshot JSON schema and example (§5);
7. named tests per area (§15);
8. the open assumptions and deviations (§16);
9. the v0.3 notes and the v0.2 index (`README.md`, `../v0.3/README.md`);
10. the `cross-node-sync.md` note that says v0.2's remote execution supersedes the bundle-move
    slice and what remains for v0.3.

This document defines the contract; it does not itself change code, schema files or tests. The
workspace gates (`cargo fmt --all`, clippy, test) pass independently of this document.
