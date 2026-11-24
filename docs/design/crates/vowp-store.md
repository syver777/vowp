# crate `vowp-store`

Depends on `vowp-protocol` only (conventions.md §2–3).

## 1. Purpose

Persistence for one node: (a) the SQLite database `~/.vowp/vowp.db` (schema in `schema.sql`, migrations, typed CRUD for every table),
(b) the file side of sessions — append-only `log.jsonl` with seq/fsync/blob spill, the content-addressed blob store, capsule and
`adapters.json` files, (c) `reindex`, which rebuilds the session index tables from the files. It knows **nothing** about job-state
legality (that is `vowp-graph`), loops, or harnesses.

Invariants this crate guarantees: the log is authoritative for session content; deleting `vowp.db` and running `reindex` loses no
session content; the ledger is append-only; a torn final log line never poisons a log; `seq` has no gaps or duplicates.

## 2. Module layout

```
crates/vowp-store/
  Cargo.toml
  src/lib.rs            // forbid(unsafe), warn(missing_docs); re-exports
  src/error.rs          // StoreError
  src/paths.rs          // NodePaths
  src/schema/global.sql // the embedded global schema (v8)
  src/schema/project.sql // the embedded project schema (v2)
  src/schema/001_init.sql // the earlier byte-identical v0.1 schema used by the v0.1 migration
  src/db.rs              // Store, open/migrate/pragmas/transaction/backup/integrity_check
  src/convert.rs        // private: row <-> protocol type helpers, ts/enum <-> TEXT
  src/agent.rs          // node credentials, enrollment codes, node path maps, NodeCard
  src/repo_surfaces.rs  // surfaces, leases, surface tokens and surface state
  src/repo_nodes.rs     // nodes, projects
  src/repo_jobs.rs      // jobs, edges
  src/repo_runs.rs      // runs, gate_results, workers
  src/repo_sessions.rs  // sessions, session_events, session_natives, capsules, switches
  src/repo_loops.rs     // loops, schedules
  src/repo_ledger.rs    // ledger, cost_totals
  src/repo_misc.rs      // inbox, limits, releases, kv, requests
  src/repo_offload.rs   // job placement, agent assignments, per-node limits
  src/repo_capacity.rs  // host samples, capacity rollups and run usage
  src/blobs.rs          // BlobStore
  src/session_log.rs    // SessionLog, NewEvent, VerifyReport, lockfile, full-pull manifest/verification
  src/session_files.rs  // capsule files, adapters.json
  src/reindex.rs        // reindex
  tests/*.rs
```

The module split follows the four concerns: connection/schema and the node/job/run repositories; the session, loop, ledger and misc repositories; the v0.4 local-node enrollment and offload records; and the blob store, session log, session files and reindex.

## 3. Public API

### 3.1 NodePaths (`paths.rs`) — never reads `$HOME` except `from_env`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePaths { root: std::path::PathBuf }
impl NodePaths {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self;            // pure; no I/O
    /// CLI/supervisor only: `$VOWP_HOME` if set and non-empty, else `$HOME/.vowp`. Err if neither is set.
    pub fn from_env() -> Result<Self, StoreError>;
    pub fn root(&self) -> &std::path::Path;
    pub fn db_path(&self) -> PathBuf;                                    // <root>/vowp.db
    pub fn sessions_dir(&self) -> PathBuf;                               // <root>/sessions
    pub fn session_dir(&self, id: &SessionId) -> PathBuf;                // <root>/sessions/<id>
    pub fn log_path(&self, id: &SessionId) -> PathBuf;                   // .../log.jsonl
    pub fn lock_path(&self, id: &SessionId) -> PathBuf;                  // .../.lock
    pub fn blobs_dir(&self, id: &SessionId) -> PathBuf;                  // .../blobs
    pub fn capsules_dir(&self, id: &SessionId) -> PathBuf;               // .../capsules
    pub fn adapters_json_path(&self, id: &SessionId) -> PathBuf;         // .../adapters.json
    pub fn session_manifest_path(&self, id: &SessionId) -> PathBuf;      // .../manifest.json (full pull only)
    pub fn logs_dir(&self) -> PathBuf;                                   // <root>/logs         (supervisor.log, ...)
    pub fn runs_dir(&self) -> PathBuf;                                   // <root>/runs/<run-id>.jsonl raw harness output mirror
    pub fn run_raw_path(&self, id: &RunId) -> PathBuf;
    pub fn backups_dir(&self) -> PathBuf;                                // <root>/backups
    pub fn tmp_dir(&self) -> PathBuf;                                    // <root>/tmp (atomic-write staging, same filesystem)
    /// mkdir -p for root, sessions, logs, runs, backups, tmp. Idempotent.
    pub fn ensure(&self) -> Result<(), StoreError>;
}
```

### 3.2 Store (`db.rs`)

**Ownership model (binding for core):** every `Store` method, reads *and* writes, takes `&self` (rusqlite's `Connection::execute`/`unchecked_transaction` need only `&Connection`). A `Store` is `Send` but not `Sync`: each thread opens its own (`Deps::open_store`). Core APIs therefore take `&Store` everywhere.

```rust
pub struct Store { /* rusqlite::Connection, Arc<dyn Clock> */ }
pub const GLOBAL_SCHEMA_VERSION: i64 = 8;
pub const PROJECT_SCHEMA_VERSION: i64 = 2;
pub const SCHEMA_VERSION: i64 = 1;   // compatibility alias for the pre-split/v0.1 API

