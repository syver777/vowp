# cli.md — the `vowp` command line

Status: current

Implemented by `vowp-cli` on top of `vowp-core`. The v0.1 command surface comes from the design
summary §12 (plus the additions in `decisions.md` D12); the v0.2 commands are specified in
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) §12–§13. The complete help tree is
generated in [`docs/reference/cli.md`](../reference/cli.md). Crate API and module split:
`crates/vowp-cli.md`.

## 1. Global behaviour

```
vowp [GLOBAL FLAGS] <command> [args]
```

| Flag | Meaning |
|---|---|
| `--json` | machine-readable output on stdout (one JSON document; `ls` commands print an array; streaming commands print JSON lines). Human text goes nowhere else. |
| `--node <name>` | run the command on that node over ssh (§8). On a control machine, commands without it route to the configured home (§6.4). |
| `--project <name\|path>` | project to act on; default: the nearest ancestor directory of the cwd containing `vowp.toml`, else the only project in the DB, else error `invalid_input` ("no project: run `vowp init`"). With more than one project and no selection, a project-scoped command exits 2 listing the projects. |
| `--home <dir>` | node home (default `$VOWP_HOME`, else `~/.vowp`). Tests always pass this. |
| `--no-color`, `-q/--quiet`, `-v/--verbose` | rendering/logging |

- Human output: aligned tables, no colour when stdout is not a TTY or `NO_COLOR` is set, timestamps shown as local `HH:MM` for today / `MM-DD HH:MM` otherwise; `--json` always uses RFC 3339 UTC.
- Ids are accepted by unique prefix (sessions, runs, inbox, releases: ≥ 6 chars of the ULID; ambiguous prefix → exit 2 listing candidates). Jobs need the full slug.
- Mutating commands print what they did in one line (`added job graph-engine (pending)`); with `--json` they print the affected object.
- Reading commands work while the supervisor runs (WAL). Commands that change job state (`retry`, `park`, `cancel`) enqueue a request row (decision D2) and wait up to 5 s for the supervisor to apply it; with no supervisor running they apply it directly through `vowp_graph::transition`.
- **Home vs control.** A machine with `~/.vowp-control/control.json` (or `VOWP_CONTROL_HOME`) is a
  control machine: reads and writes are sent to the configured home over `ssh -o BatchMode=yes`.
  `vowp up`, `vowp down` and `vowp board` refuse there with exit 5. `--home <dir>` always means
  "this directory is the home". See `v0.2/home-and-surfaces.md` §8.

### 1.1 Exit codes

| Code | Meaning | `ErrorKind` |
|---|---|---|
| 0 | success | — |
| 1 | internal / unexpected error | `Internal`, `Io`, `Db`, `Parse`, `Config`, `AdapterFailed`, `Timeout`, `AdapterUnavailable` |
| 2 | invalid input / usage (clap errors, bad id, bad cron, ambiguous prefix, missing project selection) | `InvalidInput` |
| 3 | not found (job, session, node, project, secret, tunnel, …) | `NotFound` |
| 4 | conflict (job exists, state does not allow the action, supervisor already running, lease held, migration required) | `Conflict` |
| 5 | guard failed or action refused (promotion guard, cwd outside allow-list, control cannot `up`, target above advice without `--override`, missing `--yes`) | `Guard`, `Refused` |
| 6 | provider limited / auth needed (action needs a provider that is limited) | `Limit`, `Auth` |
| 7 | remote failure (ssh could not connect, remote `vowp` missing) | `Remote` |

`--json` errors: `{"error":{"kind":"not_found","message":"job n99 not found","hint":"vowp job ls"}}` on stdout, exit code as above.

## 2. Setup and health

