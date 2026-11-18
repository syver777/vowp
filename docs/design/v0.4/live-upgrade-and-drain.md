# v0.4 — Live Upgrade, Drain Mode and Selective Subsystem Restart

- **Status:** Accepted design, implemented by `v4-c5-live-upgrade-drain`
- **Date:** 2026-11-14
- **Scope:** `vowp-store`, `vowp-core`, `vowp-cli`

## 1. Problem statement

During multi-project operation on a shared VOWP home (several concurrent repositories on one
home), two framework continuity gaps surfaced whenever `vowp` itself was rebuilt with a newer
schema or binary:

1. **Installing a newer `vowp` binary breaks CLI commands while an older supervisor runs.**
   `Store::open_global_unbound` and `Store::create_project_db` immediately attempt an in-place
   schema migration when `PRAGMA user_version` is older than the compiled `GLOBAL_SCHEMA_VERSION`
   or `PROJECT_SCHEMA_VERSION`. Because `migrate.rs` rightly calls `refuse_live_supervisor`, every
   normal CLI call (`vowp status`, `vowp fleet`, `vowp job`, `vowp inbox`) from the newly installed
   binary fails with `supervisor pid <pid> is running; stop it before migrating` until the entire
   supervisor is stopped.
2. **Restarting or upgrading requires stopping the entire node (`vowp down`), which interrupts
   every active run across all projects.**
   Today `vowp down` writes `<home>/stop`, cancels the foreground loop, and after a 6-second grace
   window terminates all direct adapter and gate child processes. There is no drain mode to finish
   in-flight runs before exiting or re-executing, and no way to restart individual subsystems
   (such as the `vowp-daemon` web window, a single project's loop state, or a drained supervisor
   binary handoff) without taking down unrelated projects.

## 2. Design goals

1. **Zero CLI breakage on binary install:** A newer `vowp` binary on disk must be able to open an
   older split-home database (`user_version >= 2`) while a live supervisor holds `supervisor.lock`
   without triggering an automatic migration or failing standard commands.
2. **Cooperative node drain (`vowp down --drain` and `vowp up --replace`):** The supervisor can
   enter a draining state where it stops admitting new runs (worker, verifier, planner, keeper,
   schedules) while continuing to harvest finished runs, evaluate gates, merge completed branches,
   and update the snapshot/heartbeat until active runs and gate tasks reach zero.
3. **Rolling binary & schema upgrade (`vowp up --replace`):** Once drained (immediately if zero
   runs are active, or as soon as in-flight runs finish), the supervisor releases or hands over the
   lock, runs the pending schema migration safely with zero active children, and spawns/re-execs the
   new supervisor binary automatically.
4. **Selective subsystem restart without full-system stop:**
   - **Per-project drain/reload:** `vowp project pause <name>` already ticks a project in
     `ProjectTickMode::Maintenance` (finish running work, start nothing new). Extending project
     maintenance and reload so one project can be drained, migrated or reset while all other
     projects continue normal dispatch.
   - **Daemon (`vowp-daemon` web window) and orchestrator host restart:** Allow restarting or
     reloading the web-window child process and individual orchestrator app hosts via a supervisor
     request without stopping `NodeSupervisor` or any running job.

## 3. Architecture & contract

### 3.1 Deferred migration while `supervisor.lock` is live (`vowp-store`)

- In `Store::from_global_connection` and `Store::create_project_db`:
  - Check whether `user_version < GLOBAL_SCHEMA_VERSION` (or `< PROJECT_SCHEMA_VERSION`).
  - If no live supervisor holds `supervisor.lock` (or if the current process *is* the lock holder
    during startup/upgrade), run the in-place migration as today.
  - If a *different* live supervisor PID holds `supervisor.lock` and the on-disk schema is a valid
    split-home version (`2 <= user_version < GLOBAL_SCHEMA_VERSION`, or `1 <= project_version <
    PROJECT_SCHEMA_VERSION`), **defer the migration**: open the connection in backward-compatible
    mode without mutating the schema. The live PID is read from `supervisor.lock` and must still
    name a running process; the migrating supervisor's own PID (written before `NodeSupervisor`
    opens the store) is not treated as another supervisor.
  - Read-only handles (`Store::open_read_only`) accept every split-home version because they
    never migrate. On the write path, the core tables (`projects`, `jobs`, `runs`, `sessions`,
    `inbox`, `kv`, `capacity`) keep their columns across v2–v6. A project v1 file lacks only the
    v2 `job_placement` table, so ordinary job/run/session reads work; operations that strictly
    need `job_placement` return an SQLite error until the handover migrates the file.
  - [`StoreError::MigrationDeferred`] names the running PID, the found version and the target
    version for callers that need a descriptive deferral error.