impl Store {
    /// `paths.ensure()`, open `paths.db_path()`, apply pragmas, migrate. Uses `SystemClock`.
    pub fn open(paths: &NodePaths) -> Result<Store, StoreError>;
    /// Existing DB only: no `ensure`, no migration, no `query_only`; missing DB → NotFound.
    pub fn open_writable(paths: &NodePaths, project: Option<&ProjectName>) -> Result<Store, StoreError>;
    pub fn open_path(path: &std::path::Path) -> Result<Store, StoreError>;     // file DB at path, parent dir must exist
    pub fn open_memory() -> Result<Store, StoreError>;                          // :memory:, for tests
    pub fn with_clock(self, clock: std::sync::Arc<dyn Clock>) -> Store;          // tests inject FakeClock (used for created_at/updated_at stamping)
    pub fn clock(&self) -> &dyn Clock;
    pub fn schema_version(&self) -> Result<i64, StoreError>;                     // PRAGMA user_version
    /// Run `f` inside BEGIN IMMEDIATE .. COMMIT; rolls back on Err or panic-free early return.
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, StoreError>) -> Result<T, StoreError>;   // rusqlite `unchecked_transaction` (takes &Connection)
    /// SQLite online backup (rusqlite `backup` feature) to `to` (a file path; parent must exist; overwritten atomically via tmp+rename).
    pub fn backup(&self, to: &std::path::Path) -> Result<(), StoreError>;
    /// `PRAGMA integrity_check` → Ok(()) iff the single row is "ok"; also `PRAGMA foreign_key_check` must be empty.
    pub fn integrity_check(&self) -> Result<(), StoreError>;
}
```

Migration/pragma rules (all in `Store::open*`):

1. Pragmas per connection, in order: `journal_mode=WAL` (skipped for `:memory:`), `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`.
2. Read `user_version` *v*. If `v > GLOBAL_SCHEMA_VERSION` → `StoreError::SchemaTooNew{found,supported}` (never touch the file).
3. If `v == 0` and the DB has no tables: run migration list `[001_init.sql]` from index *v* upward, each inside one transaction; the last statement of
   each migration file sets `PRAGMA user_version = N`. If `v == 0` but tables exist → `StoreError::Corrupt("unversioned database")`.
4. Migrations are `const MIGRATIONS: &[&str] = &[include_str!("schema/001_init.sql")]`. `001_init.sql` is **byte-identical** to
   `docs/design/schema.sql`; it is copied byte for byte. Future schema changes are new files `002_*.sql` and a new design amendment — never edits to 001.
5. Insert `kv('schema_created_at')` and `kv('node_id')` (a fresh ULID) on first creation.
6. `open_writable(paths, project)` is the daemon's narrow write constructor. It requires the
   existing `vowp.db` (missing → `NotFound`), uses read-write flags without create, sets the busy
   timeout and foreign keys, checks `user_version = 8` (newer → `SchemaTooNew`, older →
   `MigrationRequired`), and attaches an existing registered project with the same schema check.
   It never calls `paths.ensure`, `resume_migration` or a schema file, so opening it cannot create
   or migrate a home. It is not a general write-open: `Store::open` remains the CLI/supervisor
   constructor.
7. `Store::open` and `Store::open_global` migrate an existing v2 global file to v3, an
   existing v3 file to v4, an existing v4 file to v5, an existing v5 file to v6, an existing v6
   file to v7 and an existing v7 file to v8 before opening it. `migrate_v02` follows the v0.1 migration's
   staged-swap recipe: it copies the original file to `backups/pre-0.3-<ms>.db`, writes a fresh
   v3 file in `tmp/` (the embedded v8 schema shaped back to the v3 `surfaces` and
   plaintext-`leases` tables, without the later-only tables), copies all
   21 v2 global tables (including `native_events`), verifies per-table counts, integrity and
   foreign keys, and then renames the staged file into place. `nodes` is copied with an explicit
   column list and a `CASE` that maps `is_local` to `kind`; the other tables keep their v2
   definitions and copy column-for-column. The local row (`is_local = 1`) becomes
   `kind = 'home'`; every other v2 node becomes `kind = 'ssh'` with `status = 'active'`.
8. `migrate_v03` copies the original file to `backups/pre-0.4-<ms>.db`, builds a fresh v4 file in
   `tmp/`, copies every v3 table, rebuilds `surfaces` with an explicit column list (adding
   `state = 'attached'` and `revoked_at = NULL`, preserving every row), creates an empty
   `surface_tokens` table with its unique hash index, verifies per-table counts, integrity and
   foreign keys, and swaps the staged file into place. `migration.json` makes the swap
   restartable; projects, session logs and the project schema are untouched. The existing
   v0.1 → v2 `migrate_v01` path is not renumbered: it writes the current embedded schema and
   therefore leaves the home at v6.
9. `migrate_v04` copies the original file to `backups/pre-0.5-<ms>.db`, builds a fresh v5 file in
   `tmp/`, copies every v4 table except `leases`, verifies per-table counts, integrity and
   foreign keys, and swaps the staged file into place. The v5 `leases` table replaces the
   plaintext `token` column with `token_hash` (64 lowercase hex characters) and accepts the
   `agent` surface kind. Lease rows are cleared rather than migrated: a v4 lease token is
   plaintext and every lease is stale within 60 seconds, so surfaces re-acquire after the
   upgrade. Surfaces, `surface_tokens` and every other row are preserved; the migration report
   records the cleared lease count under `tables["global:leases"]` and in `warnings`.
10. `migrate_v05` is the additive v5 → v6 upgrade. It copies the original file to
    `backups/pre-0.6-<ms>.db`, then creates `agent_assignments`, its `(node, state)` index and
    `node_limits` in one `BEGIN IMMEDIATE` transaction. Before the transaction commits it counts
    every existing table before and after, requires both new tables to be empty, and runs
    `integrity_check` and `foreign_key_check`; the report carries `from_version = 5`,
    `to_version = 6` and every `global:<table>` count. A failure rolls the whole transaction back
    and leaves the v5 file readable.
11. Project databases migrate v1 → v2 on the next create/open constructor (`Store::open`,
    `Store::open_with_project` and every explicit `Store::create_project_db` caller). The
    project file is copied to `backups/project-<name>-pre-0.6-<ms>.db` first, then
    `job_placement` is created and `user_version = 2` set in one transaction. Existing rows are
    counted before and after; a failure rolls back and keeps the v1 file readable. A fresh
    project database is created directly at v2. `open_writable` never migrates: a v1 project
    attached there returns `MigrationRequired`.
12. `migrate_v06` is the additive v6 → v7 upgrade. It copies the original file to
    `backups/pre-0.7-<ms>.db`, then creates `capacity_rollups` and its `bucket` index in one
    `BEGIN IMMEDIATE` transaction. Before commit it counts every existing table before and after,
    requires the new table to be empty, and runs `integrity_check` and `foreign_key_check`; the
    report carries `from_version = 6`, `to_version = 7` and every `global:<table>` count. A
    failure rolls the whole transaction back and leaves the v6 file readable.
    `migrate_v06_if_needed` is called from `Store::open_global_unbound` in `db.rs`, after
    `migrate_v05_if_needed`, so `Store::open` and `Store::open_global` reach it through the
    shared path; `Store::open_writable` does not migrate and returns `MigrationRequired` for v6.

13. `migrate_v07` is the v7 → v8 session-ownership upgrade
    (`docs/design/v0.5.1/session-ownership.md` §3.3). SQLite cannot widen a CHECK, so it
    **rebuilds** `surfaces` and `leases`: the original file is copied to `backups/pre-0.8-<ms>.db`;
    foreign keys are switched off outside the transaction (the pragma is a no-op inside one) so
    dropping `surfaces` cannot cascade into `surface_tokens`; inside one `BEGIN IMMEDIATE`
    transaction the new tables are created, every row is copied with its id, the old tables are
    dropped and the new ones renamed, and `leases_heartbeat` plus `leases_expires` are recreated.
    `surfaces` gains `label` (NULL) and the `desktop-ui` kind; `leases` gains `owner_mode`,
    `owner_epoch` and `expires_at` and the `desktop-ui` kind. Every lease keeps its token hash and
    times with `owner_mode = 'interactive'`, `owner_epoch = 0`, no expiry — none is cleared — except
    an old offload run lease (`agent` kind, no surface), which becomes `job` mode with
    `expires_at = heartbeat_at + 600 s`. Before commit every table count is compared and
    `integrity_check` and `foreign_key_check` must pass; a failure rolls back and leaves the v7
    file readable. A v8 home is a no-op (no backup). `migrate_v07_if_needed` runs after
    `migrate_v06_if_needed`; `open_writable` does not migrate. The staged v2–v4 migrations copy
    `surfaces` with an explicit column list because the embedded table has the extra `label`.
14. Ownership API (`repo_surfaces.rs`): `take_ownership(lease, mode, expires_at)` writes the row
    and assigns `owner_epoch = previous + 1` itself (a caller epoch does not exist);
    `take_lease` is the interactive wrapper. `ownership`, `session_owner(session, now)` and
    `list_session_owners(now)` (one query) derive `OwnershipState` at read time and `activity`
    from `sessions.state`; they never read a token. `stale_leases`/`release_stale_lease` select
    only interactive rows and skip the legacy run-lease shape; `expired_job_leases(now)` selects
    `expires_at <= now`; `renew_job_lease` extends a job owner; `set_surface_label` /
    `surface_label` keep the friendly name (default `<kind name> · <node>`, saved on first
    registration, never overwritten by an upsert). A read-only handle on a not-yet-migrated v7
    file reads leases with the v7 defaults.

### 3.3 Filters and small structs

```rust
#[derive(Debug, Clone, Default)]
pub struct JobFilter { pub project: Option<ProjectName>, pub states: Vec<JobState>, pub kind: Option<JobKind>, pub role: Option<Role>,
                       pub id_prefix: Option<String>, pub limit: Option<u32> }