### `vowp init [--name <project>] [--force] [--no-framework]`
In the current git repo: writes `vowp.toml` (a `[node]` table with `tz_offset_minutes` set from the machine's current UTC offset, the `[project]` table, and the four default profiles from architecture.md §5.2 minus schedules), `.vowp/` (jobs/, checkpoints/), installs the framework files
(`AGENTS.md`, `CLAUDE.md` containing `@AGENTS.md`, `.claude/skills/vowp-{worker,planner,verifier,keeper}/SKILL.md`, `opencode.jsonc`, brief/report templates) from the templates **embedded in the binary**,
appends `.vowp/tmp/` to `.gitignore`, creates the home (global DB, directories), registers the local node and the project, creates `~/.vowp/projects/<name>/project.db`, and creates the project's loop rows (`loops` table, default intervals). Existing files are never overwritten without `--force` (skipped files are listed).
Human: `initialised project vowp at /path (8 files written, 2 skipped)`. JSON: `{"project":"vowp","path":"…","written":[…],"skipped":[…]}`. Exit 4 if already initialised and not `--force`.

### `vowp doctor`
Checks (each `ok|warn|fail` with detail): `home` writable; `db` opens + `integrity_check` + schema version; `schema` matches the binary; `projects` each have an openable project DB; `snapshot` is writable; `git` version; `tmux` version (warn if absent); harness probes via adapters
(`claude`, `opencode`, each `Adapter::probe`); `vowp.toml` parses + validates and every `[[schedules]]` cron/interval/timezone parses with `ScheduleSpec::parse` (an invalid entry fails the `schedules` check with the schedule name and error); profile ladders resolve to installed harnesses; providers reachable **only by env-var-name presence** (`PROVIDER_API_KEY` set? — never prints the value, never calls the network);
`systemd --user` unit and cron watchdog installed (warn only); supervisor running; disk space ≥ 1 GB.
Exit 0 if no `fail`, else 1. JSON: `{"ok":true,"checks":[{"name":"tmux","status":"ok","detail":"tmux 3.4"}]}`.

### `vowp up [--foreground|--detach] [--no-tmux] [--no-migrate] [--once]` · `vowp down [--force] [--kill-windows]` · `vowp status [--check]` · `vowp board [--once] [--interval <secs>]`
- `up`: runs **one node supervisor for every active project**. It registers the current checkout
  when it has a `vowp.toml`, migrates a v0.1 home first unless `--no-migrate` is given, ensures the
  tmux sessions (`vowp-home` with `board` and `capacity`; `vowp-<project>` with
  `board supervisor planner keeper inbox`; skipped with `--no-tmux`), then starts the supervisor.
  Default (= `--detach`, also accepted explicitly; used by the watchdog): detaches (re-executes
  itself with `--foreground` under `setsid`/nohup-style, writes `supervisor.lock`); `--foreground`
  runs the loop in this process (what systemd runs); `--once` runs startup plus exactly one node
  tick then exits (used by tests and cron). Exit 4 if a supervisor is already running; exit 5 on a
  control machine.
- `down`: writes `~/.vowp/stop`, waits ≤ 30 s for the lock to disappear; `--force` additionally sends SIGTERM to the pid. Running runs are interrupted and re-queued (`stalled`, not failed). Windows are left for inspection unless `--kill-windows`.
- `status`: one screen — supervisor (pid, uptime, last tick age), every project with jobs by state, slots busy/total, planner/keeper executors, limits with reset times, cost today, inbox open count. JSON keeps the v0.1 keys and adds `home` and `projects`. Exit 0 whether or not a supervisor runs (use `--json` `running` field; `vowp status --check` exits 1 if the heartbeat is older than 3 min — the watchdog uses this).
- `board`: refreshing dashboard (clear+redraw every `--interval`, default 2 s; `--once` prints once). With one project selected it prints the v0.1 single-project frame: jobs by state, the running jobs (job, executor, age, last event age), slots, limits, cost, inbox and the last checkpoint. With several projects, or no project selected, it prints the fleet frame: every project's state, jobs by state, running/queued/blocked jobs, last merge, gate failures and cost today, then provider limits and the open inbox count; the fleet frame prints no slots and no last checkpoint. It always recomputes from the databases; `board.json` is only a convenience copy written by the supervisor.

## 3. Planning and the graph

### `vowp plan "<goal>" [--profile <name>] [--dry-run]`
Starts a planner turn (session role `planner`, executor = first available rung of `profile`, default `planner`) with a prompt containing the goal, the framework rules for planners, the current graph digest and the exact `vowp job add` syntax. The planner is expected to call `vowp job add/update` and write briefs; the command streams a one-line-per-event summary and, at the end, prints the jobs created (`graph diff`) for the human to review. `--dry-run`: the planner is told not to run `vowp` commands and to output the graph as YAML instead (printed, nothing stored). Exit 6 if every rung is limited.

### `vowp job …`

| Command | Behaviour |
|---|---|
| `job add --id <slug> --title <text> --kind <k> [--role <r>] [--profile <p>] [--needs a,b] [--owns glob,…] [--reads glob,…] [--gates <set\|short,…>] [--priority n] [--max-attempts n] [--brief-file <path>\|--brief -]` | validates (`NewJob`), inserts job + edges, writes the brief (generated front-matter + body from file/stdin; with neither, a template brief), state `pending` (→ `ready` on the next tick when deps are done). Exit 4 if the id exists; 2 if a need is unknown or a cycle would form. `--role` default by kind (`build→worker`, `verify→verifier`, `plan→planner`, `decision→keeper`). |
| `job update <id> [--title] [--priority] [--profile] [--owns] [--reads] [--gates] [--add-needs a,b] [--remove-needs a] [--note text]` | `JobPatch`. Only jobs not in `running/gating/reviewing/merging` may change `owns`/`gates` (exit 4). |
| `job ls [--state s,…] [--kind k] [--role r] [--all]` | table: `ID STATE KIND ROLE PRIO NEEDS ATTEMPTS UPDATED NOTE`; default hides `done/cancelled/replanned` unless `--all`. JSON: `[Job,…]`. |
| `job show <id> [--brief] [--report] [--runs] [--gates]` | the job, its edges, last runs (executor, outcome, cost), last gate results with evidence tails; `--brief/--report` print the files. |
| `job retry <id>` | by state: `failed` → `ready` (event `Retry`, T32 resets `attempts`, `continues` and `hang_restarts`), `parked` → `ready` (`Resolved`), `stalled` → `running` (`KeeperRestart`). Exit 4 from any other state. |
| `job park <id> [--reason text]` | any non-terminal → `parked` (`parked_reason=manual`); stops its run (`interrupt` then `stop`). To resume use `job retry`. |
| `job nudge <id>` · `job restart <id>` · `job replan <id>` · `job fail <id> --reason t` | keeper commands (loops.md §7): enqueue a request row applying the corresponding `JobEvent` (`KeeperNudge|KeeperRestart|KeeperReplan|KeeperFail`); exit 4 if the job's state does not allow it. |
| `job cancel <id>` | any non-terminal → `cancelled`; stops the run, releases the ownership lock (worktree and branch are kept on disk for inspection). Dependents stay `pending`; `vowp job ls` flags them `blocked-by-cancelled`. |

### `vowp graph [--mermaid] [--export <file.yaml>] [--import <file.yaml>] [--all]`
Default: ASCII, exactly `render_ascii` of `vowp-graph` (one line per job ordered by layer: `L1 graph-engine [running] graph engine  <- protocol`). `--mermaid`: exactly `render_mermaid` (`graph TD`, one node per job, edges per kind; no `classDef`). `--export`: YAML (jobs + edges) to the file (stdout if `-`). `--import`: validates then inserts missing jobs/edges (never overwrites; conflicts listed, exit 4). Formats: `crates/vowp-graph.md`.

### `vowp attach [<job>] [--node <n>]`
`tmux attach -t vowp-<project>:job-<id>` (or the session's `board` window with no job). Exec's tmux (inherits the TTY). If tmux is absent or the window is gone: falls back to `vowp session tail --follow` of the job's latest session. Exit 3 if the job has no session.

## 4. Sessions and surfaces

All `session` subcommands accept a session id prefix; `vowp chat <id>` is the v0.2 terminal surface.

| Command | Behaviour |
|---|---|
| `session ls [--project p] [--job j] [--state s] [--limit n]` | `ID TITLE EXECUTOR NODE STATE LAST UPDATED EVENTS`; JSON `[Session,…]` |
| `session show <id> [--capsule] [--events <n>] [--lineage]` | metadata, executor history (`session.executor` events), switches, last capsule; `--events n` prints the last n events pretty |
| `session new [--title t] [--executor <exec>] [--cwd dir] [--node n]` | creates an interactive session (role `user`), prints its id (used by `chat`) |
| `session chat <id> [--executor <exec>] [--node <n>] [--follow] [--takeover]` | the same v0.2 terminal surface as `vowp chat` (the alias is kept) |
| `session switch <id> --to <executor> [--node <n>] [--reason user\|limit\|…] [--force]` | runs the switch protocol (`sessions.md` §5): prints the capsule path and the new executor; exit 6 if the target provider is limited, 4 if the session is running a turn and `--force` is not given. A remote target is probed with `vowp doctor` first. |
| `session capsule <id> [--print]` | writes a capsule now (`trigger=manual`), prints its id (or the markdown with `--print`) |
| `session export <id> [--out <file>\|-o <file>] [--tail <n>]` | writes a `vowp-bundle-v1` JSON bundle (default `./<id>.vowp-bundle.json`, tail 20 events) |
| `session import <file> [--node n] [--executor <exec>] [--cwd <dir>]` | validates checksum/version, creates a **child** session (`parent_session_id`), prints the new id; the seed capsule is attached |
| `session watch [--claude-dir <dir>] [--once]` | records interactive Claude Code transcripts (read-only importer) into canonical sessions; prints `watching N transcripts`; without `--once`, a TTY run exits 2 because continuous following is not available (non-TTY input does one pass); exit 1 if the dir is unreadable |
| `session tail <id> [--follow] [--pretty] [--from <seq>]` | prints events (JSONL, or `--pretty` one line per event); `--follow` polls the file (200 ms) and does not take the lease. This is what job windows run (`vowp __window job <id>`). |

Pretty line format: `HH:MM:SS #seq type  summary` e.g. `19:30:12 #6 tool.call  shell: cargo test`.

### `vowp chat <id> [--executor <exec>] [--node <n>] [--follow] [--takeover]`
The guaranteed terminal surface and the reference implementation of the lease rules. It acquires
the session lease (or follows read-only), runs the turn on this surface's machine by default, and
supports `/switch <executor> [--node n]`, `/status`, `/takeover`, `/follow`, `/capsule`, `/quit`.
A takeover by another surface prints
`Taken over by phone at 14:05. This surface is now read-only (follow mode).` and keeps showing new
events. See `sessions.md` §8–§11.

### Hidden session interfaces

These are implementation interfaces, not help-listed user commands. `vowp session turn-begin`,
`vowp session ingest`, `vowp session lease` and `vowp session takeover` are hidden subcommands of
`vowp session`; `vowp __exec`, `vowp __vault-exec` and `vowp __vault-admin` are hidden top-level
commands. They still accept `--json` where a document is printed; the streaming ones print one JSON
object per line.

```
vowp session turn-begin <session> --executor <exec> --purpose <p> --node <n> --request <file|->
vowp __exec --grant <file> [--spool-dir <dir>]
vowp session ingest <session> --run <run-id> [--replay] [--json]
vowp session lease <session> [--acquire <kind> --holder <h> --node <n>] [--json]
vowp session takeover <session> --kind <kind> --holder <h> --node <n> [--json]
```

`turn-begin` reads a request document (lease token, prompt, executor, purpose, node) and prints a
`TurnGrant`; `__exec` runs the adapter on the executor machine, spools every event before sending
it, and pipes `RemoteLine` JSON lines into `session ingest`; `ingest` assigns `seq`, is idempotent by
event id and acknowledges each event. The lease token is never on argv or in `ps`; the grant file is
mode 0600. `session lease` prints the token only on acquire. Full protocol: `sessions.md` §9.

## 5. Inbox, cost, nodes, loops, schedules, release

### `vowp inbox ls [--all] · show <id> · ask <title> [--body t] [--job <id>] [--kind question\|approval\|notice] [--option a --option b] · answer <id> <text\|--approve\|--reject> [--by <name>] · dismiss <id>`
`ask` creates an item (used by planner/keeper agents and scripts; with `--job` the job is parked with `parked(Question)` for kinds `question|approval`, never for `notice`; prints the item id). `ls` lists open items (`ID KIND JOB TITLE AGE`); `--all` includes answered. `answer` stores the answer, sets `answered`, and (for an item with `job_id`) moves the parked job to `ready` on the next tick (`loops.md` §9). `--approve/--reject` are shorthand answers `approve`/`reject` valid for `approval` items. Exit 4 if already answered. Inbox items are per project; `vowp fleet inbox` reads every project plus the node inbox.

### `vowp cost [--by job|role|provider|model|day] [--since <dur\|date>] [--job id]`
Table `KEY RUNS USD INPUT OUTPUT CACHE_R SOURCE`; totals row; `SOURCE` is `receipt`, `price-table`, `unknown` or `mixed`, derived from `CostRow.receipt_entries`/`price_table_entries`/`unknown_entries`: one source present → that source, more than one → `mixed`. Unknown costs are shown as `?` and counted separately (never as 0). JSON: `{"by":"job","rows":[{"key":"graph-engine","runs":3,"usd":0.42,"tokens":{…},"source":"receipt"}],"total":{…}}`.

### `vowp node add <name> <ssh-target> · ls · rm <name>`
`add` records the node and tests `ssh -o BatchMode=yes <target> vowp --version` (stores the result in `capabilities`; failure is a warning unless `--strict`). `add <name> -` registers a surface-only executor node (a laptop that dials the home; the home never dials it). `ls`: `NAME TARGET LOCAL LAST_SEEN VERSION`.

### `vowp loops [--all] [pause <loop> | resume <loop>]`
No args: table of loop rows (`NAME KIND STATE INTERVAL LAST NEXT STATUS FAILS`). Loop rows are per project; `--all` lists every project's rows with a project column. `pause/resume` set `loops.state` (L1 itself cannot be paused; use `down`). Exit 3 for an unknown loop name. With more than one project and no `--project`, exit 2 listing them.

### `vowp schedule add "<spec>" <job-template|loop|command> …` · `vowp schedule ls` · `vowp schedule pause <name>` · `vowp schedule resume <name>` · `vowp schedule rm <name>` (`--all` on any of them)
`<spec>` is a 5-field cron string or `every 30m`. Forms:
```
vowp schedule add "0 7 * * 1-5" loop l4-planner --name morning-report --tz +02:00 --missed run_once
vowp schedule add "every 30m" command --name hb -- vowp status --check
vowp schedule add "0 1 * * *" job-template --name dep-update --template-file deps.json   # a NewJob JSON (types.md §7)
```
Validated with the same rules as `[[schedules]]` (architecture.md §5.3); prints the computed next run. `ls`: `NAME SPEC ACTION STATE LAST NEXT STATUS`. Schedules are per project; `--all` lists every project's rows.

### `vowp promote <change> --to <env> [--yes] [--dry-run]` · `vowp rollback <release> [--yes]` · `vowp releases [--env e] [--limit n]`
Work→Production (`crates/vowp-core.md` §promote). `<change>` = commit/ref/job id. `promote` runs: guards → serving bank → diff candidate vs serving → approval (an inbox `approval` item unless `--yes` **and** `approval = "policy"`; `--yes` alone never bypasses a human approval) → deploy → probe → release record. `--dry-run` runs guards + diff only. Exit 5 when a guard fails or the diff is unexpected (release recorded as `refused`); exit 0 when staged awaiting approval (prints the inbox id). `rollback` redeploys the banked serving copy. `releases`: `ID ENV CHANGE STATE FROM→TO CREATED`.

## 6. Home, fleet and control (v0.2)

These commands are new in v0.2. Full `--json` shapes are in
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md) §12.

### 6.1 `vowp project`

```
vowp project add <path> [--name <n>] [--weight <1-100>]
vowp project ls [--all]
vowp project pause|resume|archive <name>
vowp project rm <name> --yes
vowp project weight <name> <1-100>
```

`add` registers an existing checkout on the home (idempotent for the same path and name; it does not
start a supervisor). `ls` prints state, weight, path and update time; `_node` is hidden unless
`--all`. `pause` stops new dispatch but keeps maintenance; `resume` returns a paused project to
`active`; `archive` stops every tick and keeps the database; `rm --yes` archives the project
directory under `~/.vowp/archive/projects/` and removes the registry row (refused while running and
for `_node`). `weight` sets the fairness weight. JSON: `ProjectEntry` or `[ProjectEntry]`;
`rm` prints `{name, archive_path, removed}`.

### 6.2 `vowp fleet`

```
vowp fleet status [--refresh] [--cached]
vowp fleet queue [--project <n>] [--state <s>]
vowp fleet inbox [--project <n>] [--all]
vowp fleet capacity
```

Reads the home's `~/.vowp/snapshot.json` (schema `vowp-snapshot/1`, exported by
`vowp schema snapshot`) when it is fresh and recomputes from the databases otherwise. On a control
machine, `status` can fall back to the cached snapshot and prints its `as of` time; `--refresh`
requires SSH and `--cached` never opens a connection. The JSON wrapper is
`{"snapshot":{…},"source":"local|remote|cache","fetched_at":"…","stale":false}`. `queue` rows
include `waiting_reason`; `inbox --json` prints `[InboxItem]` with the full body (the snapshot's own
inbox cards omit it); `capacity` is an alias of `capacity show`.

