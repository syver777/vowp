-- VOWP v0.1 node database schema  (one SQLite file per node: ~/.vowp/vowp.db)
--
-- Conventions
--   * Timestamps: TEXT, RFC 3339 UTC with milliseconds, e.g. '2026-11-08T19:25:47.123Z'. They sort lexicographically.
--   * Ids: ULID (26-char Crockford base32, TEXT) for runs, sessions, events, capsules, switches, inbox items,
--     releases and schedules. Jobs use human slugs ('example-job'); nodes/projects/loops use names.
--     Autoincrement INTEGER ids only for append-only high-volume tables (gate_results, ledger).
--   * Booleans: INTEGER 0/1 with CHECK. Enums: TEXT with CHECK listing the allowed values (kept in sync with the
--     Rust enums in vowp-protocol; the `schema_matches_enums` test in vowp-store enforces it).
--   * JSON columns: TEXT with CHECK(json_valid(..)); their shapes are the serde types in types.md.
--   * Money: REAL USD. Never NULL for "free": unknown cost is cost_usd NULL + cost_source 'unknown'.
--   * Single source of truth: session CONTENT lives in sessions/<id>/log.jsonl; `sessions`, `session_events`,
--     `session_natives` are an index and are rebuilt by `vowp reindex`. Everything else here is authoritative.
--
-- Migrations: PRAGMA user_version is the schema version. This file is migration 1 (user_version = 1).
-- vowp-store embeds it with include_str!, runs each migration N+1 inside one transaction when
-- user_version = N, and refuses to open a database whose user_version is greater than it knows.
-- Connection pragmas (set by Store::open on every connection, not stored in the file):
--   journal_mode=WAL, synchronous=NORMAL, foreign_keys=ON, busy_timeout=5000.

PRAGMA foreign_keys = ON;

-- ------------------------------------------------------------------ meta
CREATE TABLE kv (                       -- small key/value state: 'paused' = '1', 'node_id', 'schema_created_at', ...
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;

-- ------------------------------------------------------------------ nodes, projects
CREATE TABLE nodes (                    -- this node (is_local=1) and known remote nodes
  name         TEXT PRIMARY KEY,
  ssh_target   TEXT,                    -- 'user@host' or an ssh config alias; NULL for the local node
  is_local     INTEGER NOT NULL DEFAULT 0 CHECK (is_local IN (0,1)),
  capabilities TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(capabilities)),  -- {harnesses:{claude-code:{version,ok}},tmux:..,git:..}
  last_seen_at TEXT,
  created_at   TEXT NOT NULL,
  CHECK ((is_local = 1 AND ssh_target IS NULL) OR is_local = 0)
) STRICT;
CREATE UNIQUE INDEX nodes_one_local ON nodes(is_local) WHERE is_local = 1;

CREATE TABLE projects (                 -- projects homed on this node
  name           TEXT PRIMARY KEY,
  path           TEXT NOT NULL,         -- absolute path of the main checkout
  remote_url     TEXT,                  -- git remote (informational)
  default_branch TEXT NOT NULL DEFAULT 'main',
  config_json    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_json)),  -- resolved ProjectConfig snapshot (types.md)
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL
) STRICT;

