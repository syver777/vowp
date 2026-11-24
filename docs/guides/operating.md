# Operating a home

A home is one VPS with one supervisor for every active project. The supervisor is a plain process
that ticks every 60 seconds, reconciles the databases and session logs, dispatches work and writes a
snapshot. Everything it knows survives a crash; `vowp up` again reconciles and continues. Control
machines read that snapshot and send writes back to the home.

> **Status: current.** The supervisor, fleet, capacity, project, loop, schedule, backup, gc, secret
> and migration commands on this page are implemented.

## Start and stop

```sh
vowp up                          # ensure tmux windows, detach the supervisor
vowp up --foreground             # run the loop in this process (what systemd runs)
vowp up --no-tmux                # skip the initial layout; jobs still use tmux windows
vowp up --once                   # exactly one home tick, then exit
vowp down                        # stop, wait up to 30 s
vowp down --kill-windows         # also close this home's tmux windows
vowp down --force                # send SIGTERM after the wait
```

`vowp up` exits 4 if a supervisor is already running for the home. There is no per-project
supervisor in v0.3. Each tick runs the machine steps once (capacity sample, capacity advice, lease
sweep, lifecycle, machine requests) and then every active project in round-robin order, followed by the
snapshot and the heartbeat. A project that fails to start or tick is parked with a reason and does
not stop the others.

`vowp down` writes `~/.vowp/stop`; a run that is interrupted is finalised as `stalled` and
re-queued, not failed. Windows are left open for inspection unless `--kill-windows` is given.
`--force` is for a supervisor that ignored the stop request, not the normal path.

The supervisor uses `~/.vowp/supervisor.lock` containing its pid. A stale lock left by a killed
process is detected and replaced; do not delete the database to "fix" a lock. Under systemd, use
`systemctl --user stop|restart vowp` and do not start a second detached `vowp up`.