### 6.3 `vowp capacity`

```
vowp capacity show
vowp capacity report [--since <dur|date>] [--limit <n>]
vowp capacity set [--target <n>] [--mode auto|fixed] [--override] [--reason <text>] [--by <actor>]
```

`show`/`report` return `CapacityReport`: the latest host sample (or none), the owner target and
advice, running run count and p90 estimates, brakes, reserves, estimates and waiting jobs. `set`
writes one `capacity_targets` history row. A target above the advised value needs `--override`
(exit 5 otherwise); the brakes still apply and `capacity show` prints
`owner override: 8 (advised 5)`. The board does not carry capacity data. The AI advisor only
proposes: it opens a `capacity` inbox item and never changes the target itself.

### 6.4 `vowp control` and `vowp tunnel`

```
vowp control init <ssh-target> [--name <home-name>] [--strict] [--force]
vowp control status
vowp tunnel open <name> --remote-port <n> [--local-port <n>] [--host 127.0.0.1] [--allow-privileged]
vowp tunnel ls
vowp tunnel close <name>
```

`control init` creates `~/.vowp-control/` (role `control`): `control.json` (0600), `nodes.json`,
`surfaces.json`, `tunnels.json` and `spool/`. The control machine runs no supervisor, loops, graph
or worktrees; every project/fleet command routes to the home. `tunnel` starts a managed
`ssh -L 127.0.0.1:<local>:<host>:<remote>` forward, records its pid and ports in `tunnels.json`
(0600), and refuses on the home (a forward there would point at itself). `tunnel ls` prunes dead
records.