-- ------------------------------------------------------------------ the graph
CREATE TABLE jobs (
  id               TEXT PRIMARY KEY,    -- slug: [a-z0-9][a-z0-9-]{0,63}
  project          TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
  title            TEXT NOT NULL,
  kind             TEXT NOT NULL CHECK (kind IN
                     ('plan','spec','build','test','verify','fix','merge','docs','research','promote','release','decision')),
  role             TEXT NOT NULL CHECK (role IN ('planner','keeper','worker','verifier')),
  profile          TEXT NOT NULL,       -- name in vowp.toml [profiles.*]
  state            TEXT NOT NULL DEFAULT 'pending' CHECK (state IN
                     ('pending','ready','running','stalled','gating','reviewing','fixing','merging',
                      'done','failed','parked','replanned','cancelled')),
  priority         INTEGER NOT NULL DEFAULT 50,          -- higher dispatches first
  owns_json        TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(owns_json)),    -- ["crates/x/**"]
  reads_json       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(reads_json)),
  brief_path       TEXT NOT NULL,       -- relative to the project checkout: .vowp/jobs/<id>/brief.md
  report_path      TEXT NOT NULL,       -- .vowp/jobs/<id>/report.md
  gates_json       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(gates_json)),   -- [Gate]
  loop_json        TEXT NOT NULL DEFAULT '{"until":"gates","max_attempts":3,"on_exhausted":"escalate"}'
                     CHECK (json_valid(loop_json)),                              -- JobLoop
  branch           TEXT,                -- job/<id> once a worktree exists
  worktree         TEXT,                -- absolute path of the job worktree
  attempts         INTEGER NOT NULL DEFAULT 0,  -- fix rounds started (bounded by loop_json.max_attempts)
  continues        INTEGER NOT NULL DEFAULT 0,  -- automatic "continue" nudges in the current attempt (bound 2)
  hang_restarts    INTEGER NOT NULL DEFAULT 0,
  parked_reason    TEXT,                -- why state='parked' (credits, auth, question, provider_blocked, manual)
  note             TEXT NOT NULL DEFAULT '',     -- last human-readable status line
  created_by       TEXT NOT NULL DEFAULT 'human',-- 'human' | 'planner' | 'keeper' | 'supervisor' | 'schedule:<id>'
  last_progress_at TEXT,                -- last new event or commit seen (stall detection, loops.md §L1)
  state_changed_at TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL
) STRICT;
CREATE INDEX jobs_project_state ON jobs(project, state);
CREATE INDEX jobs_dispatch      ON jobs(state, priority DESC, created_at);

