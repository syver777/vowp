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
