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