CREATE TABLE edges (                    -- (src, dst, kind) reads "src <kind> dst":
  src        TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,   --   src needs dst | src verifies dst |
  dst        TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,   --   src fixes dst | src escalates dst | src blocks dst
  kind       TEXT NOT NULL CHECK (kind IN ('needs','verifies','fixes','escalates','blocks')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (src, dst, kind),
  CHECK (src <> dst)
) STRICT;
CREATE INDEX edges_dst ON edges(dst, kind);

-- ------------------------------------------------------------------ runs, gates, workers
CREATE TABLE sessions (                 -- INDEX of sessions/<id>/log.jsonl (rebuildable)
  id                 TEXT PRIMARY KEY,  -- canonical session ULID
  project            TEXT REFERENCES projects(name) ON DELETE SET NULL,
  job_id             TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  role               TEXT CHECK (role IS NULL OR role IN ('planner','keeper','worker','verifier','user')),
  title              TEXT NOT NULL DEFAULT '',
  parent_session_id  TEXT,              -- lineage across export/import (may reference another node: no FK)
  node               TEXT NOT NULL,     -- node that homes the session now
  cwd                TEXT NOT NULL,
  state              TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle','running','blocked','switching','closed')),
  executor           TEXT,              -- current Executor string 'harness:model@effort' ; NULL before first attach
  last_seq           INTEGER NOT NULL DEFAULT 0,
  last_capsule_id    TEXT,
  log_bytes          INTEGER NOT NULL DEFAULT 0,   -- size of log.jsonl at last index
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL
) STRICT;
CREATE INDEX sessions_job ON sessions(job_id);
CREATE INDEX sessions_project_updated ON sessions(project, updated_at DESC);

CREATE TABLE runs (                     -- every harness attempt (one adapter process)
  id                TEXT PRIMARY KEY,   -- ULID
  job_id            TEXT REFERENCES jobs(id) ON DELETE SET NULL,   -- NULL for planner/keeper/probe/capsule runs
  session_id        TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  role              TEXT NOT NULL CHECK (role IN ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),
  purpose           TEXT NOT NULL DEFAULT 'start' CHECK (purpose IN ('start','continue','fix','verify','plan','keep','capsule','probe','merge','gate')),
  profile           TEXT,
  rung              INTEGER NOT NULL DEFAULT 0,        -- index in the profile ladder used
  harness           TEXT NOT NULL,                     -- 'claude-code' | 'opencode' | 'cli' | 'vowp' (shell/gate runs)
  model             TEXT,
  effort            TEXT,                              -- '@effort' (claude) or '#variant' (opencode), without the sigil
  provider          TEXT,                              -- limits key: 'claude', 'deepseek', ...
  node              TEXT NOT NULL,
  native_session_id TEXT,
  pid               INTEGER,
  window            TEXT,                              -- tmux window name 'job-<id>'
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
  detail            TEXT NOT NULL DEFAULT '',          -- last 1500 chars of error/limit text (never secrets)
  CHECK ((ended_at IS NULL) = (outcome IS NULL))
) STRICT;
CREATE INDEX runs_job     ON runs(job_id, started_at);
CREATE INDEX runs_open    ON runs(ended_at) WHERE ended_at IS NULL;
CREATE INDEX runs_session ON runs(session_id);

CREATE TABLE gate_results (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id       TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  run_id       TEXT REFERENCES runs(id) ON DELETE SET NULL,   -- the run that produced the thing being gated
  attempt      INTEGER NOT NULL DEFAULT 0,                    -- jobs.attempts at evaluation time
  gate_index   INTEGER NOT NULL,                              -- position in jobs.gates_json (-1 = repo-wide merge check)
  gate_kind    TEXT NOT NULL CHECK (gate_kind IN
                 ('report','cmd','owns_respected','pushed','review','script','deploy','secret_scan')),
  gate_json    TEXT NOT NULL CHECK (json_valid(gate_json)),   -- the Gate evaluated (copy)
  passed       INTEGER NOT NULL CHECK (passed IN (0,1)),
  evidence_json TEXT NOT NULL CHECK (json_valid(evidence_json)),  -- Evidence (types.md)
  output_blob  TEXT,                                           -- sha256 of full command output in a blob dir, if spilled
  duration_ms  INTEGER NOT NULL DEFAULT 0,
  ts           TEXT NOT NULL
) STRICT;
CREATE INDEX gate_results_job ON gate_results(job_id, attempt, gate_index);

CREATE TABLE workers (                  -- concurrent worker slots (also tmux windows)
  node       TEXT NOT NULL,
  slot       INTEGER NOT NULL,          -- 1..=max_workers
  kind       TEXT NOT NULL DEFAULT 'worker' CHECK (kind IN ('worker','verifier')),
  state      TEXT NOT NULL DEFAULT 'idle' CHECK (state IN ('idle','busy','draining')),
  run_id     TEXT REFERENCES runs(id) ON DELETE SET NULL,
  job_id     TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  window     TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (node, kind, slot),
  CHECK ((state = 'idle') = (run_id IS NULL))
) STRICT;

-- ------------------------------------------------------------------ sessions: index, natives, capsules, switches
CREATE TABLE session_events (           -- index of log.jsonl lines (rebuildable); content stays in the file
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL CHECK (seq >= 1),
  id          TEXT NOT NULL,            -- event ULID
  ts          TEXT NOT NULL,
  type        TEXT NOT NULL CHECK (type IN
                ('session.created','session.executor','turn.started','turn.ended','user.message','assistant.message',
                 'assistant.reasoning','tool.call','tool.result','file.change','plan','limit','cost','switch',
                 'capsule','error','note')),
  actor       TEXT NOT NULL CHECK (actor IN ('user','assistant','system','tool')),
  harness     TEXT NOT NULL,
  model       TEXT,
  byte_offset INTEGER NOT NULL,         -- offset of the line in log.jsonl
  byte_len    INTEGER NOT NULL,
  summary     TEXT NOT NULL DEFAULT '', -- <=200 chars, for listings/search
  PRIMARY KEY (session_id, seq)
) STRICT;
CREATE UNIQUE INDEX session_events_id ON session_events(id);
CREATE INDEX session_events_type ON session_events(session_id, type, seq);

CREATE TABLE session_natives (          -- harness-side ids per session (mirror of adapters.json; rebuilt from session.executor events)
  session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  harness           TEXT NOT NULL,
  native_session_id TEXT NOT NULL,
  node              TEXT NOT NULL,
  first_seq         INTEGER NOT NULL,
  created_at        TEXT NOT NULL,
  PRIMARY KEY (session_id, harness, native_session_id)
) STRICT;

CREATE TABLE capsules (
  id           TEXT PRIMARY KEY,        -- 'cap_<ULID>'
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  at_seq       INTEGER NOT NULL,
  version      INTEGER NOT NULL DEFAULT 1,           -- capsule schema version
  trigger      TEXT NOT NULL CHECK (trigger IN ('switch','manual','turn_end','limit','context','export','job_end')),
  author       TEXT,                    -- executor string of the model that wrote it
  path         TEXT NOT NULL,           -- sessions/<id>/capsules/<at_seq:010>-<ulid>.json (at_seq zero-padded to 10 digits; authoritative copy)
  body_json    TEXT NOT NULL CHECK (json_valid(body_json)),   -- cached copy of the file
  created_at   TEXT NOT NULL
) STRICT;
CREATE INDEX capsules_session ON capsules(session_id, at_seq DESC);

CREATE TABLE switches (                 -- lineage of executor changes
  id            TEXT PRIMARY KEY,       -- ULID
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  from_executor TEXT,                   -- Executor string; NULL for the first attach
  to_executor   TEXT NOT NULL,
  from_node     TEXT,
  to_node       TEXT NOT NULL,
  capsule_id    TEXT REFERENCES capsules(id) ON DELETE SET NULL,
  reason        TEXT NOT NULL CHECK (reason IN ('user','limit','crash','context','failover','probe_back')),
  state         TEXT NOT NULL DEFAULT 'started' CHECK (state IN ('started','completed','aborted')),
  error         TEXT,
  started_at    TEXT NOT NULL,
  ended_at      TEXT,
  at_seq        INTEGER NOT NULL        -- session seq when the switch began
) STRICT;
CREATE INDEX switches_session ON switches(session_id, started_at);

-- ------------------------------------------------------------------ loops and schedules
CREATE TABLE loops (                    -- loop definitions + state (L1..L9); `vowp loops` lists them
  name         TEXT PRIMARY KEY,        -- 'l1-supervisor', 'l4-planner', ...
  kind         TEXT NOT NULL CHECK (kind IN ('L1','L2','L3','L4','L5','L6','L7','L8','L9')),
  project      TEXT REFERENCES projects(name) ON DELETE CASCADE,  -- NULL = node-wide
  interval_secs INTEGER NOT NULL CHECK (interval_secs >= 0),      -- tick or heartbeat; 0 = event-driven only
  state        TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','paused')),
  bounds_json  TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(bounds_json)),  -- LoopBounds
  last_run_at  TEXT,
  next_run_at  TEXT,
  last_status  TEXT NOT NULL DEFAULT '' ,
  fail_streak  INTEGER NOT NULL DEFAULT 0,
  updated_at   TEXT NOT NULL
) STRICT;