#[derive(Debug, Clone)]
pub struct StateChange { pub note: Option<String>, pub parked_reason: Option<ParkedReason> }
#[derive(Debug, Clone)]
pub struct RunFinish { pub outcome: RunOutcome, pub exit_code: Option<i32>, pub limit_kind: Option<LimitKind>, pub cost: Cost,
                       pub native_session_id: Option<String>, pub detail: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostGroup { Job, Role, Provider, Model, Day }
#[derive(Debug, Clone, Default)]
pub struct CostQuery { pub since: Option<Timestamp>, pub until: Option<Timestamp>, pub project: Option<ProjectName>, pub job_id: Option<JobId> }
#[derive(Debug, Clone, PartialEq)]
pub struct CostRow { pub key: String /* job id | role | provider | model | YYYY-MM-DD (UTC) */, pub usd: f64 /* sum of known usd */,
                     pub receipt_entries: u64, pub price_table_entries: u64,
                     pub unknown_entries: u64 /* cost rows whose usd is NULL (never counted as free) */, pub tokens: Tokens, pub entries: u64 /* receipt + price-table + unknown */ }
#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub struct RequestId(pub String);          // ULID
#[derive(Debug, Clone, PartialEq)]
pub struct Request { pub id: RequestId, pub kind: String /* "retry_job" | "park_job" | "cancel_job" | "pause_loop" | ... (snake_case Request variant names, crates/vowp-core.md §2) */,
                     pub payload: serde_json::Value, pub created_at: Timestamp }
#[derive(Debug, Clone, Default)]
pub struct LedgerFilter { pub kind: Option<LedgerKind>, pub job_id: Option<JobId>, pub since: Option<Timestamp>, pub limit: Option<u32> }
#[derive(Debug, Clone, Default)]
pub struct SessionFilter { pub project: Option<ProjectName>, pub job_id: Option<JobId>, pub state: Option<SessionState>, pub limit: Option<u32> }

// v0.4 local-node records (agent.rs)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub enum EnrollmentKind { Node, Surface }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCredential { pub node: NodeName, pub key_id: String, pub public_key: String,
                            pub created_at: Timestamp, pub last_used_at: Option<Timestamp>,
                            pub revoked_at: Option<Timestamp> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentCode { pub code_hash: String, pub node: NodeName, pub kind: EnrollmentKind,
                            pub surface_id: Option<String>, pub expires_at: Timestamp,
                            pub used_at: Option<Timestamp>, pub created_by: String,
                            pub created_at: Timestamp }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePath { pub node: NodeName, pub project: ProjectName, pub local_root: String,
                      pub worktree_root: String, pub allow: Vec<String>, pub updated_at: Timestamp }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceToken { pub id: String, pub surface_id: SurfaceId, pub token_hash: String,
                          pub prefix: String, pub scopes: Vec<SurfaceScope>,
                          pub created_at: Timestamp, pub expires_at: Option<Timestamp>,
                          pub revoked_at: Option<Timestamp>, pub last_used_at: Option<Timestamp> }

// v0.4 offload records (repo_offload.rs)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentAssignmentState { Queued, Sent, Accepted, Running, Disconnected, Finished, Lost, Cancelled, Rejected }
#[derive(Debug, Clone, PartialEq)]
pub struct AgentAssignment { pub id: String, pub run_id: RunId, pub session_id: SessionId,
                             pub project: ProjectName, pub node: NodeName, pub epoch: u64,
                             pub state: AgentAssignmentState, pub payload: AssignmentPayload,
                             pub created_at: Timestamp, pub claimed_at: Option<Timestamp>,
                             pub expires_at: Option<Timestamp>, pub detail: String }
#[derive(Debug, Clone, PartialEq)]
pub struct NodeLimit { pub provider: String, pub node: NodeName, pub kind: LimitKind,
                       pub resets_at: Option<Timestamp>, pub detail: String,
                       pub since: Timestamp, pub updated_at: Timestamp }
#[derive(Debug, Clone, PartialEq)]
pub struct JobPlacement { pub job: JobId, pub placement: Placement, pub updated_at: Timestamp }
#[derive(Debug, Clone, Default)]
pub struct AssignmentFilter { pub node: Option<NodeName>, pub run_id: Option<RunId>,
                              pub project: Option<ProjectName>, pub states: Vec<AgentAssignmentState>,
                              pub limit: Option<u32> }
```

`AgentAssignment.payload` is the protocol `AssignmentPayload` parsed back from `payload_json`;
the record never carries a lease token, a secret value or an absolute path. `NodeLimit.kind`
uses the protocol `LimitKind`; the table's `unavailable` value is reserved for a future
home-side writer and no store method produces it.

`job_placement` is a **side table**, not a `jobs` column: `Job`, `NewJob`, `JobPatch`, the YAML/JSON
job import and every v0.3 wire client stay byte-compatible because the row itself is unchanged.
`JobPlacement` is the read model returned by `list_job_placements`.

`SurfaceToken` is the durable half of a surface credential: `token_hash` is the lowercase hex
SHA-256 of the token, `prefix` is the short display prefix, and the plaintext token is never
stored. `SurfaceState` (`pending|attached|revoked`) and `SurfaceInfo` live in
`vowp_protocol::agent`; `SurfaceInfo` is the state-bearing read model and carries no token.

`consume_enrollment_code` is one `BEGIN IMMEDIATE` transaction: it updates `used_at` only for an
unused row whose `expires_at > now`, then reads back the winner. Two racing callers therefore get
exactly one `Some`. `node_card` computes presence as online under 30 seconds since
`last_seen_at`, degraded from 30 through 90 seconds, offline after 90; a revoked node is always
`NodePresence::Revoked`. The card never carries `node_paths` or secret names, and `runs` is `0`
until the agent-assignment phase adds a durable run index.

### 3.4 Repositories — all are `impl Store` methods

Naming per conventions.md §6: `x(&id) -> Result<Option<X>>`, `require_x(&id) -> Result<X>` (`NotFound`), `list_xs(..)`.
Every insert stamps `created_at/updated_at` from `self.clock()` unless the type carries its own timestamp.

```rust
// ---- nodes / projects
pub fn upsert_node(&self, n: &Node) -> Result<(), StoreError>;
pub fn node(&self, name: &NodeName) -> Result<Option<Node>, StoreError>;
pub fn require_node(&self, name: &NodeName) -> Result<Node, StoreError>;
pub fn local_node(&self) -> Result<Option<Node>, StoreError>;
pub fn list_nodes(&self) -> Result<Vec<Node>, StoreError>;
pub fn touch_node_seen(&self, name: &NodeName, at: Timestamp) -> Result<(), StoreError>;
pub fn remove_node(&self, name: &NodeName) -> Result<(), StoreError>;          // refuses the local node
pub fn insert_pending_node(&self, node: &NodeName, labels: &[String], at: Timestamp) -> Result<(), StoreError>;
pub fn touch_node(&self, node: &NodeName, at: Timestamp, agent_version: Option<&str>,
                  protocol: Option<&str>, capabilities: &serde_json::Value) -> Result<(), StoreError>;
pub fn set_node_status(&self, node: &NodeName, status: NodeStatus, last_error: &str,
                       at: Timestamp) -> Result<(), StoreError>;
pub fn node_card(&self, node: &NodeName, now: Timestamp) -> Result<Option<NodeCard>, StoreError>;
pub fn list_node_cards(&self, now: Timestamp) -> Result<Vec<NodeCard>, StoreError>;

// ---- node credentials, enrollment codes, path maps (agent.rs)
pub fn node_credential(&self, node: &NodeName, key_id: &str) -> Result<Option<NodeCredential>, StoreError>;
pub fn upsert_node_credential(&self, credential: &NodeCredential) -> Result<(), StoreError>;
pub fn revoke_node_credentials(&self, node: &NodeName, at: Timestamp) -> Result<u64, StoreError>;
pub fn insert_enrollment_code(&self, code: &EnrollmentCode) -> Result<(), StoreError>;
pub fn consume_enrollment_code(&self, code_hash: &str, now: Timestamp) -> Result<Option<EnrollmentCode>, StoreError>;
pub fn purge_expired_enrollment_codes(&self, before: Timestamp) -> Result<u64, StoreError>;
pub fn upsert_node_path(&self, path: &NodePath) -> Result<(), StoreError>;
pub fn list_node_paths(&self, node: &NodeName) -> Result<Vec<NodePath>, StoreError>;

// ---- surfaces / leases / surface tokens (repo_surfaces.rs)
pub fn upsert_surface(&self, s: &Surface) -> Result<(), StoreError>;             // leaves state at 'attached'
pub fn surface(&self, id: &SurfaceId) -> Result<Option<Surface>, StoreError>;
pub fn insert_pending_surface(&self, s: &Surface) -> Result<(), StoreError>;     // state='pending'
pub fn surface_by_id(&self, id: &SurfaceId) -> Result<Option<SurfaceInfo>, StoreError>;  // state-bearing
pub fn set_surface_state(&self, id: &SurfaceId, state: SurfaceState, at: Timestamp) -> Result<(), StoreError>;
pub fn insert_surface_token(&self, t: &SurfaceToken) -> Result<(), StoreError>;  // unique hash index
pub fn surface_token_by_hash(&self, hash: &str) -> Result<Option<SurfaceToken>, StoreError>;  // indexed lookup only
pub fn list_surface_tokens(&self, surface: &SurfaceId) -> Result<Vec<SurfaceToken>, StoreError>;  // oldest first; same-tick rows by insertion (rowid)
pub fn revoke_surface_token(&self, id: &str, at: Timestamp) -> Result<bool, StoreError>;
pub fn touch_surface_token(&self, id: &str, at: Timestamp) -> Result<bool, StoreError>;
pub fn lease(&self, session: &SessionId) -> Result<Option<Lease>, StoreError>;  // Lease::token is the stored hash
pub fn list_leases(&self) -> Result<Vec<Lease>, StoreError>;
pub fn take_lease(&self, lease: &Lease) -> Result<Option<Lease>, StoreError>;  // hashes Lease::token on write
pub fn heartbeat_lease(&self, session: &SessionId, token: &str, at: Timestamp) -> Result<bool, StoreError>;  // hashes the presented token
pub fn release_lease(&self, session: &SessionId, token: &str) -> Result<bool, StoreError>;  // hashes the presented token
pub fn release_lease_hash(&self, session: &SessionId, token_hash: &str) -> Result<bool, StoreError>;  // stored-hash form for a gone token file
pub fn release_stale_lease(&self, session: &SessionId, token_hash: &str, before: Timestamp) -> Result<bool, StoreError>;
pub fn hash_lease_token(token: &str) -> String;  // lowercase hex SHA-256

pub fn upsert_project(&self, p: &Project) -> Result<(), StoreError>;
pub fn project(&self, name: &ProjectName) -> Result<Option<Project>, StoreError>;
pub fn require_project(&self, name: &ProjectName) -> Result<Project, StoreError>;
pub fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

// ---- jobs / edges
/// Inserts with state Pending, defaults per types.md §7 (profile = role name, priority 50, brief/report paths
/// `.vowp/jobs/<id>/{brief,report}.md`, gates = NewJob.gates or `default_gates(kind)`, loop = NewJob.loop_ or default),
/// plus a `needs` edge per `new.needs`. All in one transaction. Errors: Conflict (duplicate id), NotFound (unknown need/project).
pub fn insert_job(&self, project: &ProjectName, new: &NewJob, created_by: &str) -> Result<Job, StoreError>;
pub fn job(&self, id: &JobId) -> Result<Option<Job>, StoreError>;                  // fills `needs` from edges
pub fn require_job(&self, id: &JobId) -> Result<Job, StoreError>;
pub fn list_jobs(&self, f: &JobFilter) -> Result<Vec<Job>, StoreError>;           // ordered by priority DESC, created_at, id
/// Writes `state`, `state_changed_at = now`, `updated_at`, `note` (if given), `parked_reason` (set iff new state == Parked, else cleared).
/// Does NOT check that the transition is legal: `vowp-graph::transition` decides, callers pass only legal ones.
/// Returns the previous state. Errors: NotFound.
pub fn set_job_state(&self, id: &JobId, state: JobState, change: &StateChange) -> Result<JobState, StoreError>;
pub fn patch_job(&self, id: &JobId, patch: &JobPatch) -> Result<Job, StoreError>;   // applies JobPatch incl. add_needs/remove_needs; state untouched
pub fn set_job_counters(&self, id: &JobId, attempts: Option<u32>, continues: Option<u32>, hang_restarts: Option<u32>) -> Result<(), StoreError>;
pub fn set_job_workspace(&self, id: &JobId, branch: Option<&str>, worktree: Option<&str>) -> Result<(), StoreError>;
pub fn touch_job_progress(&self, id: &JobId, at: Timestamp) -> Result<(), StoreError>;   // last_progress_at = max(existing, at)
pub fn set_job_note(&self, id: &JobId, note: &str) -> Result<(), StoreError>;
pub fn delete_job(&self, id: &JobId) -> Result<(), StoreError>;               // only state Pending|Cancelled|Replanned; else Conflict
pub fn add_edge(&self, e: &Edge) -> Result<(), StoreError>;                   // idempotent; unknown ids → NotFound; src==dst → InvalidInput
pub fn remove_edge(&self, e: &Edge) -> Result<bool, StoreError>;
pub fn list_edges(&self, project: Option<&ProjectName>) -> Result<Vec<Edge>, StoreError>;    // for vowp-graph
pub fn edges_of(&self, id: &JobId) -> Result<Vec<Edge>, StoreError>;              // edges where src==id OR dst==id
pub fn needs_of(&self, id: &JobId) -> Result<Vec<JobId>, StoreError>;             // dst of `needs` edges with src==id
pub fn dependents_of(&self, id: &JobId) -> Result<Vec<JobId>, StoreError>;        // src of `needs` edges with dst==id
pub fn count_jobs_by_state(&self, project: Option<&ProjectName>) -> Result<std::collections::BTreeMap<JobState, u32>, StoreError>;

// ---- runs / gate results / workers
pub fn insert_run(&self, run: &Run) -> Result<(), StoreError>;                // run.ended_at None, outcome None (else InvalidInput)
pub fn run(&self, id: &RunId) -> Result<Option<Run>, StoreError>;
pub fn require_run(&self, id: &RunId) -> Result<Run, StoreError>;
pub fn update_run_attach(&self, id: &RunId, pid: Option<u32>, native_session_id: Option<&str>, window: Option<&str>, session_id: Option<&SessionId>) -> Result<(), StoreError>;
/// Sets ended_at = now, outcome, exit_code, limit_kind, cost + token columns, detail (truncated to last 1500 chars).
/// Idempotent only in the sense that a second call returns Conflict (a run ends once).
pub fn finish_run(&self, id: &RunId, fin: &RunFinish) -> Result<(), StoreError>;
pub fn open_runs(&self) -> Result<Vec<Run>, StoreError>;                          // ended_at IS NULL
pub fn runs_of_job(&self, id: &JobId) -> Result<Vec<Run>, StoreError>;            // oldest first
pub fn last_run_of_job(&self, id: &JobId) -> Result<Option<Run>, StoreError>;
pub fn insert_gate_result(&self, r: &GateResult) -> Result<i64, StoreError>;  // returns rowid; r.id ignored
pub fn gate_results_for(&self, job: &JobId, attempt: u32) -> Result<Vec<GateResult>, StoreError>;   // ordered by gate_index, id
pub fn latest_gate_results(&self, job: &JobId) -> Result<Vec<GateResult>, StoreError>;               // newest attempt only
pub fn upsert_worker(&self, w: &Worker) -> Result<(), StoreError>;
pub fn list_workers(&self, node: &NodeName) -> Result<Vec<Worker>, StoreError>;
pub fn ensure_worker_slots(&self, node: &NodeName, kind: WorkerKind, count: u32) -> Result<(), StoreError>;  // creates idle slots 1..=count, removes idle slots above count

// ---- sessions index, events index, natives
pub fn upsert_session(&self, s: &Session) -> Result<(), StoreError>;
pub fn session(&self, id: &SessionId) -> Result<Option<Session>, StoreError>;
pub fn require_session(&self, id: &SessionId) -> Result<Session, StoreError>;
pub fn list_sessions(&self, f: &SessionFilter) -> Result<Vec<Session>, StoreError>;        // updated_at DESC
pub fn set_session_state(&self, id: &SessionId, state: SessionState, executor: Option<&Executor>) -> Result<(), StoreError>;
/// Placement-only update for an orchestrator move: writes `node`, `cwd` and `updated_at`, and leaves
/// `last_seq`, `last_capsule_id`, `log_bytes`, `state`, `executor` and every project/job column alone.
/// Separate from `upsert_session`, which rewrites every column; an unknown id is `NotFound` and inserts no row.
pub fn set_session_location(&self, id: &SessionId, node: &NodeName, cwd: &str) -> Result<(), StoreError>;
pub fn index_event(&self, row: &EventIndexRow) -> Result<(), StoreError>;             // also bumps sessions.last_seq / updated_at / log_bytes in the same tx
pub fn event_rows(&self, id: &SessionId, after_seq: u64, limit: u32) -> Result<Vec<EventIndexRow>, StoreError>;
pub fn event_row_by_id(&self, id: &EventId) -> Result<Option<EventIndexRow>, StoreError>;
pub fn upsert_native(&self, session: &SessionId, harness: HarnessKind, native: &str, node: &NodeName, first_seq: u64) -> Result<(), StoreError>;
pub fn natives_of(&self, session: &SessionId) -> Result<Vec<SessionNative>, StoreError>;
pub fn native_for(&self, session: &SessionId, harness: HarnessKind) -> Result<Option<String>, StoreError>;   // most recent
pub struct SessionNative { pub session_id: SessionId, pub harness: HarnessKind, pub native_session_id: String, pub node: NodeName, pub first_seq: u64, pub created_at: Timestamp }

// ---- capsules / switches
pub fn insert_capsule(&self, c: &Capsule, path: &str) -> Result<(), StoreError>;       // also sets sessions.last_capsule_id
pub fn capsule(&self, id: &CapsuleId) -> Result<Option<Capsule>, StoreError>;
pub fn latest_capsule(&self, session: &SessionId) -> Result<Option<Capsule>, StoreError>;  // highest at_seq, then created_at
pub fn list_capsules(&self, session: &SessionId) -> Result<Vec<Capsule>, StoreError>;      // newest first
pub fn insert_switch(&self, s: &Switch) -> Result<(), StoreError>;
pub fn finish_switch(&self, id: &SwitchId, state: SwitchState, error: Option<&str>, capsule_id: Option<&CapsuleId>) -> Result<(), StoreError>;   // state Completed|Aborted only; capsule_id links the captured capsule (COALESCE keeps an existing link)
pub fn switch(&self, id: &SwitchId) -> Result<Option<Switch>, StoreError>;
pub fn list_switches(&self, session: &SessionId) -> Result<Vec<Switch>, StoreError>;       // oldest first
pub fn open_switches(&self) -> Result<Vec<Switch>, StoreError>;                            // state = Started (crash recovery)

// ---- loops / schedules
pub fn upsert_loop(&self, l: &Loop) -> Result<(), StoreError>;
pub fn loop_(&self, name: &str) -> Result<Option<Loop>, StoreError>;
pub fn list_loops(&self) -> Result<Vec<Loop>, StoreError>;
pub fn set_loop_state(&self, name: &str, state: ActiveState) -> Result<(), StoreError>;
pub fn record_loop_run(&self, name: &str, last_run: Timestamp, next_run: Option<Timestamp>, status: &str, ok: bool) -> Result<(), StoreError>; // ok=false → fail_streak+1 else 0
pub fn insert_schedule(&self, s: &Schedule) -> Result<(), StoreError>;                  // name unique → Conflict
pub fn schedule(&self, id_or_name: &str) -> Result<Option<Schedule>, StoreError>;
pub fn list_schedules(&self) -> Result<Vec<Schedule>, StoreError>;
pub fn due_schedules(&self, now: Timestamp) -> Result<Vec<Schedule>, StoreError>;           // state=Active AND next_run_at <= now, oldest next_run first
pub fn record_schedule_run(&self, id: &ScheduleId, last_run: Timestamp, next_run: Option<Timestamp>, status: &str) -> Result<(), StoreError>;
pub fn set_schedule_state(&self, id: &ScheduleId, state: ActiveState) -> Result<(), StoreError>;
pub fn remove_schedule(&self, id_or_name: &str) -> Result<bool, StoreError>;

// ---- ledger
/// Append-only. Returns rowid. Cost columns come from `e.cost` (usd NULL when None/Unknown).
pub fn append_ledger(&self, e: &LedgerEntry) -> Result<i64, StoreError>;
pub fn ledger_since(&self, f: &LedgerFilter) -> Result<Vec<LedgerEntry>, StoreError>;      // ascending id
/// Sums `kind='cost'` rows grouped by `group` (`Role` groups by `ledger.role`, `Job` by `job_id`, `Provider` by `provider`, `Model` by `model`; rows where the key column is NULL fall in the key `"(none)"`; no joins); ordered by usd DESC then key. `Day` groups by the UTC date of `ts`. `CostRow` counts entries per `cost_source` so the CLI can print `receipt`, `price-table`, `unknown` or `mixed`.
pub fn cost_totals(&self, q: &CostQuery, group: CostGroup) -> Result<Vec<CostRow>, StoreError>;

// ---- inbox / limits / releases
pub fn insert_inbox(&self, i: &InboxItem) -> Result<(), StoreError>;
pub fn inbox_item(&self, id: &InboxId) -> Result<Option<InboxItem>, StoreError>;
pub fn list_inbox(&self, state: Option<InboxState>) -> Result<Vec<InboxItem>, StoreError>;  // oldest first
/// Open → Answered. Conflict if not open. Does not touch the job (core does that).
pub fn answer_inbox(&self, id: &InboxId, answer: &str, by: &str) -> Result<InboxItem, StoreError>;
/// Sets `state='dismissed'`, `answered_by=by`, `answered_at=now`, `answer=NULL` (so the schema CHECK `(state='open') = (answered_at IS NULL)` holds). Conflict if not open.
pub fn dismiss_inbox(&self, id: &InboxId, by: &str) -> Result<(), StoreError>;
pub fn upsert_limit(&self, l: &Limit) -> Result<(), StoreError>;                       // keyed by provider; keeps `since` of an existing row with the same kind
pub fn limit_for(&self, provider: &str) -> Result<Option<Limit>, StoreError>;              // row regardless of expiry
pub fn clear_limit(&self, provider: &str) -> Result<bool, StoreError>;
/// Rows still in force at `now`: `resets_at IS NULL OR resets_at > now`.
pub fn active_limits(&self, now: Timestamp) -> Result<Vec<Limit>, StoreError>;
pub fn record_probe(&self, provider: &str, at: Timestamp, ok: bool) -> Result<(), StoreError>;   // sets last_probe_at; fail streak +1 or reset 0
pub fn insert_release(&self, r: &Release) -> Result<(), StoreError>;
pub fn update_release(&self, r: &Release) -> Result<(), StoreError>;
pub fn release(&self, id: &ReleaseId) -> Result<Option<Release>, StoreError>;
pub fn list_releases(&self, project: &ProjectName, env: Option<&str>) -> Result<Vec<Release>, StoreError>;   // newest first

// ---- kv and requests
pub fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError>;
pub fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError>;
pub fn kv_delete(&self, key: &str) -> Result<bool, StoreError>;
pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, StoreError>;
/// CLI → supervisor requests (decisions.md D2): stores kv key `req:<ULID>` = json `{"kind":"<snake_case>","payload":{…}}` (the exact wire form of `vowp_core::requests::Request`; the store treats kind/payload opaquely).
pub fn push_request(&self, kind: &str, payload: &serde_json::Value) -> Result<RequestId, StoreError>;
/// Atomically (one transaction) reads all `req:*` rows in id order and deletes them.
pub fn take_requests(&self) -> Result<Vec<Request>, StoreError>;

// ---- offload state (repo_offload.rs)
/// Rejects a payload whose assignment/run/session/project/epoch does not match the row
/// (`InvalidInput`) and a duplicate id or a `(run_id, epoch)` that is not higher than the run's
/// newest epoch (`Conflict`). A higher epoch is a new row, never an overwrite.
pub fn insert_assignment(&self, assignment: &AgentAssignment) -> Result<(), StoreError>;
pub fn assignment(&self, id: &str) -> Result<Option<AgentAssignment>, StoreError>;
pub fn assignments_for_node(&self, node: &NodeName) -> Result<Vec<AgentAssignment>, StoreError>;   // oldest first
pub fn assignments_for_run(&self, run: &RunId) -> Result<Vec<AgentAssignment>, StoreError>;        // epoch, id
pub fn list_assignments(&self, filter: &AssignmentFilter) -> Result<Vec<AgentAssignment>, StoreError>;
/// Writes state/claimed_at/detail and returns the updated row; payload and epoch are untouched.
pub fn set_assignment_state(&self, id: &str, state: AgentAssignmentState, claimed_at: Option<Timestamp>, detail: &str)
    -> Result<AgentAssignment, StoreError>;
pub fn upsert_node_limit(&self, limit: &NodeLimit) -> Result<(), StoreError>;   // keeps `since` for the same kind
pub fn node_limits(&self, node: &NodeName) -> Result<Vec<NodeLimit>, StoreError>;                    // by provider
pub fn clear_node_limit(&self, provider: &str, node: &NodeName) -> Result<bool, StoreError>;
/// Persists `placement.to_string()` in the bound project's `project.job_placement` side table.
/// A missing job is `NotFound`; an unknown stored selector reads back as `Corrupt`.
pub fn set_job_placement(&self, job: &JobId, placement: Placement, at: Timestamp) -> Result<(), StoreError>;
pub fn job_placement(&self, job: &JobId) -> Result<Option<Placement>, StoreError>;
pub fn list_job_placements(&self) -> Result<Vec<JobPlacement>, StoreError>;                          // by job id
pub fn clear_job_placement(&self, job: &JobId) -> Result<bool, StoreError>;

// ---- capacity history (repo_capacity.rs)
pub struct CapacityRollup { pub bucket: Timestamp, pub bucket_secs: u64, pub samples: u32,
                            pub mem_available_min_bytes: u64, pub mem_available_avg_bytes: u64,
                            pub mem_available_max_bytes: u64, pub load1_avg: f64,
                            pub disk_free_home_min_bytes: u64, pub source: HistorySource }
/// Aggregates `[bucket, bucket + bucket_secs)` from `host_samples`; a bucket with no raw rows is
/// skipped. A repeated call updates the same row, so a restart cannot duplicate it.
pub fn rollup_host_samples(&self, bucket: Timestamp, bucket_secs: u64) -> Result<bool, StoreError>;
pub fn capacity_rollups_since(&self, since: Timestamp, bucket_secs: u64) -> Result<Vec<CapacityRollup>, StoreError>;
pub fn prune_capacity_rollups(&self, before: Timestamp) -> Result<u64, StoreError>;
```

### 3.5 BlobStore (`blobs.rs`)

```rust
/// Content-addressed store `<dir>/<sha256 hex>`; one per session (`blobs/`) and one used for gate output.
pub struct BlobStore { dir: std::path::PathBuf }
impl BlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self;                       // no I/O
    /// Writes bytes to `<dir>/.tmp-<ulid>` then fsync + `rename` to `<dir>/<sha256>`; creates dir. If the target exists, skips the write (dedupe).
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef, StoreError>;     // BlobRef{sha256, bytes, preview: first 512 chars, lossy UTF-8, newlines kept}
    pub fn get(&self, sha256: &str) -> Result<Vec<u8>, StoreError>;     // NotFound; verifies the digest of what it read → Corrupt on mismatch
    pub fn exists(&self, sha256: &str) -> bool;
    pub fn path_of(&self, sha256: &str) -> PathBuf;                     // rejects non-hex / wrong length with InvalidInput (path traversal guard)
}
/// Lowercase hex SHA-256 of one file, read in bounded chunks (used by full-pull manifests).
pub fn sha256_file(path: &std::path::Path) -> Result<String, StoreError>;
```

### 3.6 SessionLog (`session_log.rs`)

```rust
/// What a caller appends: the session layer chooses actor/source/body; the log assigns id, seq, ts, session_id.
#[derive(Debug, Clone)]
pub struct NewEvent { pub actor: Actor, pub source: EventSource, pub body: EventBody, pub raw: Option<serde_json::Value>, pub ts: Option<Timestamp> }
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport { pub lines: u64, pub last_seq: u64, pub problems: Vec<String> /* "line 12: seq 5 expected 4", "line 30: invalid JSON", ... */, pub torn_tail_bytes: u64 }
impl VerifyReport { pub fn is_ok(&self) -> bool; }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport { pub torn_tail_bytes: u64 /* bytes truncated from a non-terminated final line */, pub events_found: u64 }