### 6.5 `vowp migrate`

```
vowp migrate [--dry-run] [--rollback <backup>] [--yes]
```

Splits a v0.1 `vowp.db` into the global DB plus one DB per project, verifies row counts per table,
and keeps the original as `backups/pre-0.2-<ts>.db`. It refuses while a supervisor is up.
`--dry-run` prints the report without writing; `--rollback <backup>` restores a pre-migration
backup. `vowp up` runs the migration automatically (with a backup) unless `--no-migrate` is given.
JSON: `MigrationReport {from_version,to_version,backup,projects,tables,verified,dry_run}`.

### 6.6 `vowp secret`

```
vowp secret set <NAME>
vowp secret ls
vowp secret rm <NAME> [--yes]
vowp secret upgrade
vowp secret request <NAME>
```

Values are never accepted as arguments. `set` reads from a hidden prompt (requires a TTY; exit 5
without one); `ls` prints names and the last four characters; `rm` deletes; `upgrade` prints the one
sudo command that moves basic-mode files to the vault user; `request` starts a one-time paste page
bound to `127.0.0.1` with a 10-minute expiry and returns `{name,url,expires_at,single_use}`. Basic
mode stores `~/.vowp/secrets/<NAME>` (0600, same user); vault mode stores under
`/home/vowp-vault/secrets/<NAME>` through the `vowp __vault-exec`/`__vault-admin` helpers. Injected
values are redacted from events, capsules, report artifacts, raw run mirrors and the snapshot; no
secret is included in a backup. Honest limit: code that is given a value can print it.