CREATE TABLE schedules (                -- L9: cron / interval triggers
  id            TEXT PRIMARY KEY,       -- ULID
  project       TEXT REFERENCES projects(name) ON DELETE CASCADE,
  name          TEXT NOT NULL UNIQUE,
  spec_kind     TEXT NOT NULL CHECK (spec_kind IN ('cron','interval')),
  spec          TEXT NOT NULL,          -- 5/6-field cron string (UTC unless tz set) or 'every 30m'
  tz            TEXT NOT NULL DEFAULT 'UTC',   -- IANA name; v0.1 supports 'UTC' and fixed offsets '+02:00'
  action_kind   TEXT NOT NULL CHECK (action_kind IN ('job_template','loop','command')),
  action_json   TEXT NOT NULL CHECK (json_valid(action_json)),   -- ScheduleAction
  missed_policy TEXT NOT NULL DEFAULT 'run_once' CHECK (missed_policy IN ('run_once','skip')),
  state         TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','paused')),
  last_run_at   TEXT,
  next_run_at   TEXT,
  last_status   TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL
) STRICT;
CREATE INDEX schedules_due ON schedules(state, next_run_at);

-- ------------------------------------------------------------------ ledger, inbox, limits, releases
CREATE TABLE ledger (                   -- append-only; never UPDATEd or DELETEd (trigger below)
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN
                ('dispatch','gate','merge','cost','switch','limit','promote','rollback','inbox','schedule','state','note')),
  project     TEXT,
  job_id      TEXT,
  run_id      TEXT,
  session_id  TEXT,
  role        TEXT CHECK (role IS NULL OR role IN ('planner','keeper','worker','verifier','capsule','probe','merge','gate')),  -- RunRole of the run (cost grouping by role)
  provider    TEXT,
  harness     TEXT,
  model       TEXT,
  usd         REAL,                     -- NULL unless kind='cost'
  tokens_in   INTEGER NOT NULL DEFAULT 0,
  tokens_out  INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning  INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  cost_source TEXT CHECK (cost_source IS NULL OR cost_source IN ('receipt','price-table','unknown')),
  summary     TEXT NOT NULL,            -- one line, human readable
  detail_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(detail_json))
) STRICT;
CREATE INDEX ledger_ts   ON ledger(ts);
CREATE INDEX ledger_job  ON ledger(job_id, ts);
CREATE INDEX ledger_kind ON ledger(kind, ts);
CREATE TRIGGER ledger_no_update BEFORE UPDATE ON ledger BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;
CREATE TRIGGER ledger_no_delete BEFORE DELETE ON ledger BEGIN SELECT RAISE(ABORT, 'ledger is append-only'); END;