pub struct SessionLog { /* File (append), path, session_id, next_seq, bytes, BlobStore, Arc<dyn Clock>, lock guard, dirty: bool */ }
impl SessionLog {
    /// Creates `<session_dir>/{log.jsonl,blobs/,capsules/}` and the lock; fails with Conflict if log.jsonl already exists.
    pub fn create(paths: &NodePaths, id: &SessionId, clock: Arc<dyn Clock>) -> Result<SessionLog, StoreError>;
    /// Opens an existing log for append: takes the lock, scans the file, repairs a torn tail (rule 5), positions at last_seq+1.
    pub fn open(paths: &NodePaths, id: &SessionId, clock: Arc<dyn Clock>) -> Result<SessionLog, StoreError>;
    /// Read-only: no lock, no repair (a torn tail is ignored and reported by `verify`). Used by `session show`, `tail`, `export`, `reindex`.
    pub fn open_read(paths: &NodePaths, id: &SessionId) -> Result<SessionLogReader, StoreError>;
    /// What `open` found/repaired (all zero for `create`). Lets callers and tests observe torn-tail repair.
    pub fn open_report(&self) -> &OpenReport;
    pub fn session_id(&self) -> &SessionId;
    pub fn last_seq(&self) -> u64;
    pub fn byte_len(&self) -> u64;
    /// Spill big tool output (rule 3), assign seq/id/ts, write one line `json\n`, flush to the OS (no fsync). Returns the stored envelope
    /// and its `(byte_offset, byte_len)` so the caller can index it. Rejects: payload not matching `body` type (never happens), seq overflow.
    pub fn append(&self, ev: NewEvent) -> Result<AppendedEvent, StoreError>;
    /// Appends a pre-built envelope verbatim (import/reindex tools). Requires `env.seq == last_seq+1` and `env.session_id == self.id` else `SeqGap`/`SessionMismatch`.
    pub fn append_envelope(&self, env: &EventEnvelope) -> Result<AppendedEvent, StoreError>;
    /// fsync log file (and the directory entry once, after create). No-op if nothing is dirty.
    pub fn sync(&self) -> Result<(), StoreError>;
    pub fn blobs(&self) -> &BlobStore;
}
impl Drop for SessionLog { /* best-effort sync(), then release the lock file */ }
pub struct AppendedEvent { pub envelope: EventEnvelope, pub byte_offset: u64, pub byte_len: u64 }

