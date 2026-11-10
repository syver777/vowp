# cli.md — the `vowp` command line (v0.1)

Implemented by `vowp-cli` on top of `vowp-core`. Every command below is a v0.1 command from the design summary §12 (plus the few additions in `decisions.md` D12).
Crate API and module split: `crates/vowp-cli.md`.

## 1. Global behaviour

```
vowp [GLOBAL FLAGS] <command> [args]
```

| Flag | Meaning |
|---|---|
| `--json` | machine-readable output on stdout (one JSON document; `ls` commands print an array; streaming commands print JSON lines). Human text goes nowhere else. |
| `--node <name>` | run the command on that node over ssh (§4). |
| `--project <name\|path>` | project to act on; default: the nearest ancestor directory of the cwd containing `vowp.toml`, else the only project in the DB, else error `invalid_input` ("no project: run `vowp init`"). |
| `--home <dir>` | node home (default `$VOWP_HOME`, else `~/.vowp`). Tests always pass this. |
| `--no-color`, `-q/--quiet`, `-v/--verbose` | rendering/logging |

- Human output: aligned tables, no colour when stdout is not a TTY or `NO_COLOR` is set, timestamps shown as local `HH:MM` for today / `MM-DD HH:MM` otherwise; `--json` always uses RFC 3339 UTC.
- Ids are accepted by unique prefix (sessions, runs, inbox, releases: ≥ 6 chars of the ULID; ambiguous prefix → exit 2 listing candidates). Jobs need the full slug.
- Mutating commands print what they did in one line (`added job graph-engine (pending)`); with `--json` they print the affected object.
- Reading commands work while the supervisor runs (WAL). Commands that change job state (`retry`, `park`, `cancel`) enqueue a request row (decision D2) and wait up to 5 s for the supervisor to apply it; with no supervisor running they apply it directly through `vowp_graph::transition`.

### 1.1 Exit codes

| Code | Meaning | `ErrorKind` |
|---|---|---|
| 0 | success | — |
| 1 | internal / unexpected error | `Internal`, `Io`, `Db`, `Parse`, `Config`, `AdapterFailed`, `Timeout`, `AdapterUnavailable` |
| 2 | invalid input / usage (clap errors, bad id, bad cron, ambiguous prefix) | `InvalidInput` |
| 3 | not found (job, session, node, …) | `NotFound` |
| 4 | conflict (job exists, state does not allow the action, supervisor already running) | `Conflict` |
| 5 | guard failed or action refused (promotion guard, cwd outside allow-list) | `Guard`, `Refused` |
| 6 | provider limited / auth needed (action needs a provider that is limited) | `Limit`, `Auth` |
| 7 | remote failure (ssh could not connect, remote `vowp` missing) | `Remote` |

`--json` errors: `{"error":{"kind":"not_found","message":"job n99 not found","hint":"vowp job ls"}}` on stdout, exit code as above.

## 2. Setup and health