CREATE TABLE inbox (                    -- L7: questions and approvals for humans
  id          TEXT PRIMARY KEY,         -- ULID
  project     TEXT REFERENCES projects(name) ON DELETE CASCADE,
  job_id      TEXT REFERENCES jobs(id) ON DELETE SET NULL,   -- the parked job waiting on the answer, if any
  kind        TEXT NOT NULL CHECK (kind IN ('question','approval','decision','notice')),
  title       TEXT NOT NULL,
  body        TEXT NOT NULL DEFAULT '',
  options_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(options_json)),  -- suggested answers, e.g. ["approve","reject"]
  state       TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','answered','dismissed')),
  answer      TEXT,
  answered_by TEXT,
  created_at  TEXT NOT NULL,
  answered_at TEXT,
  CHECK ((state = 'open') = (answered_at IS NULL))
) STRICT;
CREATE INDEX inbox_open ON inbox(state, created_at);

CREATE TABLE limits (                   -- per provider: why it is unavailable and until when
  provider      TEXT PRIMARY KEY,       -- 'claude', 'deepseek', ...
  harness       TEXT,
  kind          TEXT NOT NULL CHECK (kind IN ('quota','credits','rate','context','auth','unavailable')),
  resets_at     TEXT,                   -- NULL = until a human resolves it (credits, auth)
  detail        TEXT NOT NULL DEFAULT '',
  since         TEXT NOT NULL,
  last_probe_at TEXT,
  probe_fail_streak INTEGER NOT NULL DEFAULT 0,
  concurrency_cap   INTEGER,            -- temporary max concurrent sessions after 429s (NULL = use config)
  cap_until     TEXT,
  updated_at    TEXT NOT NULL
) STRICT;

CREATE TABLE releases (                 -- Work -> Production promotions
  id             TEXT PRIMARY KEY,      -- ULID
  project        TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
  env            TEXT NOT NULL,         -- target environment name from vowp.toml [env.*]
  change_ref     TEXT NOT NULL,         -- git ref / commit / job id being promoted
  from_commit    TEXT,                  -- what was live before (serving snapshot commit or digest)
  to_commit      TEXT,
  state          TEXT NOT NULL DEFAULT 'planned' CHECK (state IN
                   ('planned','guarded','banked','approved','deploying','probing','live','failed','rolled_back','refused')),
  guards_json    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(guards_json)),   -- [GuardResult]
  snapshot_path  TEXT,                  -- serving bank location
  rollback_cmd   TEXT,                  -- exact command that restores the snapshot
  probe_json     TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(probe_json)),
  approved_by    TEXT,
  rolled_back_by TEXT REFERENCES releases(id),   -- id of the rollback release record, if any
  note           TEXT NOT NULL DEFAULT '',
  created_at     TEXT NOT NULL,
  finished_at    TEXT
) STRICT;
CREATE INDEX releases_project ON releases(project, env, created_at DESC);

PRAGMA user_version = 1;