/// Last complete event of a log, used by the divergence report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogHead { pub last_seq: u64, pub event_type: Option<EventType>, pub ts: Option<Timestamp> }

pub struct SessionLogReader { /* path, BlobStore */ }
impl SessionLogReader {
    pub fn last_seq(&self) -> Result<u64, StoreError>;
    /// Last seq plus the last event's type/timestamp, in one read; empty log => `LogHead::default()`.
    pub fn head(&self) -> Result<LogHead, StoreError>;
    pub fn read_all(&self) -> Result<Vec<EventEnvelope>, StoreError>;
    pub fn read_from(&self, after_seq: u64, limit: Option<usize>) -> Result<Vec<EventEnvelope>, StoreError>;   // seq > after_seq
    pub fn tail(&self, n: usize) -> Result<Vec<EventEnvelope>, StoreError>;                                  // last n, ascending
    pub fn iter(&self) -> Result<EventIter, StoreError>;                       // streaming, with byte offsets
    pub fn read_at(&self, byte_offset: u64, byte_len: u64) -> Result<EventEnvelope, StoreError>;               // via the index
    pub fn verify(&self) -> Result<VerifyReport, StoreError>;
    pub fn blobs(&self) -> &BlobStore;
}
pub struct EventIter { /* BufReader<File>, offset, expected_seq */ }
impl Iterator for EventIter { type Item = Result<IndexedEvent, StoreError>; }
pub struct IndexedEvent { pub envelope: EventEnvelope, pub byte_offset: u64, pub byte_len: u64 }
```

Plus `pub fn index_row(a: &AppendedEvent) -> EventIndexRow` (builds the `session_events` row: harness/model from `source`, `summary` from `EventBody::summary()`).

Full session-directory snapshots (`session pull --full`) use the same module:

```rust
pub const SESSION_DIR_FORMAT: &str = "vowp-session-dir-v1";
pub const SESSION_DIR_MAX_BYTES: u64 = 1_073_741_824;                  // 1 GiB default cap
pub const SESSION_DIR_MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDirFile { pub path: String, pub bytes: u64, pub sha256: String }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDirManifest { pub format: String, pub origin: SessionId, pub files: Vec<SessionDirFile> }
impl SessionDirManifest {
    pub fn validate(&self) -> Result<(), StoreError>;                  // format, safe layout, digests, sorted unique paths, log.jsonl present
    pub fn total_bytes(&self) -> u64;                                  // saturating sum of declared sizes
}
/// Size check first, then hash; the listing is log.jsonl, optional adapters.json, blobs/<sha256>, capsules/*.json.
pub fn session_dir_manifest(paths: &NodePaths, id: &SessionId, max_bytes: u64) -> Result<SessionDirManifest, StoreError>;
pub fn read_session_dir_manifest(path: &std::path::Path) -> Result<SessionDirManifest, StoreError>;
pub fn write_session_dir_manifest(paths: &NodePaths, id: &SessionId, m: &SessionDirManifest) -> Result<PathBuf, StoreError>;
/// Re-hash every listed file, require the manifest set to equal the directory set, verify the log, adapters and capsules.
pub fn verify_session_dir(paths: &NodePaths, id: &SessionId, m: &SessionDirManifest) -> Result<(), StoreError>;
```

### 3.7 Session files and reindex

```rust
// session_files.rs
pub fn write_capsule_file(paths: &NodePaths, c: &Capsule) -> Result<std::path::PathBuf, StoreError>;   // capsules/<at_seq 10-digit zero padded>-<ulid>.json, atomic tmp+rename, pretty JSON
pub fn read_capsule_file(path: &std::path::Path) -> Result<Capsule, StoreError>;                       // parse + Capsule::validate
pub fn list_capsule_files(paths: &NodePaths, id: &SessionId) -> Result<Vec<PathBuf>, StoreError>;      // name order = at_seq order
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptersMap { pub natives: Vec<AdapterMapping> }                                           // append-only list
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterMapping { pub harness: HarnessKind, pub native_session_id: String, pub node: NodeName, pub first_seq: u64, pub created_at: Timestamp }
pub fn read_adapters_json(paths: &NodePaths, id: &SessionId) -> Result<AdaptersMap, StoreError>;      // missing file → empty map
/// Adds a mapping (never overwrites or removes an existing one; same (harness,native) pair is a no-op). Atomic tmp+rename.
pub fn add_adapter_mapping(paths: &NodePaths, id: &SessionId, m: AdapterMapping) -> Result<(), StoreError>;