### 6.7 `vowp gc`

```
vowp gc [--apply] [--class <class>] [--now <RFC3339>]
```

Dry run by default: lists eligible files per lifecycle class (`production`, `truth`, `open_work`,
`secrets`, `closed_sessions`, `run_output`, `worktrees`, `samples`, `cache`). `--apply` deletes
only the eligible classes; production, truth, open work and secrets are never touched. A nightly
`gc-nightly` schedule runs `--apply` by default. `--now` is a test clock override.

### 6.8 `vowp backup` (v0.2 forms)

```
vowp backup [--to <path|target>] [--offsite <rsync|git target>] [--encrypt <age recipient>]
vowp backup --restore <backup-dir> --to-home <fresh-dir> [--force]
```

The full backup unit is the global DB plus every project DB, capsules and the catalog, written with
SQLite's online backup; rotation keeps 7 daily and 4 weekly copies. `--to <file>.db` keeps the v0.1
single-global-DB copy. `--offsite` copies through rsync or a git remote; `--encrypt` wraps the
archive with `age` to the given recipient. `--restore` stages a backup into a fresh home. Secrets
are never backed up. JSON: `BackupReport`.

### 6.9 `vowp setup`

```
vowp setup plan --target <ssh-target> [--json] [answers…]
vowp setup home --target <ssh-target> --apply [--yes] [--json] [answers…]
vowp setup control [--apply] [--target <ssh-target>] [--name <n>] [--force]
vowp setup verify [--json]
```

