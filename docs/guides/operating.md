# Operating a node

A node runs one supervisor per project home. The supervisor is a plain process that ticks every 60
seconds, reconciles the database and the session logs, and starts or stops work. Everything it knows
survives a crash; `vowp up` again reconciles and continues.

> **Status: current.** The supervisor core, `vowp doctor` and every lifecycle, status, board, loop,
> backup, reindex and cost command on this page are implemented.

## Start and stop

```sh
vowp up                          # ensure tmux windows, detach the supervisor
vowp up --foreground             # run the loop in this process (what systemd runs)
vowp up --no-tmux                # skip the initial layout; jobs still use tmux windows
vowp up --once                   # exactly one tick, then exit
vowp down                        # stop, wait up to 30 s
vowp down --kill-windows         # also close this project's tmux windows
vowp down --force                # send SIGTERM after the wait
```

`vowp up` exits 4 if a supervisor is already running for the project. `vowp down` writes
`~/.vowp/stop`; a run that is interrupted is finalised as `stalled` and re-queued, not failed. Windows
are left open for inspection unless `--kill-windows` is given. `--force` is for a supervisor that ignored
the stop request, not the normal path.

The supervisor uses a lock file `~/.vowp/supervisor.lock` containing its pid. A stale lock left by a
killed process is detected and replaced; do not delete the database to "fix" a lock.

## Status and the board

```sh
vowp status
vowp status --json
vowp status --check             # exit 1 when the heartbeat is older than 3 min
vowp board                      # refresh every 2 s
vowp board --once
vowp board --interval 5
```

`status` reports the supervisor pid, uptime, last tick age, jobs by state, busy/total slots, the current
planner and keeper executors, active limits with reset times, cost today and the open inbox count. It
exits 0 whether or not a supervisor is running; use the `--json` `running` field for scripts.
`status --check` is the watchdog contract: exit 1 means the heartbeat is stale.

`board` recomputes the same summary from the database and redraws it. `~/.vowp/board.json` is a
convenience copy written every tick for external readers; it is never the source of truth.

## Logs and files

```sh
tail -n 100 ~/.vowp/logs/supervisor.log
tail -F ~/.vowp/logs/supervisor.log
```

Each line is `<timestamp> <LEVEL> <target> <message>`, with targets such as `l1`, `l2`, `l3`, `merge`,
`gates`, `session`, `adapter`, `tmux` and `cli`. Levels are `ERROR`, `WARN`, `INFO` and `DEBUG`. The
file is append-only and grows until you rotate it; v0.1 has no built-in rotation, so use your normal
log rotation for `~/.vowp/logs/supervisor.log`.

| Path | What it is |
|---|---|
| `~/.vowp/heartbeat` | Unix seconds, rewritten atomically every tick. |
| `~/.vowp/board.json` | Last board snapshot. |
| `~/.vowp/supervisor.lock` | Pid of the running supervisor. |
| `~/.vowp/logs/supervisor.log` | Structured loop log. |
| `~/.vowp/sessions/<id>/log.jsonl` | Authoritative session content. |
| `~/.vowp/runs/<runId>.jsonl` | Raw harness stdout, debug only. |
| `~/.vowp/inbox/<id>.md` | Human-readable copy of each inbox item. |
| `<project>/.vowp/checkpoints/` | Planner checkpoint summaries every 120 minutes. |

Never log or paste prompts, environment values or secret-looking strings. Run and limit details are
redacted before storage, but session logs are still sensitive: they can contain prompts, tool output and
file contents.

## Loops

```sh
vowp loops
vowp loops pause l4-planner
vowp loops resume l4-planner
```

The rows are seeded by `vowp init` and ensured on the first `vowp up` (startup is idempotent):
`l1-supervisor` (60 s), `l2-worker`, `l3-verify`, `l4-planner` (1800 s), `l5-keeper` (1200 s),
`l6-failover`, `l7-inbox`, `l8-watchdog` (300 s) and `l9-schedules` (60 s). The table shows state,
interval, last/next run, status and failure streak.

`l1-supervisor` cannot be paused on its own; use `vowp down`. Pausing a loop skips its step but does not
cancel work already running. A loop that repeats the same action three times without new evidence stops
and escalates rather than spinning.

## Schedules

Schedules are rows too; see the [schedules guide](schedules.md). They can create a job, trigger a loop
or run a command. Missed runs while the node was down either run once (`run_once`) or are skipped
(`skip`); they never replay N times.

## Backups and reindex

```sh
vowp backup
vowp backup --to /backup/vowp-before-upgrade.db
vowp reindex
vowp reindex --session <id>
```

`vowp backup` takes an online SQLite backup to `~/.vowp/backups/vowp-<timestamp>.db` (or the named
file) and prints the path and size. It is safe while the supervisor runs.

`vowp reindex` rebuilds the `sessions`, `session_events`, `session_natives` and `capsules` index rows
from `sessions/*/log.jsonl`. It is safe while the supervisor runs and prints counts. It does not rebuild
jobs, gates or decisions: those live in the database and in the project files. Restore a backup instead
of hand-editing `vowp.db`.

## Watchdog

The design uses two layers: a systemd user unit with `Restart=always` (or an equivalent process
supervisor) and a five-minute cron check that reads `~/.vowp/heartbeat` and restarts the service when it
is missing or older than 180 seconds. The repository ships
[`scripts/install.sh`](../../scripts/README.md), which installs the binary, the unit and the cron check;
a source build does not install them until you run it.

Alternatively, run `vowp up --foreground` under your own process supervisor, or keep a terminal open
with `vowp up` and check `vowp status --check` from cron. The watchdog itself never decides job state; it
only restarts the supervisor, which then reconciles.

## Upgrade the binary

1. `vowp down`.
2. Build or install the new binary over the old one.
3. `vowp doctor` to check the schema version and configuration.
4. `vowp up`.

Database migrations are forward-only and keyed by `PRAGMA user_version`. A database written by a newer
binary is refused (`SchemaTooNew`) rather than guessed at. Back up before an upgrade; rollback of the
binary is safe only if the old binary understands the schema, so restore the backup if it does not.

## Costs

```sh
vowp cost
vowp cost --by provider --since 7d
vowp cost --by job --job example-graph
```

Rows show runs, USD, tokens, cache reads and the source (`receipt`, `price-table`, `unknown` or
`mixed`). Unknown costs are shown as `?` and counted separately, never as zero. Cost comes from the
harness receipt when available; otherwise a labelled price-table estimate, otherwise unknown.

## Routine checks

Once a day:

- `vowp status` — supervisor healthy, no stuck `running` job, limits resetting.
- `vowp inbox ls` — questions and approvals waiting.
- `vowp cost --since 1d` — spend and unknown-cost count.
- `vowp backup` — keep a copy outside the node.

Once a week:

- Read `~/.vowp/logs/supervisor.log` for repeated `WARN`/`ERROR` targets.
- Check disk space; runs and blobs grow with tool output.
- Review `open_items` and `questions_for_owner` in finished reports.

Next: [sessions](sessions.md) for switching model or harness, or
[troubleshooting](../troubleshooting.md) when the board is not moving.