### `vowp init [--name <project>] [--force] [--no-framework]`
In the current git repo: writes `vowp.toml` (a `[node]` table with `tz_offset_minutes` set from the machine's current UTC offset, the `[project]` table, and the four default profiles from architecture.md §5.2 minus schedules), `.vowp/` (jobs/, checkpoints/), installs the framework files
(`AGENTS.md`, `CLAUDE.md` containing `@AGENTS.md`, `.claude/skills/vowp-{worker,planner,verifier,keeper}/SKILL.md`, `opencode.jsonc`, brief/report templates) from the templates **embedded in the binary**,
appends `.vowp/tmp/` to `.gitignore`, creates `~/.vowp` (db, dirs), registers the local node and the project, and creates the loop rows (`loops` table, default intervals). Existing files are never overwritten without `--force` (skipped files are listed).
Human: `initialised project vowp at /path (8 files written, 2 skipped)`. JSON: `{"project":"vowp","path":"…","written":[…],"skipped":[…]}`. Exit 4 if already initialised and not `--force`.

### `vowp doctor`
Checks (each `ok|warn|fail` with detail): `home` writable; `db` opens + `integrity_check` + schema version; `git` version; `tmux` version (warn if absent); harness probes via adapters
(`claude`, `opencode`, each `Adapter::probe`); `vowp.toml` parses + validates and every `[[schedules]]` cron/interval/timezone parses with `ScheduleSpec::parse` (an invalid entry fails the `schedules` check with the schedule name and error); profile ladders resolve to installed harnesses; providers reachable **only by env-var-name presence** (`PROVIDER_API_KEY` set? — never prints the value, never calls the network);
`systemd --user` unit and cron watchdog installed (warn only); supervisor running; disk space ≥ 1 GB.
Exit 0 if no `fail`, else 1. JSON: `{"ok":true,"checks":[{"name":"tmux","status":"ok","detail":"tmux 3.4"}]}`.

### `vowp up [--foreground|--detach] [--no-tmux] [--once]` · `vowp down [--force] [--kill-windows]` · `vowp status` · `vowp board [--once] [--interval <secs>]`
- `up`: creates/ensures tmux session `vowp-<project>` with windows `board supervisor planner keeper inbox` (skipped with `--no-tmux`), then starts the supervisor. Default (= `--detach`, also accepted explicitly; used by the watchdog): detaches (re-executes itself with `--foreground` under `setsid`/nohup-style, writes `supervisor.lock`); `--foreground` runs the loop in this process (what systemd runs); `--once` runs exactly one tick then exits (used by tests and cron). Exit 4 if a supervisor is already running (`vowp status` shows it).
- `down`: writes `~/.vowp/stop`, waits ≤ 30 s for the lock to disappear; `--force` additionally sends SIGTERM to the pid. Running runs are interrupted and re-queued (`stalled`, not failed). Windows are left for inspection unless `--kill-windows`.
- `status`: one screen — supervisor (pid, uptime, last tick age), jobs by state, slots busy/total, planner/keeper executors, limits with reset times, cost today, inbox open count. JSON: `{"supervisor":{"running":true,"pid":123,"last_tick":"…","heartbeat_age_secs":12,"paused":false},"jobs":{"running":3,…},"slots":{"busy":3,"total":4},"limits":[{"provider":"claude","kind":"quota","resets_at":"…"}],"cost_today_usd":1.23,"inbox_open":2}`. Exit 0 whether or not a supervisor runs (use `--json` `running` field; `vowp status --check` exits 1 if the heartbeat is older than 3 min — the watchdog uses this).
- `board`: refreshing dashboard (clear+redraw every `--interval`, default 2 s; `--once` prints once): jobs by state with the running ones listed (job, executor, age, last event age), slots, limits, cost, inbox, last checkpoint. Reads only the DB.

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
Default: ASCII, exactly `render_ascii` of `vowp-graph` (one line per job ordered by layer: `L1 graph-engine [running] graph engine  <- protocol`). `--mermaid`: exactly `render_mermaid` (`graph TD`, one node per job, edges per kind; no `classDef` — state colours are v0.2). `--export`: YAML (jobs + edges) to the file (stdout if `-`). `--import`: validates then inserts missing jobs/edges (never overwrites; conflicts listed, exit 4). Formats: `crates/vowp-graph.md`.

### `vowp attach [<job>] [--node <n>]`
`tmux attach -t vowp-<project>:job-<id>` (or the session's `board` window with no job). Exec's tmux (inherits the TTY). If tmux is absent or the window is gone: falls back to `vowp session tail --follow` of the job's latest session. Exit 3 if the job has no session.

## 4. Sessions

All `session` subcommands accept a session id prefix; `vowp chat <id>` is an alias of `vowp session chat <id>`.

| Command | Behaviour |
|---|---|
| `session ls [--project p] [--job j] [--state s] [--limit n]` | `ID TITLE EXECUTOR NODE STATE LAST UPDATED EVENTS`; JSON `[Session,…]` |
| `session show <id> [--capsule] [--events <n>] [--lineage]` | metadata, executor history (`session.executor` events), switches, last capsule; `--events n` prints the last n events pretty |
| `session new [--title t] [--executor <exec>] [--cwd dir] [--node n]` | creates an interactive session (role `user`), prints its id (used by `chat`) |
| `session chat <id> [--executor <exec>]` | interactive loop on stdin/stdout: each line is a user turn (`SessionOps::run_turn`, streaming assistant text, tool calls shown as `· tool: <name>`); slash commands `/switch <harness:model[@e][#v]> [--node n]`, `/capsule`, `/export <file>`, `/events [n]`, `/quit`. Exits 6 on an unswitched limit with the hint to `/switch`. Non-TTY stdin: reads turns until EOF (scriptable). |
| `session switch <id> --to <executor> [--node <n>] [--reason user\|limit\|…] [--force]` | runs the switch protocol (`sessions.md` §5): prints the capsule path and the new executor; exit 6 if the target provider is limited, 4 if the session is running a turn and `--force` is not given |
| `session capsule <id> [--print]` | writes a capsule now (`trigger=manual`), prints its id (or the markdown with `--print`) |
| `session export <id> [--out <file>\|-o <file>] [--tail <n>]` | writes a `vowp-bundle-v1` JSON bundle (default `./<id>.vowp-bundle.json`, tail 20 events) |
| `session import <file> [--node n] [--executor <exec>] [--cwd <dir>]` | validates checksum/version, creates a **child** session (`parent_session_id`), prints the new id; the seed capsule is attached |
| `session watch [--claude-dir <dir>] [--once]` | *nice-to-have*: records interactive Claude Code sessions (read-only transcript importer) into canonical sessions; prints `watching N transcripts`; exit 1 if the dir is unreadable |
| `session tail <id> [--follow] [--pretty] [--from <seq>]` | prints events (JSONL, or `--pretty` one line per event); `--follow` polls the file (200 ms). This is what job windows run (`vowp __window job <id>`). |

Pretty line format: `HH:MM:SS #seq type  summary` e.g. `19:30:12 #6 tool.call  shell: cargo test`.

## 5. Inbox, cost, nodes, loops, schedules, release

### `vowp inbox ls [--all] · show <id> · ask <title> [--body t] [--job <id>] [--kind question\|approval\|notice] [--option a --option b] · answer <id> <text\|--approve\|--reject> [--by <name>] · dismiss <id>`
`ask` creates an item (used by planner/keeper agents and scripts; with `--job` the job is parked with `parked(Question)` for kinds `question|approval`, never for `notice`; prints the item id). `ls` lists open items (`ID KIND JOB TITLE AGE`); `--all` includes answered. `answer` stores the answer, sets `answered`, and (for an item with `job_id`) moves the parked job to `ready` on the next tick (`loops.md` §9). `--approve/--reject` are shorthand answers `approve`/`reject` valid for `approval` items. Exit 4 if already answered.

### `vowp cost [--by job|role|provider|model|day] [--since <dur\|date>] [--job id]`
Table `KEY RUNS USD INPUT OUTPUT CACHE_R SOURCE`; totals row; `SOURCE` is `receipt`, `price-table`, `unknown` or `mixed`, derived from `CostRow.receipt_entries`/`price_table_entries`/`unknown_entries`: one source present → that source, more than one → `mixed`. Unknown costs are shown as `?` and counted separately (never as 0). JSON: `{"by":"job","rows":[{"key":"graph-engine","runs":3,"usd":0.42,"tokens":{…},"source":"receipt"}],"total":{…}}`.

### `vowp node add <name> <ssh-target> · ls · rm <name>`
`add` records the node and tests `ssh -o BatchMode=yes <target> vowp --version` (stores the result in `capabilities`; failure is a warning unless `--strict`). `ls`: `NAME TARGET LOCAL LAST_SEEN VERSION`.

### `vowp loops [pause <loop> | resume <loop>]`
No args: table of loop rows (`NAME KIND STATE INTERVAL LAST NEXT STATUS FAILS`). `pause/resume` set `loops.state` (L1 itself cannot be paused; use `down`). Exit 3 for an unknown loop name.

### `vowp schedule add "<spec>" <job-template|loop|command> … · ls · pause <name> · resume <name> · rm <name>`
`<spec>` is a 5-field cron string or `every 30m`. Forms:
```
vowp schedule add "0 7 * * 1-5" loop l4-planner --name morning-report --tz +02:00 --missed run_once
vowp schedule add "every 30m" command --name hb -- vowp status --check
vowp schedule add "0 1 * * *" job-template --name dep-update --template-file deps.json   # a NewJob JSON (types.md §7)
```
Validated with the same rules as `[[schedules]]` (architecture.md §5.3); prints the computed next run. `ls`: `NAME SPEC ACTION STATE LAST NEXT STATUS`.

### `vowp promote <change> --to <env> [--yes] [--dry-run]` · `vowp rollback <release> [--yes]` · `vowp releases [--env e] [--limit n]`
Work→Production (`crates/vowp-core.md` §promote). `<change>` = commit/ref/job id. `promote` runs: guards → serving bank → diff candidate vs serving → approval (an inbox `approval` item unless `--yes` **and** `approval = "policy"`; `--yes` alone never bypasses a human approval) → deploy → probe → release record. `--dry-run` runs guards + diff only. Exit 5 when a guard fails or the diff is unexpected (release recorded as `refused`); exit 0 when staged awaiting approval (prints the inbox id). `rollback` redeploys the banked serving copy. `releases`: `ID ENV CHANGE STATE FROM→TO CREATED`.

## 6. Maintenance and hidden commands

| Command | Behaviour |
|---|---|
| `vowp reindex [--session <id>]` | rebuilds `sessions`, `session_events`, `session_natives`, `capsules` rows from `sessions/*/log.jsonl`; prints counts. Safe while the supervisor runs. |
| `vowp backup [--to <file>]` | SQLite online backup to `~/.vowp/backups/vowp-<ts>.db` (or the file); prints the path and size. |
| `vowp schema [<type>]` | with no arg lists the JSON-Schema type names; with a name prints that schema (types.md §14). |
| `vowp --version` / `vowp version` | `vowp 0.1.0 (<git sha>) schema 1` |
| `vowp __window <board\|supervisor\|planner\|keeper\|inbox\|job> [<id>]` | hidden; what tmux windows run (renders the board, tails `supervisor.log`, tails the planner/keeper session, lists the inbox, tails a job session). Read-only. |

## 7. `--node <name>` over ssh

1. Look up the node (`NotFound` → exit 3). `is_local` ⇒ ignore the flag.
2. Build argv: the original args with `--node <name>` removed. If the command is interactive (`attach`, `session chat`, `board` without `--once`) add `-t`.
3. Execute `ssh -o BatchMode=yes -o ConnectTimeout=10 [-t] <ssh_target> vowp <argv…>` with stdin/stdout/stderr inherited; the remote exit code is returned unchanged; ssh failure (exit 255) ⇒ exit 7 with `{"error":{"kind":"remote",…}}` under `--json`.
4. Arguments are passed as a single remote command string built with POSIX single-quote escaping (`shell_quote`, shared with `vowp-tmux`); no argument is ever interpolated unquoted.
5. `session import` with `--node n`: the bundle file is streamed over ssh stdin (`vowp session import - …`); `session export --node n` writes the bundle locally from the remote's stdout.
6. Cross-node `session switch --to … --node server`: exports a bundle locally, imports on `server`, runs the switch there, and records a `switch` event with `from_node`/`to_node` on both sides (`sessions.md` §5 and §6).

## 8. Named tests (crate `vowp-cli`, hermetic: temp `--home`, temp git repos, fake harness scripts, `FakeWindows` / private tmux socket)

`parse_global_flags_and_node`, `exit_code_mapping_table`, `json_error_shape_not_found`, `init_creates_files_and_registers_project`, `init_refuses_overwrite_without_force`,
`doctor_reports_missing_harness_as_fail_with_fake_path`, `doctor_never_prints_env_values`,
`doctor_flags_invalid_cron_schedule`, `doctor_accepts_valid_schedules`, `job_add_ls_show_roundtrip_json`, `job_add_rejects_cycle_and_unknown_need`,
`job_retry_only_from_failed_stalled_parked`, `job_park_and_cancel_enqueue_request_without_supervisor_apply_directly`, `graph_ascii_and_mermaid_golden`, `graph_export_import_roundtrip`,
`inbox_answer_unparks_job_on_tick`, `cost_by_job_marks_unknown_not_zero`, `loops_pause_resume_and_unknown_loop_exit_3`, `schedule_add_cron_prints_next_run`, `schedule_add_rejects_bad_cron_exit_2`,
`session_export_import_roundtrip_via_cli`, `session_tail_pretty_golden`, `session_switch_with_fake_adapters`, `node_ssh_argv_quoting` (builds the ssh argv without executing it; asserts quoting of args with spaces, quotes and `$`),
`node_flag_remote_failure_exit_7` (uses a fake `ssh` script on PATH exiting 255), `reindex_after_db_delete_via_cli`, `backup_creates_openable_db`, `up_once_runs_one_tick_with_fake_adapters`, `status_check_exit_1_on_stale_heartbeat`.