`plan` prints every phase, command and file it will touch without changing anything; `home` applies
the plan idempotently (install `vowp`, `git`, `tmux`, the systemd unit, the global DB, the vault or
basic-mode secrets, register repositories, run `doctor`; no-sudo path included); `control` sets up
this machine as a control machine; `verify` runs a fixture job through gates and merge, checks the
snapshot is readable from the control machine, and creates an orchestrator session that answers
through `vowp chat`. A human hand-off (sudo password, harness login, secret value) stops the run with
exit 5 instead of working around it. Answers are names only: `--harness`, `--provider`, `--secret`,
`--repository`; a secret value is never an argument.

### 6.10 `vowp orchestrator`

```
vowp orchestrator new [--name <n>] [--project <p>] --app opencode|claude-code|chat [--executor <e>] [--force]
vowp orchestrator ls
vowp orchestrator stop <name>
vowp orchestrator resume <name>
```

Creates an ordinary session with role `orchestrator` (fleet-wide or project-bound), a project-local
directory `~/.vowp/orchestrators/<name>/` with `orchestrator.json` (0600), the app config
(`opencode.json` or `.mcp.json`) registering `vowp mcp`, and an `AGENTS.md` instructions template.
It starts the app host where the app supports one, takes the lease, and prints `join.desktop`,
`join.phone` and `join.chat` instructions. `--app claude-code` records a degraded orchestrator and
exits non-zero when no claude.ai login exists rather than pretending the host runs. `stop` stops the
host and releases the lease (the session stays); `resume` starts it again; the supervisor resumes
running orchestrators after a restart. JSON: `OrchestratorCard`.