`vowp up` migrates a v0.1 home automatically after a backup unless `--no-migrate` is passed. See
[Migration and upgrades](#migration-and-upgrades).

## Status, fleet and the board

```sh
vowp status
vowp status --json
vowp status --check             # exit 1 when the heartbeat is older than 3 min
vowp fleet status
vowp fleet status --refresh
vowp fleet queue
vowp fleet inbox
vowp board --once
vowp board --interval 5
```

`status` reports the supervisor pid, uptime, last tick age, jobs by state across projects, busy/total
slots, the planner and keeper executors, active limits, cost today and the open inbox. It exits 0
whether or not a supervisor is running; use the JSON `running` field in scripts. `status --check` is
the watchdog contract: exit 1 means the heartbeat is stale.

`fleet status` reads `snapshot.json` when it is fresh and recomputes from the databases otherwise.
On a control machine it fetches the home's snapshot and caches it; when the home is unreachable it
prints the cached answer with an `as of` time and `stale` marker. `fleet queue` lists queued jobs
with their waiting reason (`waiting: RAM 1.2 GB short`, `waiting: provider deepseek 429`), and
`fleet inbox` aggregates open machine and project items.

`vowp board` is the refreshing view. With one project registered and selected it prints the v0.1 project
board; with several projects, or no project selected, it prints the fleet board with a project column.
`board --once` prints one frame. The `vowp-home` tmux session has `board` and `capacity` windows; each
project has its own session.

## Projects

```sh
vowp project ls
vowp project ls --all
vowp project pause <name>
vowp project resume <name>
vowp project archive <name>
vowp project rm <name> --yes
vowp project weight <name> 2
```

| Command | Effect |
|---|---|
| `pause` | Stops new dispatch for that project; running work finishes. |
| `resume` | Makes it active again. |
| `archive` | No ticks and no new work; the database stays. |
| `rm --yes` | Archives the project's database under `~/.vowp/archive/projects/` and removes the registry row. Production records are kept. |
| `weight <name> <1..=100>` | Fairness share when several projects compete for capacity. |

A broken project is paused with a `broken:` reason and an inbox notice. Fix the cause, then
`vowp project resume <name>`; the other projects kept running.

`vowp project add <path>` is the way to add a checkout that already has `vowp.toml`. `vowp init` is
the way to initialise a new checkout; both create the project database, loops and worker slots.

## Capacity

Capacity is measured, advised, owner-decided and machine-protected. The supervisor samples the host
every few seconds, stores per-run usage, admits work against a RAM/CPU/disk/provider reserve, and
brakes when the machine is under pressure. Running work is never killed by the capacity manager.

```sh
vowp capacity show
vowp capacity report --since 7d
vowp capacity report --limit 20
vowp capacity set --target 5 --reason "five workers during the migration"
vowp capacity set --target 8 --override --reason "owner override for the release"
vowp capacity set --mode auto
vowp capacity set --mode fixed --target 4
```

| Mode | Meaning |
|---|---|
| `auto` | The measured advice becomes the target. This is the default. |
| `fixed` | The owner's number is used. Above the advice it needs `--override`; the board shows `owner override: 8 (advised 5)`. |

The report shows the host sample, the current target and advice, running usage, brakes, reserve,
per-project estimates and waiting jobs. A job that is ready but not running shows its reason on the
board and in `fleet queue`; a provider limit and a capacity brake are different causes.

When an agent worker node is online, `capacity show`, `capacity report` and `fleet status` add a
`cluster:` line: online worker nodes, pooled busy/target workers, summed CPU, available/total RAM
and free disk. The home target still governs runs on the home; an eligible offloaded run is
admitted against the chosen worker node's slots and headroom. The full rule is in
[Capacity](../concepts/capacity.md#cluster-capacity).

The AI keeper proposes a target every 30 minutes and on events, as a `decision` inbox item. It never
changes the target itself. The owner answers, and the orchestrator applies it with
`vowp capacity set`. If a run is OOM-killed, the supervisor records it, parks that job with a notice
and raises its estimate; it does not kill other work.

`[node].max_workers` and `[node].max_verifiers` in `vowp.toml` are per-machine preferences. The machine
capacity manager decides actual admission; a target above the advice is honoured but the brakes still
apply.

## Logs and files

```sh
tail -n 100 ~/.vowp/logs/supervisor.log
tail -F ~/.vowp/logs/supervisor.log
```

Each log line is `<timestamp> <LEVEL> <target> <message>`, with targets such as `l1`, `l2`, `l3`,
`merge`, `gates`, `session`, `adapter`, `tmux`, `capacity`, `node` and `cli`. Levels are `ERROR`,
`WARN`, `INFO` and `DEBUG`. The file is append-only and grows until you rotate it; VOWP has no
built-in rotation for the supervisor log, so use your normal log rotation.

| Path | What it is |
|---|---|
| `~/.vowp/vowp.db` | Global database: machines (`nodes` table), projects, sessions, surfaces, leases, capacity, limits, catalog, machine inbox and ledger. |
| `~/.vowp/projects/<name>/project.db` | Per-project database: jobs, edges, runs, gates, workers, loops, schedules, inbox, ledger, releases. |
| `~/.vowp/sessions/<id>/log.jsonl` | Authoritative session content. |
| `~/.vowp/snapshot.json` | Last fleet snapshot, replaced atomically every tick. |
| `~/.vowp/board.json` | v0.1 compatibility copy, written only when exactly one non-reserved project is active (or one paused project is the only one). |
| `~/.vowp/orchestrators/<name>/` | Project-local app config, instructions, lease token, host and mirror state. |
| `~/.vowp/secrets/` | Basic-mode secret files (0600). Vault mode keeps values under the vault user. |
| `~/.vowp/heartbeat` | Unix seconds, rewritten atomically every tick. |
| `~/.vowp/supervisor.lock` | Pid of the running supervisor. |
| `~/.vowp/logs/supervisor.log` | Structured loop log. |
| `~/.vowp/runs/<runId>.jsonl` | Raw harness stdout, debug only; lifecycle-managed. |
| `~/.vowp/inbox/<id>.md` | Human-readable copy of an inbox item. |
| `~/.vowp/backups/` | Backup directories and the pre-migration backup. |
| `~/.vowp/archive/projects/` | Archived project databases. |
| `~/.vowp/migration.json` | Present only while a migration is in flight. |
| `<project>/.vowp/checkpoints/` | Planner checkpoint summaries. |

Never log or paste prompts, environment values or secret-looking strings. Exact injected values are
replaced with `[REDACTED:<NAME>]` on the session-service and remote-ingest event paths, in the
snapshot, in capsules, and in a finished job report and run mirror. The supervisor log, gate command
evidence, ledger rows, the `session switch` harness events and the `session watch` importer are
written without that redaction, and a report or run mirror is still raw while its turn runs. Treat
session logs, the databases and every file under `~/.vowp` as sensitive: they can contain prompts,
tool output and file contents. The coverage is stated exactly in the
[security model](../security.md#redaction).

## Loops

```sh
vowp loops
vowp loops --all
vowp loops pause l4-planner
vowp loops resume l4-planner
```

Loop rows are per project. `vowp loops` uses the selected project; with several projects and no
selection it exits 2 and lists them, so use `vowp loops --all` or `--project <name>`. The rows are:

| Loop | Default | Does |
|---|---|---|
| `l1-supervisor` | 60 s | Reconcile, dispatch and the main tick. Cannot be paused on its own; use `vowp down`. |
| `l2-worker` | event | Continue a worker that stopped before its report (bounded). |
| `l3-verify` | event | Run the verifier and create bounded fix jobs. |
| `l4-planner` | 1800 s | Planner heartbeat. |
| `l5-keeper` | 1200 s | Diagnose stuck work and propose keeper actions. |
| `l6-failover` | event | Switch a limited executor and probe it back. |
| `l7-inbox` | event | Deliver questions and notices. |
| `l8-watchdog` | 300 s | Write the heartbeat; the cron/service watchdog restarts a dead supervisor. |
| `l9-schedules` | 60 s | Fire due schedules, including `gc-nightly`. |

Pausing a loop skips its step but does not cancel work already running. A loop that repeats the same
action three times without new evidence stops and escalates rather than spinning.

## Schedules

Schedules are per-project rows; see the [schedules guide](schedules.md). `vowp schedule --all ls`
lists every project's rows. A schedule can create a job, trigger a loop or run a command. Missed runs
while the machine was down either run once (`run_once`) or are skipped (`skip`); they never replay N
times. The supervisor creates the nightly `gc-nightly` interval schedule in each project the first
time lifecycle runs there.

## Backups and restore

```sh
vowp backup
vowp backup --to /mnt/backup/
vowp backup --to /mnt/backup/ --offsite rsync://backup.example/vowp --encrypt age1...
vowp backup --to /mnt/backup/ --offsite git:<remote>
vowp backup --restore /mnt/backup/<timestamp> --to-home /srv/vowp-restored
vowp reindex
vowp reindex --session <id>
```

A backup is one directory under `~/.vowp/backups/<UTC timestamp>/` by default, containing:

- `global.db` and `projects/<name>.db` for every registered project, written with SQLite's online
  backup while the supervisor runs;
- `sessions/<id>/log.jsonl`, blobs, capsules, `adapters.json` and `summary.md`;
- `catalog.json` and `manifest.json` with SHA-256 hashes and per-table row counts.

The default rotation keeps the newest seven daily and four weekly backups. A path ending in `.db`
keeps the v0.1 form: one global-database copy, not a full backup. `--to <dir>/` or an existing
directory is a full directory backup. `--offsite` (or an `rsync://` / `git:` target) copies the
directory with the external `rsync` or `git` binary; `--encrypt` uses the external `age` binary. A
missing tool is a clear exit 5, never a silent unencrypted copy.

Secrets are never backed up, encrypted or not. Restore them with `vowp secret set NAME` after a
restore. The manifest says `secrets/: never backed up`.

`--restore <dir> --to-home <dir>` writes a fresh home, verifies every hash and row count in a staging
directory, runs integrity checks and only then swaps it into place. It refuses to overwrite a
non-empty target without `--force`. The restore never reads the live store, so it also works when
the serving home is lost. Test the restore path before an upgrade if the data matters.

`vowp reindex` rebuilds the session index (`sessions`, `session_events`, `session_natives`,
`capsules`) from `sessions/*/log.jsonl`. It is safe while the supervisor runs and prints counts. It
does not rebuild jobs, gates or decisions; restore a backup instead of hand-editing a database.

## Lifecycle and garbage collection

Every file VOWP creates belongs to a lifecycle class:

| Class | Default |
|---|---|
| `production` | Releases, rollback snapshots, serving banks and prod pointers. Never auto-deleted. |
| `truth` | Databases, capsules, passing gate evidence, decisions and ledger. Never auto-deleted; backed up. |
| `open_work` | Worktrees of unmerged or unpushed jobs, active sessions and orchestrator sessions. Never auto-deleted. |
| `secrets` | Secret files and the mode file. Never auto-deleted, never backed up in plain text. |
| `closed_session` | Full log and blobs of a closed session. Kept 30 days, then compacted to capsule + summary. |
| `run_output` | `runs/*.jsonl`, `*.stderr`. 14 days; gzip after 1 day. |
| `worktree` | Merged jobs removed after merge; failed or cancelled jobs kept 7 days, branch kept. |
| `job_window` | Tagged `job-<id>` tmux windows whose job is done, cancelled, failed, parked or replanned. Listed by `gc`; `--apply` closes them. |
| `sample` | Host samples: 24 h full rate, 30 days as 1-minute averages. |
| `cache` | Control snapshot cache and `tmp/`. Disposable. |

```sh
vowp gc
vowp gc --class run_output
vowp gc --apply
vowp gc --older-than 7d
vowp gc --older-than 7d --class closed_session --apply
```

`vowp gc` is a dry run by default: it lists files per class, bytes and a sample of paths, plus the
files and bytes an `--apply` would collect now, and changes nothing. `--apply` deletes only
`closed_session` (after writing a final capsule and `summary.md`), `run_output`, `worktree`, `sample`
and expired `cache` entries, and closes `job_window` entries. It never deletes production, truth,
open work or secrets. The nightly `gc-nightly` schedule runs `vowp gc --apply` once per day per
project, so it closes orphan job windows too.

`--older-than <duration>` shortens the window for `closed_session` and `run_output` only, for one
explicit pass; it does not change the defaults or the nightly schedule. Durations are a positive whole
number followed by `s`, `m`, `h`, `d` or `w` (`36h`, `7d`, `2w`); zero, negative, malformed and
larger-than-ten-year values exit 2. With `--class`, only `closed_session` and `run_output` accept the
override. A dry run prints `ELIGIBLE` and `ELIGIBLE BYTES` next to each class, so review those before
adding `--apply`. The pass refuses and lists any session with a live lease, a job that still holds its
lock (running, stalled, gating, reviewing, fixing or merging), or a running orchestrator. Compaction
still leaves the session row, capsule and `summary.md`, so `vowp reindex` and `vowp session show`
continue to work for every remaining session.

After compaction, `session show --events` and `session tail` on that session exit 3 with
`session compacted to capsule <id>; use 'session show --capsule'`. `session show --capsule` and
export still work; export writes the capsule with an empty event tail and says so. There is no
full-log replay after compaction.

## Secrets

```sh
vowp secret set <NAME>
vowp secret set <NAME> --stdin
vowp secret ls
vowp secret request <NAME>
vowp secret rm <NAME> --yes
vowp secret upgrade
vowp secret upgrade --encrypted
```

`set` reads a hidden prompt (there is no argument for the value), writes atomically with mode 0600
and records only the last four characters in the index. `set --stdin` reads the value from the
process stdin instead, so a script or an SSH pipe needs no TTY; it strips one trailing newline and
refuses an empty value. `request` starts a one-time page bound to `127.0.0.1` with a random token
and a ten-minute expiry; through a control-machine tunnel the owner's browser sees a local URL. The
AI sees `NAME saved, ends in …last4`, never the value.

`upgrade` prints the exact sudo command that creates the vault user and moves the files. Without
sudo, VOWP continues in basic mode with the same injection and redaction. `upgrade --encrypted`
instead stores basic-mode values as age ciphertext under `~/.vowp/secrets/encrypted/` with a
mode-0600 `~/.vowp/secrets/age.key`; it encrypts before deleting plaintext, writes the mode file
last, refuses vault mode and does not use `systemd-creds`. `secret ls --json` reports the active
`mode` (`basic`, `vault_user` or `encrypted`) with no values.

The honest limits, the injection path and the redaction coverage are in the
[security model](../security.md#secrets) and [Running a home on a VPS](vps.md#secrets).

## Orchestrators

```sh
vowp orchestrator new --name main --app chat --executor <executor>
vowp orchestrator new --name main --app opencode --project <project>
vowp orchestrator ls
vowp orchestrator move main --node <node>
vowp orchestrator move main --home
vowp orchestrator stop main
vowp orchestrator resume main
```

An orchestrator is a project-less or project-bound session with the `orchestrator` role. Its
directory is `~/.vowp/orchestrators/<name>/` and holds only project-local app config, the
instructions template, the lease token, host state, the transcript cursor (`mirror.json`), the
keeper log (`keeper.log`) and the keeper pid (`keeper.pid`). `new` starts the app host where that app
supports it (an opencode server that binds loopback, or Claude Remote Control, which opens no local
port and connects outbound to claude.ai) and prints desktop, phone and `vowp chat` join
instructions. `stop` stops the host and releases the lease; the session stays. `resume` starts it
again. A host that cannot start is marked `degraded` and does not stop the supervisor.

`move` switches an existing orchestrator between this home and an enrolled local agent node without
changing its `SessionId` or its session log on the home. Use `--node <node>` to run the app on that
node and `--home` to bring it back; exactly one is required. The current host stops, its lease is
released, a handoff capsule is written on the same session when it has context, and the app is
started at the target. The target is checked before anything stops, so an unknown, pending or
revoked node, or a project the node has not mapped, leaves the current host running. Moving to the
placement the orchestrator already uses is a no-op, or resumes a stopped record there. A
project-bound orchestrator needs its project path mapped on the target node, and the node needs a
paired surface for the session. Put the global `--home <DIR>` flag before `orchestrator move`;
after the command, `--home` is the target switch.

The join instructions use generic `<home-ssh-target>` and `<session>` placeholders; the control
machine supplies the concrete target. The fallback is always `vowp chat <session-id>`.

To connect a local UI client to the orchestrator's app process, ask for the endpoint document:

```sh
vowp orchestrator endpoint main --json            # on the home: location home or node
vowp agent orchestrator endpoint main --json      # on the agent machine that runs the app
```

A home-hosted live opencode host returns a loopback `url`, the native app session id and Basic
`auth`; the password is in that explicit `--json` document only. A stopped host, Claude Remote
Control and `chat` return null endpoint values. A node-hosted record on the home is metadata only;
run the agent command on that machine to get its local auth. These commands never open SSH, a
tunnel, a browser or the node's files.

### Who owns the orchestrator's session

The endpoint document also carries `owner` (the recorded owner of the session, active, stale or
expired, or `null`) and `access`. `access` is `read_only` unless you pass the live owner's surface
id:

```sh
vowp orchestrator endpoint main --surface <surface-id>   # access: owner only for the live owner
```

A UI that must write attaches first. `attach` creates a `desktop-ui` surface, prints a one-time grant
(surface id, lease token, `access`, `url` and expiry) and then **holds the lease in the foreground**,
heartbeating until its stdin reaches end-of-file, when it releases the lease:

```sh
vowp orchestrator attach main --holder opencode-desktop --label "StackSpirit · macbook"
vowp orchestrator attach main --holder opencode-desktop --takeover --json
```

The UI process must start `attach` as a child and keep it alive while the UI writes; closing its stdin
releases the lease. No detached keeper exists for an attached UI in this release, so a UI that exits
without a live `attach` simply has no lease. Treat the grant like a secret: the token is printed once,
on stdout, and the hold notice goes to stderr so `--json` stays parseable. `attach` works only on a
home-hosted orchestrator with a live app endpoint: a node-hosted orchestrator is refused, a stopped
host asks you to run `vowp orchestrator resume <name>` first, and `--node` is not an SSH redirect.

If another surface owns the session (the orchestrator's own app host normally does while it runs),
`attach` refuses unless you pass `--takeover`, or unless that owner is stale (no heartbeat for 60
seconds). A takeover replaces the lease, increments the owner
epoch, writes a capsule and tells the old surface to stop; the old surface keeps following read-only.
`vowp chat` asks before taking a live lease (`/takeover`, or `--takeover` at start) and `/follow`
returns to read-only. A UI that only observes does not attach; it asks `endpoint` for `owner` and
`access: read_only` and uses read routes only.

To see who owns a session:

```sh
vowp fleet status          # an owners: section, one line per owned recent session
vowp session lease <id>    # holder, kind, node, mode, state, activity, epoch, expiry
vowp session show <id>     # an owner: block, or owner: none
```

Each `fleet status` line shows the session id, title, label, kind and mode, `holder@node`, state,
activity and, for an interactive owner, `heartbeat <age>`; a job shows `expires in <span>` or
`expired`. Nothing in these views prints a token or password. A running job is owned in `job` mode
for the life of the run (renewed by run activity, not by the 60-second window); taking it over from
a person's surface fences the run, writes a capsule and leaves the job `ready`.

**Limits.** Ownership is recorded and cooperative: a UI that calls the app's HTTP API directly with
the engine password is not stopped by it. The loopback engine proxy that would enforce it is **planned
and deferred**, and there is no node-side proxy, so `vowp agent orchestrator endpoint` reports
`read_only` with no home owner view. See [home and surfaces](../concepts/home-and-surfaces.md#who-owns-a-session).

## Migration and upgrades

A v0.1 home has one `vowp.db` with project tables. v0.2 splits it into `vowp.db` plus
`projects/<name>/project.db`. `vowp up` migrates automatically after a backup; to inspect or control
it:

```sh
vowp migrate --dry-run
vowp migrate --yes
vowp migrate --rollback ~/.vowp/backups/pre-0.2-<timestamp>.db
```

The migration:

- refuses while a supervisor is running, on a corrupt database, or when a row names a project that
  does not exist;
- writes `~/.vowp/backups/pre-0.2-<timestamp>.db` first and reports the path;
- stages the new files, copies the global and per-project rows, verifies row counts and integrity,
  then swaps them in;
- leaves session logs unchanged and is restartable from `migration.json` after a crash.

`--rollback` restores the pre-migration database and moves the 0.2 project directory aside. It also
refuses while a supervisor is running. The acceptance test for a real v0.1 home is the migration
test in the release checklist.

Upgrade the binary in this order: stop the supervisor, install the new binary, run `vowp doctor`,
start `vowp up`. Database migrations are forward-only and keyed by `PRAGMA user_version`; a database
written by a newer binary is refused rather than guessed at. Back up before an upgrade and test the
restore path.

## Routine checks

Once a day:

- `vowp status` and `vowp fleet status` — supervisor healthy, no stuck `running` job, limits resetting.
- `vowp fleet inbox` — questions and approvals waiting.
- `vowp capacity show` — target, advice and any brake.
- `vowp cost --since 1d` — spend and unknown-cost count.
- `vowp backup` — keep a copy outside the home.

Once a week:

- Read `~/.vowp/logs/supervisor.log` for repeated `WARN`/`ERROR` targets.
- Check disk space; runs and blobs grow with tool output.
- Run `vowp gc` (dry run) and review the classes before an `--apply`.
- Review `open_items` and `questions_for_owner` in finished reports.

Next: [Sessions and surfaces](sessions.md) for chat, takeover and switching, or
[Troubleshooting](../troubleshooting.md) when the board is not moving.