// reindex.rs
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexReport { pub sessions: u32, pub events: u64, pub capsules: u32, pub natives: u32, pub torn_tails: u32, pub problems: Vec<String> }
/// Rebuild the index tables for one session (Some) or every directory under sessions/ (None): see rule 8.
/// A duplicate event id in one log or across sessions is `StoreError::Conflict` before any row changes.
pub fn reindex(store: &Store, paths: &NodePaths, session: Option<&SessionId>) -> Result<ReindexReport, StoreError>;
```

## 4. Behaviour rules

### 4.1 Log file format and append (rules 1–6)

1. One JSON object per line (`serde_json::to_string`, no pretty printing, no embedded newlines — JSON escapes them), terminated by `\n`, UTF-8.
   Line = serialised `EventEnvelope` (types.md §5.1). `byte_offset` = file offset of the first byte of the line; `byte_len` includes the `\n`.
2. `append` algorithm: (1) if `body` is `ToolResult` with inline `output` longer than `BLOB_SPILL_BYTES` (strictly `> 8192` **bytes**) → `blobs.put(output)`,
   set `output=None`, `output_ref=Some(BlobRef)` ; same for `UserMessage.attachments` inputs already refs; `FileChange.diff_ref` is the caller's job.
   (2) `seq = next_seq`; (3) `ts = ev.ts.unwrap_or(clock.now())`; ts must be ≥ previous event's ts minus nothing (no monotonic check; harness clocks may differ) — only `seq` is ordered;
   (4) build envelope via `EventEnvelope::new` (fresh `EventId`); (5) write line with a single `write_all` to a file opened with `O_APPEND`; (6) `next_seq += 1`, `bytes += len`, `dirty = true`.
   An `io::Error` after a partial write → the log marks itself `poisoned`; further appends return `StoreError::Poisoned` until reopened (reopen repairs the torn tail).
3. **Spill boundary:** an output of exactly 8192 bytes stays inline; 8193 bytes is spilled. The `BlobRef.preview` is the first 512 chars. `raw` is never spilled but if `raw` serialises
   to > 64 KiB it is dropped (`raw: None`) and `replayable` unchanged — raw is display-only.
4. **fsync policy:** `append` never fsyncs. The session layer (vowp-core::session) calls `sync()` after appending `turn.ended`, `session.executor` (detach),
   `switch`, `capsule`, `session.created`, and `error` with kind `Internal`; `Drop` also syncs. After `create`, the parent directory is fsynced once.
5. **Torn tail repair (open for append):** scan from the start counting lines. A final fragment not ending in `\n`, or a final line that is not valid JSON, is a *torn tail*:
   truncate the file to the end of the last good line (`set_len`), fsync, and remember `torn_tail_bytes` (the supervisor logs a warning; `reindex` counts it in `torn_tails`).
   A bad line that is **not** the last (invalid JSON, or seq ≠ previous+1, or `session_id` mismatch) is corruption → `StoreError::Corrupt{path,line,reason}`; the log is never modified.
   `open` is O(file size); sessions with >100 MB logs may take seconds (acceptable v0.1; v0.2 adds a sidecar tail index).
6. **Single-writer lock:** `.lock` created with `OpenOptions::create_new(true)` containing `<pid>\n<hostname-free unix_secs>`. If it exists: parse pid; if the pid is alive
   (`/proc/<pid>` exists on Linux; on macOS `kill -0` via `std::process::Command::new("kill")`) → `StoreError::Locked{pid}`; if dead or unparsable → stale: remove and retry once.
   Drop removes the lock (best effort). `open_read` never locks.

### 4.2 Blobs, capsules, adapters.json

7. Blob names are lowercase hex SHA-256 of the exact bytes; `put` is idempotent; writes are atomic (tmp in the same dir, fsync file, rename, fsync dir). `get` re-hashes.
   Capsule and adapters.json writes use the same atomic recipe through `NodePaths::tmp_dir()`-independent temp files in the target directory (rename must stay on one filesystem).
   A full-pull directory may also carry `manifest.json` (format `vowp-session-dir-v1`). It lists only
   the content files with their byte sizes and lowercase sha256 digests; `reindex` ignores it. A
   snapshot is written into a staging directory, `verify_session_dir` re-hashes every listed file
   and requires the manifest set to equal the actual set (plus a clean log and valid capsules), and
   only then is the directory renamed to `sessions/<id>/`. A file that is missing, extra, resized
   or changed is a `Corrupt` error.

### 4.3 Reindex

8. `reindex(store, paths, session)` algorithm, per session directory `<id>` whose name parses as `SessionId` (others are skipped with a problem line):
   1. `SessionLogReader::iter()` over `log.jsonl`; a torn tail is ignored and counted; corruption (rule 5) → the session is skipped, problem recorded, other sessions continue.
   2. Check event ids **before touching the index**: an id repeated in this log, or one already owned by another session in `session_events`, returns `StoreError::Conflict` (`duplicate event id` / `already belongs to session`); no rows are changed. `session_events.id` is unique across the whole index, so this is a conflict, not a problem line.
   3. In **one transaction** per session: delete that session's `session_events` and `session_natives` rows (**not** `capsules`, `sessions`, `runs`, `switches`, `ledger`: `switches.capsule_id` has `ON DELETE SET NULL`, so deleting capsule rows would silently clear every switch→capsule link);
      then for each event insert the `session_events` row; track `last_seq`, `log_bytes`, `updated_at = last event ts`.
   4. Rebuild the `sessions` row **if missing** (db deleted): from `session.created` (title, cwd, project, job_id, role, parent) and the latest `session.executor`
      (`executor` string, `node`), `state = idle` (or `closed` if a trailing `note` text `closed`; v0.1 always `idle`). If the row exists, only `last_seq`, `log_bytes`, `updated_at`, `executor` are refreshed.
   5. `session_natives` rebuilt from every `session.executor` attach event carrying `native_session_id` (`first_seq` = that event's seq), then merged with `adapters.json` (union).
   6. `capsules` **upserted** from `capsules/*.json` with `INSERT … ON CONFLICT(id) DO UPDATE` (never `INSERT OR REPLACE`/delete, which would fire the FK action); invalid files → problem line, skipped; capsule rows whose file is missing are left untouched and reported as a problem; `sessions.last_capsule_id` = latest by `at_seq`.
   7. `switches` rows missing from the DB are re-created from `switch` events (`state = completed`, `to_node` = the `to.node` or this node); existing rows untouched (their `capsule_id` is preserved; a re-created row's `capsule_id` comes from the `switch` event's `capsule_id`, set only if that capsule row exists).
   Result: after `rm vowp.db && Store::open && reindex(None)`, `sessions`, `session_events`, `session_natives`, `capsules` (and lineage `switches`) equal the pre-deletion rows
   except fields that only the DB held (`sessions.state` of in-flight sessions, `job_id` when the creating event lacks it). **Content is never lost.**
9. `reindex` is idempotent: running twice yields identical tables (test `reindex_is_idempotent`).

### 4.4 General

10. All SQL is parameterised; no string-built SQL with user data. Enum ↔ TEXT via the serde names (single helper in `convert.rs`, so schema CHECKs and enums cannot drift;
    `schema_matches_enums` proves it).
11. `Store` is `Send` but not `Sync`; each thread/process opens its own. WAL + `busy_timeout=5000` make short concurrent writers safe; a `SQLITE_BUSY` after the timeout → `StoreError::Busy`.
12. Timestamps are stored/parsed as canonical RFC 3339 ms strings (`Timestamp::as_rfc3339`); a malformed value read from the DB → `StoreError::Corrupt`.
13. JSON columns are written with `serde_json::to_string` of the protocol type and read back through `from_str` of that type; a parse failure → `StoreError::Corrupt`.
14. `Store::transaction` nests by savepoint only one level; repo methods that need atomicity (insert_job, index_event, take_requests, answer_inbox) open their own transaction and must not be called inside a `transaction` closure
    that already holds one unless documented (they detect `!conn.is_autocommit()` and use `SAVEPOINT` instead).
15. `surface_tokens` reads order by `created_at, rowid`. A rotation and its replacement can share
    one clock tick, and insertion order is the only stable tie-breaker; without it the newest token
    could sort before the old one and the prefix/overlap view would be wrong.
16. Capacity retention is enforced by the sampler, not by a trigger: raw `host_samples` older than
    48 hours and `capacity_rollups` older than 30 days are deleted, and the bucket containing
    `now` is always newer than either cutoff. The rollup writer runs for the previous completed
    5-minute bucket and upserts `(bucket, bucket_secs)`, so a repeated tick is idempotent.

## 5. Error enum

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found: {what} {id}")]                       NotFound { what: &'static str, id: String },
    #[error("conflict: {0}")]                                Conflict(String),
    #[error("invalid input: {0}")]                           InvalidInput(String),
    #[error("database schema version {found} is newer than supported {supported}")] SchemaTooNew { found: i64, supported: i64 },
    #[error("corrupt data in {path:?} (line {line:?}): {reason}")] Corrupt { path: String, line: Option<u64>, reason: String },
    #[error("sequence gap in session {session}: expected {expected}, got {got}")] SeqGap { session: String, expected: u64, got: u64 },
    #[error("event belongs to session {got}, log is {expected}")] SessionMismatch { expected: String, got: String },
    #[error("session log is locked by pid {pid}")]           Locked { pid: u32 },
    #[error("session log is poisoned by an earlier write error; reopen it")] Poisoned,
    #[error("database busy")]                                Busy,
    #[error("sqlite: {0}")]                                  Sqlite(rusqlite::Error),
    #[error("io at {path}: {source}")]                       Io { path: String, source: std::io::Error },
    #[error("json: {0}")]                                    Json(#[from] serde_json::Error),
    #[error("protocol: {0}")]                                Protocol(#[from] vowp_protocol::ProtocolError),
    #[error("home directory not determinable (set VOWP_HOME)")] NoHome,
}
impl StoreError { pub fn kind(&self) -> ErrorKind; }     // NotFound→NotFound, Conflict/Locked→Conflict, Sqlite/Busy/SchemaTooNew→Db, Corrupt/Json→Parse, Io→Io, ...
impl From<rusqlite::Error> for StoreError { /* maps SQLITE_BUSY → Busy, UNIQUE/PK → Conflict, FK → InvalidInput, trigger RAISE → Conflict */ }
```

## 6. Named tests (hermetic: `tempfile::tempdir()`, `Store::open_memory()` or a temp file DB; no HOME, no network)

| Test | Asserts |
|---|---|
| `schema_file_is_identical_to_design` | `include_str!("../src/schema/001_init.sql") == include_str!("../../../docs/design/schema.sql")` (paths relative to `crates/vowp-store/tests/`); the file must exist in the repo layout — **no skip, no fallback**: a missing file is a compile error, a difference is a test failure |
| `open_creates_current_schema_versions` | fresh temp DB: `global_schema_version()==7`, a bound project reports `project_schema_version()==2`, the pre-split `schema_version()` compat accessor stays 1, tables listed in each schema.sql exist, WAL mode (`PRAGMA journal_mode` = `wal`), `foreign_keys` = 1 |
| `open_refuses_newer_schema` | set `PRAGMA user_version = 99` on a temp DB file, `Store::open_path` → `SchemaTooNew{found:99,supported:7}`, file bytes unchanged |
| `open_is_idempotent` | open twice on the same file: no error, `node_id` kv unchanged |
| `open_writable_answers_and_never_creates_or_migrates` | `open_writable` opens an existing home, answers an inbox item and reads it back; a missing `vowp.db` is `NotFound` and its directory is not created; an empty zero-byte file is `MigrationRequired` and stays untouched |
| `migration_v2_to_v3_preserves_nodes_projects_and_sessions` | a v2 fixture with nodes, a project and a session migrates to v3; counts, `kv('node_id')`, project and session rows survive; the backup still reports v2 |
| `migration_v3_to_v4_preserves_surfaces_and_rows` | a v3 fixture with a project, session, credential, path map, surface, lease and native event migrates to v4; every v3 table count is unchanged, the surface keeps its holder/metadata with `state='attached'`, `surface_tokens` starts empty, the backup still reports v3, and the rebuilt kind check accepts `agent`; opening the migrated home then runs the v4→v5 upgrade and clears the ephemeral lease |
| `migration_v4_to_v5_clears_leases_and_preserves_surfaces` | a v4 fixture with a surface, surface token, session and plaintext lease migrates to v5; every non-lease table count is unchanged, the lease count goes from 1 to 0 with a warning, the backup still reports v4 and keeps the plaintext row, and the migrated `leases` declares `token_hash`, not `token` |
| `surface_token_round_trips_and_hash_is_unique` | insert, lookup by hash and list round-trip scopes/timestamps; a second row with the same hash is `Conflict` |
| `surface_token_revoke_and_expiry_are_recorded` | `expires_at` round-trips, `touch_surface_token` records `last_used_at`, the first revoke records `revoked_at`, a second revoke is false |
| `surface_state_round_trips_pending_attached_revoked` | a pending surface reads back as `pending`, `attached` clears `revoked_at`, `revoked` records it, and a missing id is `NotFound` |
| `surface_token_schema_has_no_plaintext_token` | `surface_tokens` declares `token_hash`/`prefix` and no plaintext token column; a stored row never contains the plaintext token |
| `local_node_is_home_after_migration` | `is_local = 1` becomes `kind = 'home'`; other nodes become `kind = 'ssh'` with `status = 'active'` |
| `enrollment_code_is_single_use` | one sequential redemption, then `None`; two racing connections produce exactly one winner |
| `enrollment_code_expires` | a code is invalid at its expiry instant; `purge_expired_enrollment_codes` removes it |
| `node_credential_round_trips_and_revokes` | insert/update round-trip preserves `created_at`, revocation is idempotent and returns the changed count; an unknown node fails its foreign key |
| `node_path_upsert_is_idempotent` | two upserts leave one row per `(node, project)` with the newest map |
| `node_status_and_presence_persist` | pending/active status, labels, version, protocol and capacity round-trip; presence is online/degraded/offline at 10/60/120 seconds; `last_error` persists |
| `revoked_node_is_not_active` | a revoked node stays `Revoked` and never becomes `Active` after a late heartbeat |
| `schema_matches_enums` | parse every `CHECK (col IN ('a','b',...))` list from `schema.sql` and compare to the serde names of the matching Rust enum for: `jobs.kind/role/state`, `edges.kind`, `runs.role/purpose/outcome/limit_kind/cost_source`, `gate_results.gate_kind`, `session_events.type/actor`, `sessions.state/role`, `capsules.trigger`, `switches.reason/state`, `loops.kind/state`, `schedules.spec_kind/action_kind/missed_policy/state`, `ledger.kind`, `inbox.kind/state`, `limits.kind`, `releases.state`, `workers.kind/state` — sets must be equal |
| `job_insert_defaults_and_needs_edges` | `insert_job` with minimal `NewJob` yields state Pending, priority 50, brief path `.vowp/jobs/<id>/brief.md`, default gates for kind, and `job.needs` equals the `needs` edges |
| `job_insert_duplicate_and_unknown_need` | duplicate id → `Conflict`; unknown need → `NotFound`; no partial rows left (transaction) |
| `job_state_roundtrip_all_states` | `set_job_state` to each of the 13 `JobState`s and read back; `Parked` keeps `parked_reason`, any other state clears it; `state_changed_at` equals injected `FakeClock` time |
| `list_jobs_filter_and_order` | states filter, role filter, ordering priority DESC then created_at |
| `edges_add_remove_and_queries` | `needs_of`, `dependents_of`, `edges_of`, self-edge rejected, idempotent add, cascade delete when a job is deleted |
| `delete_job_only_when_unstarted` | Running job → `Conflict`; Pending → ok |
| `run_lifecycle_and_open_runs` | insert, `open_runs` lists it, `finish_run` sets outcome+cost columns, second `finish_run` → `Conflict`, `ended_at/outcome` CHECK enforced |
| `gate_results_ordering_and_latest` | attempts 0 and 1: `gate_results_for(job,1)` ordered by `gate_index`; `latest_gate_results` returns attempt 1 only; evidence JSON round-trips (`Evidence::Cmd`) |
| `workers_slots_ensure` | `ensure_worker_slots(node,Worker,4)` creates 1..=4 idle; shrinking to 2 removes idle 3–4 but keeps a busy slot 4 |
| `session_index_and_natives` | `upsert_session`, `index_event` bumps `last_seq`, `native_for` returns most recent, duplicate `(session,harness,native)` is a no-op |
| `capsule_insert_sets_last_capsule` | `insert_capsule` updates `sessions.last_capsule_id`; `latest_capsule` picks highest `at_seq` |
| `switch_lifecycle_and_open_switches` | `insert_switch` (Started) shows in `open_switches`; `finish_switch(..., Completed, None, None)` removes it; `finish_switch(..., Started, ...)` → `InvalidInput`; a missing id → `NotFound` |
| `finish_switch_links_capsule` | `finish_switch(..., Completed, None, Some(capsule))` links the capsule and sets `ended_at`; `finish_switch(..., Aborted, Some(error), None)` records the error and leaves an unlinked row unlinked |
| `due_schedules_selects_active_due_only` | paused, future and due rows: only active-and-due returned, ordered by `next_run_at` |
| `loop_fail_streak` | `record_loop_run(ok=false)` twice → streak 2; `ok=true` resets to 0 |
| `ledger_is_append_only` | `UPDATE ledger` and `DELETE FROM ledger` via raw SQL both fail with the trigger message; `append_ledger` returns increasing ids |
| `cost_totals_group_by_each_dimension` | seed 6 cost rows (2 jobs, 2 roles, 2 providers, 2 models, 2 days, one with `usd NULL`); for each `CostGroup` assert exact sums, `receipt_entries`/`price_table_entries`/`unknown_entries` (sum = `entries`), token totals, ordering |
| `cost_totals_respects_filters` | `since/until/project/job_id` narrow the result |
| `inbox_answer_flow` | open → `answer_inbox` ok; second answer → `Conflict`; CHECK `(state='open') = (answered_at IS NULL)` holds; `dismiss_inbox` on an open item yields `state='dismissed'` with `answered_at` set and `answer IS NULL`; dismissing an answered item → `Conflict` |
| `limits_active_and_clear` | rows with past `resets_at`, future, and `None`: `active_limits(now)` = future + `None`; `clear_limit` removes; `upsert_limit` preserves `since` for same kind |
| `record_probe_streak` | failing probes increment `probe_fail_streak`, success resets |
| `releases_roundtrip` | insert, `update_release` state transitions, `list_releases` newest first, `rolled_back_by` self-reference FK |
| `kv_and_requests_queue` | `push_request` ×3 then `take_requests` returns 3 in push order and a second `take_requests` is empty |
| `fk_cascade_project_delete` | deleting a project (raw SQL) cascades jobs/edges; `sessions.project` set NULL |
| `backup_produces_openable_copy` | write rows, `backup(to)`, `Store::open_path(to)` sees the same rows, `integrity_check` ok |
| `blob_spill_boundary` | tool result of exactly 8192 bytes stays inline; 8193 bytes → `output=None`, `output_ref.bytes==8193`, `blobs/<sha>` exists and `get` returns the original bytes |
| `blob_put_is_atomic_and_deduped` | two `put`s of equal bytes → same sha, one file; `get` after tampering the file → `Corrupt`; `path_of("../x")` → `InvalidInput` |
| `log_append_assigns_seq_and_ts` | 5 appends with `FakeClock` → seq 1..5, ts from clock, `EventId`s unique and increasing, line offsets add up to file length |
| `log_rejects_gap_and_duplicate_envelope` | `append_envelope` with seq 3 when last is 1 → `SeqGap{expected:2,got:3}`; seq 1 again → `SeqGap` |
| `log_torn_tail_recovery` | write 4 events, truncate the file mid-line 4 (cut N bytes), `open` → `last_seq==3`, file truncated to the end of line 3, next `append` gets seq 4, `verify()` is ok and `log.open_report().torn_tail_bytes` equals the number of bytes truncated |
| `log_corrupt_middle_is_error_not_repair` | flip a byte inside line 2 of 4: `open` → `Corrupt{line:Some(2)}`, file unchanged; `open_read(...).verify()` lists the problem |
| `log_survives_reopen_proptest` | proptest: random sequences (≤ 40) of events with random sync points and random reopen points; after the final reopen `read_all` equals the model: same events, strictly consecutive seqs from 1, no duplicates, no reordering |
| `log_fsync_markers` | `sync()` on a clean log is a no-op (dirty flag false); appending sets dirty; `sync` clears it (observable via a test-only counter feature `#[cfg(test)] fn sync_count()`) |
| `log_lock_blocks_second_writer` | `open` twice from two threads/handles: second → `Locked`; after drop the lock file is gone and a new `open` works; a lock file containing a dead pid (e.g. 999999) is treated as stale and replaced |
| `log_reader_tail_and_read_from` | `tail(2)`, `read_from(3, Some(2))`, `read_at(offset,len)` return the right envelopes; `iter` yields increasing offsets |
| `log_big_tool_result_roundtrip` | 100 KB output appended and read back: `output_ref` resolves via `blobs().get` to the original text |
| `capsule_file_roundtrip` | `write_capsule_file` → `read_capsule_file` equals input; file name starts with zero-padded `at_seq`; invalid capsule JSON → `Corrupt`/`Protocol` error |
| `adapters_json_is_append_only` | add 2 mappings, re-adding the first is a no-op, file order preserved; missing file reads as empty |
| `reindex_after_deleting_db_restores_index` | build a store+3 sessions with logs, capsules and adapters.json; snapshot index tables; delete the DB file; `Store::open` + `reindex(None)`; `sessions`, `session_events`, `session_natives`, `capsules` rows equal the snapshot (ignoring `state`); in a second case the DB is NOT deleted and `switches` rows with `capsule_id` set still have it after `reindex` (`reindex_preserves_switch_capsule_links`) |
| `reindex_restores_user_session_role` | a session created with role `user` (from `session.created`) keeps `sessions.role='user'` after `rm vowp.db` + reindex |
| `reindex_is_idempotent` | run twice; tables identical |
| `reindex_single_session_leaves_others` | modifying one log then `reindex(Some(id))` updates only that session's rows |
| `reindex_skips_corrupt_session` | one session with a corrupt middle line: reported in `problems`, others indexed |
| `reindex_rejects_an_event_id_owned_by_another_session` | the second session's log reuses an indexed event id → `StoreError::Conflict` (`already belongs to session`), no row or events for the rejected session |
| `reindex_rejects_a_duplicate_event_id_in_one_log` | one log repeats an event id → `StoreError::Conflict` (`duplicate event id`), no indexed events |
| `paths_layout` | `NodePaths::new("/x")` produces every path exactly as listed in §3.1 (no I/O); `from_env` honours `VOWP_HOME` (test sets the env var inside a mutex-guarded section) and errors with neither set |
| `sha256_file_matches_the_blob_name_written_by_put` | a file written by `BlobStore::put` hashes to the blob name returned by `put` |
| `session_dir_manifest_verifies_and_detects_corruption` | a log+blob manifest verifies, `manifest.json` round-trips, a same-size changed blob is `Corrupt` (`sha256 mismatch`) and an extra blob file is `Corrupt` (`not listed`) |
| `session_dir_manifest_refuses_a_directory_over_the_size_cap` | `session_dir_manifest(..., max_bytes=1)` refuses a larger session with `InvalidInput` naming the limit |
| `session_dir_manifest_rejects_traversal_and_reserved_paths` | `..`, absolute paths, `manifest.json`, `.lock`, `import.json`, non-digest blob names, non-JSON capsule names and `summary.md` are refused |
| `migration_v5_to_v6_adds_agent_assignments_and_node_limits_preserving_rows` | a v5 fixture with nodes, a project, a session and kv rows migrates additively; every existing count is unchanged, both new tables start empty, the index exists, the backup stays v5, and the migrated home opens at v6 |
| `migration_v5_to_v6_failure_leaves_the_v5_file_intact` | a conflicting pre-existing `node_limits` makes the migration fail; the version stays 5, no partial `agent_assignments` row/table remains and existing rows stay readable |
| `capacity_rollups_migrates_v6_to_v7_with_a_backup` | a v6 fixture with host samples migrates additively to v7; `capacity_rollups` and its bucket index exist and start empty, every existing count is unchanged, the backup stays v6, and the v6 file still opens at v6 |
| `capacity_rollups_migration_failure_leaves_the_v6_file_readable` | a conflicting pre-existing `capacity_rollups` makes the migration fail; the version stays 6, no partial row/table remains and existing rows stay readable |
| `v7_home_migrates_to_v8_preserving_surfaces_tokens_and_leases` | a real v7 home keeps every surface, token and lease with their ids and hashes; `desktop-ui` is accepted; a job lease without an expiry is rejected |
| `v7_agent_run_lease_becomes_job_mode_with_an_expiry` | an old `agent`/no-surface lease becomes `job` with `heartbeat + 600 s` |
| `v8_migration_is_idempotent_and_writes_a_backup_first` | opening migrates once with one untouched v7 `pre-0.8` backup; a second run is a no-op |
| `capacity_rollups_table_has_the_v7_columns_and_index` | the v7 table has every documented column, the `source` CHECK and the `capacity_rollups_bucket` index |
| `rollup_host_samples_aggregates_the_bucket_and_skips_empty_ones` | one completed bucket aggregates min/avg/max, load and disk free, and a bucket with no raw rows writes nothing |
| `rollup_host_samples_is_idempotent_across_a_repeated_write` | a repeated rollup for one bucket updates the same row and recomputes `samples` instead of duplicating it |
| `prune_capacity_rollups_keeps_thirty_days` | rows older than 30 days are deleted, rows inside the window stay, and the bucket containing `now` is never removed |
| `migration_v1_to_v2_adds_job_placement_and_preserves_jobs` | a v1 project fixture with two jobs gets `job_placement` and `user_version = 2` on the next `open_with_project`; both jobs survive and the backup is the v1 file |
| `job_placement_round_trips_home_any_and_node` | `home`, `any`, `node:<name>` and `node:<name>+home` round-trip through the side table, list returns the newest value and clear removes it |
| `job_placement_cascades_with_its_job` | deleting a pending job removes its `job_placement` row through the foreign key |
| `assignment_round_trips_payload_and_state` | insert/read preserves the parsed payload; `set_assignment_state` writes state/claimed_at/detail; node, run and filtered lists find the row |
| `assignment_rejects_a_mismatched_payload_and_duplicate_run_epoch` | each mismatched run/session/project/epoch is `InvalidInput` with no row written; a duplicate or lower epoch is `Conflict`; a higher epoch is a separate row |
| `assignment_payload_json_contains_no_lease_token_or_absolute_path` | the stored JSON has no lease token/secret value field and every string is project-relative |
| `node_limit_upserts_per_provider_and_node` | `(provider, node)` is the key; two nodes stay separate, a kind change resets `since`, a same-kind update keeps it, and clear removes one row |
| `node_limit_clears_and_cascades_with_the_node` | an explicit clear removes one provider and deleting the node cascades its remaining limits |

## 7. Out of scope / v0.2

HTTP/daemon access, replication/libSQL, FTS search over events, sidecar tail index for huge logs, log signing/hash chain, automatic backup scheduling (that is a `vowp schedule` command item in core), blob garbage collection.

The v0.4 store covers enrollment, the surface-token/state layer, the v5 lease hash, the
Phase 2 offload state (`agent_assignments`, `node_limits`, `job_placement`) and the Phase 4
`capacity_rollups` history. The host sampler and the rollup writer are `vowp-core` work; this
crate stores and reads the state. Token generation and constant-time verification are
core/gateway work; this crate only persists the hash, prefix and scopes (for surface tokens) or
the hash (for leases).