### 6.11 `vowp mcp`

```
vowp mcp --session <id> --surface <id> --token-file <path>
```

A stdio MCP server (plain threads, no async runtime) started by an app. Read tools:
`fleet_status`, `fleet_queue`, `inbox_list`, `capacity_report`, `session_tail`. Write tools:
`inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_set`, `tunnel_open`, `tunnel_close`,
`secret_request`, `session_handoff`, `session_resume`. Every write reads the lease token from the
file on each call, checks the lease and records the actor `session:<id>/surface:<surface-id>`;
reads still work in follow mode. `tunnel_open`/`tunnel_close` refuse on the home.

## 7. Maintenance and hidden commands

| Command | Behaviour |
|---|---|
| `vowp reindex [--session <id>]` | rebuilds `sessions`, `session_events`, `session_natives`, `capsules` rows from `sessions/*/log.jsonl`; prints counts. Safe while the supervisor runs. |
| `vowp backup …` | see §6.8. |
| `vowp schema [<type>]` | with no arg lists the JSON-Schema type names; with a name prints that schema (types.md §14). `snapshot` is the v0.2 snapshot schema. Still needs an initialised home in this build. |
| `vowp --version` | prints `vowp 0.2.0` (the binary version only). |
| `vowp version` | prints `vowp 0.2.0 (<git sha>) schema 1`. |
| `vowp __window <board\|capacity\|supervisor\|planner\|keeper\|inbox\|job> [<id>]` | hidden; what tmux windows run (renders the board or capacity screen, tails `supervisor.log`, tails the planner/keeper session, lists the inbox, tails a job session). Read-only. |
| `vowp __exec --grant <file> [--spool-dir <dir>]` | hidden remote-executor client (sessions.md §9). |
| `vowp __vault-exec --env <NAME> -- <cmd…>` · `vowp __vault-admin probe` · `vowp __vault-admin set\|rm <NAME>` | hidden vault-user helper; `probe` takes no name and checks the helper context and vault directory, `set` reads the value from stdin, never argv. In basic mode `__vault-exec` is a plain exec. |
| `vowp setup __home-init [--root <dir>] [--name <n>] [--harness <h>] [--provider <p>]` | hidden internal bootstrap used by `setup home`; not part of the public contract. |
| `vowp orchestrator __keepalive <name>` | hidden: keeps one orchestrator's lease alive after the CLI exits. |

## 8. `--node <name>` over ssh