### 3.2 Node drain state & rolling handoff (`vowp-core` & `vowp-cli`)

- **Drain state (`<home>/drain`):**
  - `vowp down --drain` writes `<home>/drain`, enqueues a `wake` request and waits for the
    supervisor to release the lock. `vowp up --replace` writes the same marker, wakes the
    supervisor and waits for the old process to exit before running the normal `up` path.
  - When `<home>/drain` is present, `NodeSupervisor::tick` forces every active project into
    `ProjectTickMode::Maintenance` (harvest, gates, merge queue, inbox, snapshot, heartbeat — no
    new worker/verifier/planner/keeper dispatch and no schedule triggers). A drain that appears
    mid-tick is also observed by the dispatch gate, so one slow pass cannot admit a job.
  - At the end of each tick in drain mode, `NodeSupervisor` checks whether total active runner
    threads and gate/merge tasks across all projects equal zero and whether any finished run is
    still awaiting harvest. It exits only when both are true.
  - When active count reaches zero:
    - For `vowp down --drain`: the supervisor exits cleanly (`0` active children killed). The CLI
      wrapper stops the optional `web_window` child, records the stopped marker and removes the
      lock; the drain marker is removed on the exit path.
    - For `vowp up --replace`: the old supervisor stops the same way. The waiting `up --replace`
      process then runs `Store::open_global`/`Store::open` with no active children, applying any
      pending schema migration, and starts the current on-disk `vowp` binary (`up --foreground` when
      `--foreground` was given, otherwise the normal detached child). The new supervisor resumes
      `ProjectTickMode::Active` dispatch.
- **Selective subsystem restart (`Request` variants):**
  - `Request::RestartWebWindow`: the node supervisor records the request; the foreground CLI loop
    sees it through `NodeSupervisor::take_web_window_restart`, stops and restarts the `vowp-daemon`
    child, refreshes `<home>/web-window`, and never touches a runner pool or project supervisor.
  - `Request::ReloadProject { project }`: the node supervisor queues the project and re-opens its
    `ProjectSupervisor` and store connection once that project's active work is zero. The reload
    runs the same startup reconciliation and cross-db repair as a supervisor restart, without
    interrupting other projects.

## 4. Job breakdown (`v4-c5-live-upgrade-drain`)

To avoid ownership conflicts with the in-flight `v4-asif-*` and `v4-local-2*` waves (`cli.rs`,
`cmd/mod.rs`, `vowp-core/src/lib.rs`), `v4-c5-live-upgrade-drain` depends on `v4-asif-cli` and
`v4-local-2h-agent-harness` and owns:

- `crates/vowp-store/src/db.rs`, `crates/vowp-store/src/migrate.rs`,
  `crates/vowp-store/src/error.rs`, `crates/vowp-store/tests/migration.rs`
- `crates/vowp-core/src/loops/supervisor.rs`, `crates/vowp-core/src/requests.rs`,
  `crates/vowp-core/tests/node_supervisor.rs`
- `crates/vowp-cli/src/cli.rs`, `crates/vowp-cli/src/signals.rs`, `crates/vowp-cli/src/cmd/up.rs`,
  `crates/vowp-cli/tests/up.rs`
- `docs/design/v0.4/live-upgrade-and-drain.md`, `docs/design/cli.md`,
  `docs/design/crates/vowp-core.md`

### Required hermetic tests

1. `cli_opens_older_schema_while_live_supervisor_holds_lock` — proves a newer `Store::open` /
   `open_global` / `open_with_project` does not fail with `refuse_live_supervisor` when a live
   supervisor PID holds the lock on an older split-home schema, and migrates automatically once
   the lock is released.
2. `drain_mode_finishes_in_flight_run_without_admitting_ready_jobs` — proves a `<home>/drain`
   tick runs in maintenance mode until the active run finishes and harvests, never dispatches a
   waiting `ready` job, and exits cleanly without killing the child.
3. `down_drain_and_up_replace_handoff` — proves `down --drain` lets an in-flight run finish and
   `up --replace` drains, releases the lock and resumes dispatch from a new supervisor process.
4. `selective_project_reload_keeps_other_projects_running` — proves reloading one project re-opens
   only that supervisor and leaves another project's state and generation untouched, and that a
   `restart_web_window` request reaches the CLI wrapper without recreating a project.
5. `restart_web_window_request_replaces_only_the_daemon_child` — proves the foreground CLI stops
   and starts the `vowp-daemon` child for a `restart_web_window` request while the node supervisor
   keeps running.