1. Look up the node (`NotFound` → exit 3). `is_local` ⇒ ignore the flag.
2. Build argv: the original args with `--node <name>` removed. If the command is interactive (`attach`, `session chat`, `chat`, `board` without `--once`) add `-t`.
3. Execute `ssh -o BatchMode=yes -o ConnectTimeout=10 [-t] <ssh_target> vowp <argv…>` with stdin/stdout/stderr inherited; the remote exit code is returned unchanged; ssh failure (exit 255) ⇒ exit 7 with `{"error":{"kind":"remote",…}}` under `--json`.
4. Arguments are passed as a single remote command string built with POSIX single-quote escaping (`shell_quote`, shared with `vowp-tmux`); no argument is ever interpolated unquoted.
5. `session import` with `--node n`: the bundle file is streamed over ssh stdin (`vowp session import - …`); `session export --node n` writes the bundle locally from the remote's stdout.
6. Cross-node `session switch --to … --node server`: the target's adapter is probed with `vowp --node server doctor --json` before the source is touched. v0.1's bundle move remains available as explicit export/import; v0.2 remote execution keeps the session on the home and moves only the executor (`sessions.md` §9).
7. On a control machine, commands without `--node` are sent to the configured home target; the rules are in §1 and `v0.2/home-and-surfaces.md` §8.

## 9. Named tests (crate `vowp-cli`, hermetic: temp `--home`, temp git repos, fake harness scripts, `FakeWindows` / private tmux socket)

`parse_global_flags_and_node`, `exit_code_mapping_table`, `json_error_shape_not_found`, `init_creates_files_and_registers_project`, `init_refuses_overwrite_without_force`,
`doctor_reports_missing_harness_as_fail_with_fake_path`, `doctor_never_prints_env_values`,
`doctor_flags_invalid_cron_schedule`, `doctor_accepts_valid_schedules`, `job_add_ls_show_roundtrip_json`, `job_add_rejects_cycle_and_unknown_need`,
`job_retry_only_from_failed_stalled_parked`, `job_park_and_cancel_enqueue_request_without_supervisor_apply_directly`, `graph_ascii_and_mermaid_golden`, `graph_export_import_roundtrip`,
`inbox_answer_unparks_job_on_tick`, `cost_by_job_marks_unknown_not_zero`, `loops_pause_resume_and_unknown_loop_exit_3`, `schedule_add_cron_prints_next_run`, `schedule_add_rejects_bad_cron_exit_2`,
`session_export_import_roundtrip_via_cli`, `session_tail_pretty_golden`, `session_switch_with_fake_adapters`, `node_ssh_argv_quoting` (builds the ssh argv without executing it; asserts quoting of args with spaces, quotes and `$`),
`node_flag_remote_failure_exit_7` (uses a fake `ssh` script on PATH exiting 255), `reindex_after_db_delete_via_cli`, `backup_creates_openable_db`, `up_once_runs_one_tick_with_fake_adapters`, `status_check_exit_1_on_stale_heartbeat`.

v0.2 adds the CLI-facing subset of the spec's named tests:
`project_registry_add_ls_weight_pause_resume_archive_and_rm`,
`fleet_status_reads_fresh_snapshot_or_recomputes`, `fleet_queue_includes_waiting_reason`,
`owner_override_above_advice_is_honoured_and_shown`, `capacity_set_history_and_json`,
`migration_rollback_restores_v01_home`, `control_init_writes_control_json`,
`control_routes_status_over_fake_ssh`, `tunnel_open_records_pid_and_port`, `tunnel_close_kills_group`,
`basic_mode_writes_lists_replaces_and_removes`, `one_time_page_serves_a_form_and_stores_exactly_one_value`,
`one_time_page_times_out_without_a_value`, `control_routes_secret_writes_with_tty_and_requests_through_a_tunnel`,
`gc_dry_run_lists_by_class_and_changes_nothing`, `gc_apply_never_deletes_protected_classes`,
`backup_rotation_seven_daily_four_weekly`, `restore_drill_reopens_and_counts_match`,
`setup_plan_lists_every_phase_command_and_file`, `setup_home_stops_at_handoff_with_exit_5`,
`setup_verify_fixture_job_merges`, `orchestrator_new_writes_only_project_local_config`,
`mcp_write_tool_checks_lease_and_records_actor`, `chat_follow_mode_is_read_only`,
`remote_ingest_is_idempotent_by_event_id`. The full per-feature list is in
`v0.2/home-and-surfaces.md` §15.
