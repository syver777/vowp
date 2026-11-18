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
(`claude`, `opencode`, each `Adapter::probe`; `acp` only when `[harnesses.acp]` or an ACP profile rung is configured); `vowp.toml` parses + validates and every `[[schedules]]` cron/interval/timezone parses with `ScheduleSpec::parse` (an invalid entry fails the `schedules` check with the schedule name and error); profile ladders resolve to installed harnesses; providers reachable **only by env-var-name presence** (`PROVIDER_API_KEY` set? — never prints the value, never calls the network);
`systemd --user` unit and cron watchdog installed (warn only); supervisor running; disk space ≥ 1 GB.
Exit 0 if no `fail`, else 1. JSON: `{"ok":true,"checks":[{"name":"tmux","status":"ok","detail":"tmux 3.4"}]}`.

### `vowp up [--foreground|--detach] [--no-tmux] [--no-migrate] [--once] [--replace]` · `vowp down [--drain] [--force] [--kill-windows]` · `vowp status [--check]` · `vowp board [--once] [--interval <secs>]`
- `up`: runs **one node supervisor for every active project**. It registers the current checkout
  when it has a `vowp.toml`, migrates a v0.1 home first unless `--no-migrate` is given, ensures the
  tmux sessions (`vowp-home` with `board` and `capacity`; `vowp-<project>` with
  `board supervisor planner keeper inbox`; skipped with `--no-tmux`), then starts the supervisor.
  Default (= `--detach`, also accepted explicitly; used by the watchdog): detaches (re-executes
  itself with `--foreground` under `setsid`/nohup-style, writes `supervisor.lock`); `--foreground`
  runs the loop in this process (what systemd runs); `--once` runs startup plus exactly one node
  tick then exits (used by tests and cron). `--replace` drains a running supervisor first: it
  writes `<home>/drain`, wakes the supervisor, waits for its runner threads and gate tasks to
  reach zero, and only then continues with the normal `up` path, so pending store migrations run
  with no active children before the updated binary resumes dispatch. Exit 4 if a supervisor is
  already running without `--replace`; exit 5 on a control machine.
- `down`: writes `~/.vowp/stop`, waits ≤ 30 s for the lock to disappear; `--force` additionally sends SIGTERM to the pid. Running runs are interrupted and re-queued (`stalled`, not failed). `--drain` instead writes `<home>/drain` and wakes the supervisor: every project ticks in maintenance mode (harvest, gates, merge, inbox, snapshot, heartbeat) and admits no new work, and the process exits cleanly once active runner threads and gate tasks reach zero. The drain wait is much longer than a plain stop so in-flight runs can finish; `--force` still escalates. Windows are left for inspection unless `--kill-windows`.
- `status`: one screen — supervisor (pid, uptime, last tick age), every project with jobs by state, slots busy/total, planner/keeper executors, limits with reset times, cost today, inbox open count. JSON keeps the v0.1 keys and adds `home` and `projects`. Exit 0 whether or not a supervisor runs (use `--json` `running` field; `vowp status --check` exits 1 if the heartbeat is older than 3 min — the watchdog uses this).
- `board`: refreshing dashboard (clear+redraw every `--interval`, default 2 s; `--once` prints once). With one project selected it prints the v0.1 single-project frame: jobs by state, the running jobs (job, executor, age, last event age), slots, limits, cost, inbox and the last checkpoint. With several projects, or no project selected, it prints the fleet frame: every project's state, jobs by state, running/queued/blocked jobs, last merge, gate failures and cost today, then provider limits and the open inbox count; the fleet frame prints no slots and no last checkpoint. It always recomputes from the databases; `board.json` is only a convenience copy written by the supervisor.

## 3. Planning and the graph

### `vowp plan "<goal>" [--profile <name>] [--dry-run]`
Starts a planner turn (session role `planner`, executor = first available rung of `profile`, default `planner`) with a prompt containing the goal, the framework rules for planners, the current graph digest and the exact `vowp job add` syntax. The planner is expected to call `vowp job add/update` and write briefs; the command streams a one-line-per-event summary and, at the end, prints the jobs created (`graph diff`) for the human to review. `--dry-run`: the planner is told not to run `vowp` commands and to output the graph as YAML instead (printed, nothing stored). Exit 6 if every rung is limited.

### `vowp job …`

| Command | Behaviour |
|---|---|
| `job add --id <slug> --title <text> --kind <k> [--role <r>] [--profile <p>] [--needs a,b] [--owns glob,…] [--reads glob,…] [--gates <set\|short,…>] [--priority n] [--max-attempts n] [--brief-file <path>\|--brief -] [--placement home\|any\|node:<name>]` | validates (`NewJob`), inserts job + edges, writes the brief (generated front-matter + body from file/stdin; with neither, a template brief), state `pending` (→ `ready` on the next tick when deps are done). `--placement` is parsed by `Placement::parse` and stored in the `job_placement` side table in the same success path; `node:<name>` (and `node:<name>+home`) is refused with exit 5 **before anything is written** when `offload.enabled = false`. Exit 4 if the id exists; 2 if a need is unknown, a cycle would form or the placement grammar is invalid. `--role` default by kind (`build→worker`, `verify→verifier`, `plan→planner`, `decision→keeper`). |
| `job update <id> [--title] [--priority] [--profile] [--owns] [--reads] [--gates] [--add-needs a,b] [--remove-needs a] [--note text]` | `JobPatch`. Only jobs not in `running/gating/reviewing/merging` may change `owns`/`gates` (exit 4). |
| `job ls [--state s,…] [--kind k] [--role r] [--all]` | table: `ID STATE KIND ROLE PRIO NEEDS ATTEMPTS UPDATED NOTE`; default hides `done/cancelled/replanned` unless `--all`. JSON: `[Job,…]`. |
| `job show <id> [--brief] [--report] [--runs] [--gates]` | the job, its edges, last runs (executor, outcome, cost), last gate results with evidence tails; `--brief/--report` print the files. When the job has a `job_placement` row, human output adds `placement: <value>` and `--json` adds a `placement` key; `job ls` keeps its table and JSON `Job` array unchanged. |
| `job retry <id>` | by state: `failed` → `ready` (event `Retry`, T32 resets `attempts`, `continues` and `hang_restarts`), `parked` → `ready` (`Resolved`), `stalled` → `running` (`KeeperRestart`). Exit 4 from any other state. |
| `job park <id> [--reason text]` | any non-terminal → `parked` (`parked_reason=manual`); stops its run (`interrupt` then `stop`). To resume use `job retry`. |
| `job nudge <id>` · `job restart <id>` · `job replan <id>` · `job fail <id> --reason t` | keeper commands (loops.md §7): enqueue a request row applying the corresponding `JobEvent` (`KeeperNudge|KeeperRestart|KeeperReplan|KeeperFail`); exit 4 if the job's state does not allow it. |
| `job cancel <id>` | any non-terminal → `cancelled`; stops the run, releases the ownership lock (worktree and branch are kept on disk for inspection). Dependents stay `pending`; `vowp job ls` flags them `blocked-by-cancelled`. |

### `vowp graph [--mermaid] [--export <file.yaml>] [--import <file.yaml>] [--all] [--json]`
Default: ASCII, exactly `render_ascii` of `vowp-graph` (one line per job ordered by layer: `L1 graph-engine [running] graph engine  <- protocol`). `--json`: one document `{"jobs": [Job,…], "edges": [Edge,…]}` for the normal read path (jobs ordered by id, edges in store/graph order; `--all` keeps the same project selection as ASCII). `--mermaid`: exactly `render_mermaid` (`graph TD`, one node per job, edges per kind; no `classDef`) and takes precedence over `--json`, so `vowp graph --json --mermaid` prints only Mermaid. `--export`: YAML (jobs + edges) to the file (stdout if `-`). `--import`: validates then inserts missing jobs/edges (never overwrites; conflicts listed, exit 4). Formats: `crates/vowp-graph.md`.

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
| `chat --agent --session <id> [--executor <exec>] [--node <n>]` | the Phase 1 local surface: the agent holds the lease and runs the adapter on this machine |
| `session switch <id> --to <executor> [--node <n>] [--reason user\|limit\|…] [--force]` | runs the switch protocol (`sessions.md` §5): prints the capsule path and the new executor; exit 6 if the target provider is limited, 4 if the session is running a turn and `--force` is not given. A remote target is probed with `vowp doctor` first. |
| `session capsule <id> [--print]` | writes a capsule now (`trigger=manual`), prints its id (or the markdown with `--print`) |
| `session export <id> [--out <file>\|-o <file>] [--format bundle\|asif] [--tail <n>] [--redact <literal>]... [--redact-file <file>]... [--keep-host-paths] [--report <file>] [--strict\|--allow-loss]` | writes a `vowp-bundle-v1` JSON bundle (default `./<id>.vowp-bundle.json`, tail 20 events). With `--format asif` it writes one deterministic `.asif.zip` (default `./<id>.asif.zip`) or, with `--out -`, the exact package bytes; see the ASIF subsection below. |
| `session import <file> [--format bundle\|asif] [--node n] [--executor <exec>] [--cwd <dir>] [--same-id] [--branch <id>] [--report <file>] [--strict\|--allow-loss]` | validates checksum/version, creates a **child** session (`parent_session_id`), prints the new id; the seed capsule is attached. `--same-id` keeps the bundle id only when no local row or session directory exists, writes no parent and copies the bundle tail into the log; a present id is exit 4 and nothing is changed. With `--format asif` it creates a new idle session from an ASIF package, standalone document, directory or stdin; see the ASIF subsection below. |
| `session pull <node> <session> [--same-id \| --full]` | runs on the local machine: exports the bundle from `node` over ssh, stages it in a private temporary directory, verifies format/protocol/checksum, imports a **child** (`parent_session_id` = source id) and prints both ids. JSON `{"node","source_session","child_session"}`. With `--same-id` it imports under the source id with no parent and JSON `{"node","source_session","same_id_session"}`. With `--full` it streams the whole `sessions/<id>/` directory (full log, blobs, capsules, `adapters.json`) as a `vowp-session-dir-v1` manifest plus file bytes, verifies every digest and size (1 GiB cap) before the final rename, reindexes, and prints JSON `{"node","source_session","full_session","files","bytes"}`; it never creates a child or rewrites a bundle. When `--same-id` or `--full` finds the source id present locally, the pull exits 4 with a divergence report naming both heads (last seq, last event type/time, state and executor where available) and the note `choose which side advances; no merge performed`; `--json` returns `{"error":{"kind":"conflict","code":"session_divergence","message","hint","node","source_session","local","remote","note"}}`. Nothing is overwritten, forced or merged. A failed remote run, malformed/truncated/corrupt snapshot, over-cap directory or checksum mismatch imports nothing and leaves no `sessions/<id>/`. |
| `session push <node> <session> [--same-id]` | runs on the local machine: resolves the local session by id or prefix, exports one `vowp-bundle-v1` in memory (no temporary file in the project) and streams it to `vowp --project <project> --json session import -` on `node` over ssh. The default creates a **child** on the remote (`parent_session_id` = source id) and prints `pushed session <local> to <node> as child <remote-child>`; JSON `{"node","source_session","child_session"}`. With `--same-id` it first runs the remote read-only `session show --json --events 1 <id>`: remote exit 3 means absent and the push imports under the source id with no parent, printing `pushed session <local> to <node> with same id <id>`; JSON `{"node","source_session","same_id_session"}`. When the id already exists on the remote, the push exits 4 with the same divergence document as `session pull` (`kind: conflict`, `code: session_divergence`, both heads and `choose which side advances; no merge performed`) and exports, imports or mutates nothing. An unknown local session is exit 3 before any ssh; a local node is exit 2 with the export/import hint. Any other remote pre-check failure, local export failure or remote import failure relays its exit code and output and prints no success line. |
| `session watch [--claude-dir <dir>] [--once] [--follow]` | records interactive Claude Code transcripts (read-only, byte-offset importer) into one canonical session per Claude uuid; prints `watching N transcripts`; `--follow` polls every two seconds until Ctrl-C, and a TTY run without `--once` or `--follow` follows the same way; `--once`, and a non-TTY run without `--follow`, make one incremental pass; exit 1 if the dir is unreadable |
| `session tail <id> [--follow] [--pretty] [--from <seq>]` | prints events (JSONL, or `--pretty` one line per event); `--follow` polls the file (200 ms) and does not take the lease. This is what job windows run (`vowp __window job <id>`). |

Pretty line format: `HH:MM:SS #seq type  summary` e.g. `19:30:12 #6 tool.call  shell: cargo test`.

### ASIF export and import (`--format asif`)

ASIF 0.4 is a review draft, not a released standard. `--format bundle` remains the default for
both commands and keeps its exact output, JSON, exit codes and remote paths. ASIF is a second
format, not a replacement; every ASIF command reports declared losses and the VOWP operations
that are not captured. It does not emit or interpret a continuation object, does not import a
native harness transcript and does not claim a lossless round trip.

```text
vowp session export <id> --format asif [--out <file>|-]
                     [--redact <literal>]... [--redact-file <file>]...
                     [--keep-host-paths] [--report <file>]
                     [--strict|--allow-loss] [--json]
vowp session import <file>|- --format asif [--node <name>]
                    [--cwd <dir>] [--branch <id>] [--report <file>]
                    [--strict|--allow-loss] [--json]
```

Export writes one deterministic ZIP package (`ZIP_STORED`). The default path is
`./<resolved-session-id>.asif.zip`; `--out -` writes the exact package bytes to stdout and is
refused (exit 2) on a TTY. `--json` conflicts with `--out -` (exit 2). `--tail` is invalid for
ASIF (exit 2). Export is read-only: it writes no export capsule, takes no session lock and moves
no lease. A torn or corrupt `log.jsonl` is invalid input (exit 2) and leaves no output.

Import accepts a `.asif.zip` package, a standalone `session.json`, a directory containing
`session.json`, or `-` stdin. It always creates a new idle session with a fresh VOWP ULID.
`--same-id` and `--executor` are invalid (exit 2). `--cwd` defaults to the selected project
path and must exist. A multi-branch document is exit 2 without `--branch`; the error names the
available branch ids when the input is a standalone document or directory, and otherwise tells
the caller how to list them. `--report` with a remote `--node` is exit 2 (the full report stays
on the node; a node registered local runs the local command); a directory input with `--node`
is also exit 2. A failed import leaves no session row, log, blob, capsule, lease or partial
report.

Losses and strict mode:

- Default: declared omissions and normalizations succeed and are reported. Recoverable
  integrity/ambiguity problems are still refused by the adapter.
- `--allow-loss`: passes through to the adapter, so recoverable duplicate/orphan/ambiguity
  problems become declared losses. It does not make unsupported semantics safe.
- `--strict`: asks the adapter to declare those recoverable problems, then refuses any
  `Blocking` or `Integrity` loss with exit 5. No new package/report is kept; an existing
  `--out` or `--report` file is left untouched. Inherent `Omission`/`Normalization` losses stay
  non-fatal. `--strict` and `--allow-loss` conflict with each other (exit 2).
- Human output prints the loss count and the unique loss kinds to stderr; zero losses is stated
  explicitly. `--json` keeps the summary (`loss_count`, `losses`, `unsupported`); `--report`
  writes the full `vowp-asif-report-v1` document. Redaction values are never echoed.

`session export --format asif --json` prints:

```json
{
  "format": "asif",
  "path": "/srv/work/vowp/01KA....asif.zip",
  "session": "01KA...",
  "bytes": 18342,
  "report_path": null,
  "report": {
    "adapter_version": "0.1.1",
    "asif_version": "0.4",
    "asif_pin": "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f",
    "source_session": "01KA...",
    "target_session": null,
    "counts": {"events_in": 42, "events_out": 42, "resources": 3, "capsules": 1},
    "loss_count": 2,
    "losses": [
      {"kind": "cwd-omitted", "stage": "translation", "scope": "environment", "message": "..."},
      {"kind": "contexts-reconstructed", "stage": "normalization", "scope": "contexts", "message": "..."}
    ],
    "unsupported": ["vowp.jobs", "vowp.gates", "vowp.leases", "vowp.project-db"]
  }
}
```

`session import --format asif --json` prints the `AsifImportResponse` document: the new
`session` row, the same `report` summary with `target_session` set, and non-fatal reindex
`warnings`. The warnings are not losses: a source project or job that is absent locally leaves
the session's `project`/`job_id` null and is reported only on stderr.

`--node` uses the existing SSH transport. Export runs the hidden remote
`session export <id> --format asif --out - --asif-stream`, reads one envelope line, streams
exactly the announced bytes to a private temporary file (or `<out>.tmp`), verifies the byte
count and SHA-256, and only then renames the package or writes it to stdout. A truncated stream,
extra byte, malformed envelope or digest mismatch is exit 7 and leaves no local package. A
strict refusal is exit 5 with no stream. `--redact-file` is read locally and forwarded as
individual `--redact` literals; the file is never sent. `--report` with `--node` is exit 2.
Import streams the file or stdin to the remote and relays its `AsifImportResponse` and exit
code; nothing is created locally on failure.

### `vowp chat <id> [--executor <exec>] [--node <n>] [--follow] [--takeover]`
The guaranteed terminal surface and the reference implementation of the lease rules. It acquires
the session lease (or follows read-only), runs the turn on this surface's machine by default, and
supports `/switch <executor> [--node n]`, `/status`, `/takeover`, `/follow`, `/capsule`, `/quit`.
A takeover by another surface prints
`Taken over by phone at 14:05. This surface is now read-only (follow mode).` and keeps showing new
events. See `sessions.md` §8–§11.

### `vowp chat --agent --session <id> [--executor <exec>] [--node <n>]`
The Phase 1 local surface on the Mac. `--session` is equivalent to the positional id. The command
loads `~/.vowp-agent/agent.json`, the node key and the paired surface token, then the agent client
acquires the lease, sends `TurnBegin` with a project-relative `PathRef`, runs the configured
adapter on this machine and streams spooled events to the home gateway. The home log remains the
only log. Assistant text, tools, errors and notes print in the same 80-column format as the home
chat. Ctrl-C stops the adapter and releases the lease. A takeover or lease loss prints
`This surface is read-only: <reason>` and exits 0; a full spool exits 7. `--follow` and
`--takeover` are refused in agent mode. The parser records `--agent` and the CLI dispatches to
the agent bridge before the control-machine route and before `Ctx::build`, so the Mac needs no
local VOWP home database and the command is never forwarded to the home. The local checkout's
`vowp.toml` supplies the project name, working directory and default executor (the first rung of
the `worker` profile; `--executor` overrides it). The home path keeps its required positional id,
help tree and missing-id error.

The Mac-side v0.4 agent commands (`vowp agent enroll`, `vowp agent paths`, `vowp agent run`,
`vowp agent status`) live in §12. They use `~/.vowp-agent` rather than the home/project store.

### Hidden session interfaces

These are implementation interfaces, not help-listed user commands. `vowp session turn-begin`,
`vowp session ingest`, `vowp session lease`, `vowp session takeover` and `vowp session dir-export`
are hidden subcommands of `vowp session`; `vowp __exec`, `vowp __vault-exec` and
`vowp __vault-admin` are hidden top-level commands. They still accept `--json` where a document is
printed; the streaming ones print one JSON object per line.

```
vowp session turn-begin <session> --executor <exec> --purpose <p> --node <n> --request <file|->
vowp __exec --grant <file> [--spool-dir <dir>]
vowp session ingest <session> --run <run-id> [--replay] [--json]
vowp session lease <session> [--acquire <kind> --holder <h> --node <n>] [--json]
vowp session takeover <session> --kind <kind> --holder <h> --node <n> [--json]
vowp session dir-export <session> [--max-bytes <n>]
```

`dir-export` is the source half of `session pull --full`: it prints one `vowp-session-dir-v1`
manifest line and then the listed file bytes on stdout. It is read-only and takes no session lock.
`session export --format asif --out - --asif-stream` is the hidden source half of a remote ASIF
export: it prints one `vowp-asif-export-stream-v1` envelope line (`format`, `session`, `bytes`,
`sha256`, `report`) and then exactly the package bytes. It is read-only and is not help-listed.

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

### `vowp node add <name> <ssh-target> · ls · rm <name> [--yes]`
`add` records the node and tests `ssh -o BatchMode=yes <target> vowp --version` (stores the result in `capabilities`; failure is a warning unless `--strict`). `add <name> -` registers a surface-only executor node (a laptop that dials the home; the home never dials it). `ls`: `NAME TARGET LOCAL LAST_SEEN VERSION`. `rm` never prompts, accepts `--yes` for scripts, removes the node row and purges its stranded enrollment codes; an unknown node is exit 3.

### `vowp node enroll <name> [--label <label>]... [--replace] · revoke <name> [--yes] · show <name> [--json]`
These commands are home-only; a control machine exits 5 because the code and credentials need the home store. `enroll` creates an `agent`-kind `pending` node and a one-time 52-character code valid for ten minutes, then prints the code once and the command `vowp agent enroll --target <home-ssh-target> --code-file -`. The code is never written to the log or a file; an existing node is exit 4. `--replace` rotates a `pending` node's code in one transaction: the old pending code is refused immediately, the labels are updated, and the node stays `pending`. An `active` or `revoked` node is refused with exit 4 (revoke or remove it first) so a live enrollment is never wiped. `revoke` marks the node `revoked` and revokes every credential (without `--yes` it refuses with exit 5); an unknown node is exit 3. Phase 0 has no live connection registry, so the gateway sends `Revoked` on the node's next frame. `show` prints the redacted `NodeCard`: kind, status, computed presence, labels, latest capacity, mapped projects, agent version, protocol, last error and `last_seen_at`. It never prints a path map, an absolute local path or a secret name. `node ls` and its JSON `Node` shape are unchanged.

JSON: `enroll` and `enroll --replace` print `{"node":"mac-studio","code":"…","expires_at":"2026-11-14T10:10:00.000Z","command":"vowp agent enroll --target <home-ssh-target> --code-file -"}` (human output starts with `created pending node` or `replaced pending node`); `revoke` prints `{"node":"mac-studio","status":"revoked","credentials_revoked":1}`; `show` prints the flattened `NodeCard` plus `"last_seen_at"` and no path-map field. `rm --json` prints the removed `Node` row.

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
vowp capacity reload [--check]
vowp capacity history [--since <dur|date>] [--bucket 5m|1h|1d]
```

`show`/`report` return `CapacityReport`: the latest host sample (or none), the owner target and
advice, running run count and p90 estimates, brakes, reserves, estimates and waiting jobs. `show`
and `report` also print the effective provider caps and whether a config reload is pending; their
`--json` document carries the extra `provider_caps` object and `reload_pending` boolean. `set`
writes one `capacity_targets` history row. A target above the advised value needs `--override`
(exit 5 otherwise); the brakes still apply and `capacity show` prints
`owner override: 8 (advised 5)`. The board does not carry capacity data. The AI advisor only
proposes: it opens a `capacity` inbox item and never changes the target itself.

`reload --check` parses and validates the project `vowp.toml` plus node defaults, then prints the
target and provider caps it would apply. It writes no target row, ledger row or reload state and
does not need a running supervisor. `reload` without `--check` asks the running supervisor to
apply the on-disk config and prints the effective values; with no live supervisor it exits 5 and
prints a `vowp up` hint. Both forms are read-only for `vowp.toml` and never restart the supervisor
or stop a run. An invalid file is refused with exit 1; the supervisor keeps the previous config,
logs the path and error and opens a node-inbox notice.

`history` reads the stored 5-minute `capacity_rollups` from the home's global database. `--since`
defaults to 24 hours and accepts a duration or `YYYY-MM-DD`; `--bucket` defaults to `5m` and also
accepts `1h` or `1d` as read-time aggregations weighted by `samples`. It opens the store with
`Store::open_read_only`, writes no target, advice, ledger or rollup row and is safe while the
supervisor runs. A `--since` before the retained 30-day window is clamped and a note is printed;
an unknown bucket is exit 2. Human output is one row per point
(`AT SAMPLES MEM_MIN/AVG/MAX LOAD1 DISK_MIN SOURCE`), a `window:` footer with the effective
`since`, `bucket_secs` and `retention_secs`, and a cost footer naming `vowp cost --by day`; an
empty range prints `history: no rollups in the window`. `--json` prints `CapacityHistory`
(`generated_at`, `since`, `until`, `bucket_secs`, `retention_secs`, `points`). Raw samples are kept
48 hours and rollups 30 days; a missing bucket is a gap, not a zero.

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
vowp orchestrator new [--name <n>] [--project <p>] --app opencode|claude-code|chat [--node <n>] [--executor <e>] [--force]
vowp orchestrator ls
vowp orchestrator stop <name>
vowp orchestrator resume <name>
```

Creates an ordinary session with role `orchestrator` (fleet-wide or project-bound), a project-local
directory `~/.vowp/orchestrators/<name>/` with `orchestrator.json` (0600), the app config
(`opencode.json` or `.mcp.json`) registering `vowp mcp`, and an `AGENTS.md` instructions template.
It starts the app host where the app supports one, takes the lease, and prints `join.desktop`,
`join.phone` and `join.chat` instructions. `--node <n>` names the local node that runs the app (it
is not an SSH redirect for this command); the node materializes its own private config and reports
`running` only after the app is healthy. `--app claude-code` records a degraded orchestrator and
exits non-zero when no claude.ai login exists rather than pretending the host runs. `stop` stops the
host and releases the lease (the session stays); `resume` starts it again; the supervisor resumes
running orchestrators after a restart. JSON: `OrchestratorCard`.

An opencode app may write its own 0600 `service.json` or, on macOS, print its readiness on stdout
(`server listening on http://127.0.0.1:<port>` and `server password <secret>`); the agent parses
the stdout lines and materializes a private 0600 `service.json`, so no file must exist before the
start. A default `opencode`, `claude` or `vowp` missing from `PATH` is `capability_missing` and
starts no process. The default `chat` app is started as `vowp chat --agent <session>` with
`VOWP_AGENT_HOME` set to the agent state directory and a piped stdin held open; an early exit is
`app_exited`. `OrchestratorCard.last_error` carries the redacted failure reason for a `failed`,
`degraded` or `node_offline` record, is `null` for `starting`, `running` and `stopped`, and is
cleared by a clean `resume` or `stop`. It never contains a port, password, secret value or absolute
local path.

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
| `vowp --version` | prints `vowp 0.3.0` (the binary version only). |
| `vowp version` | prints `vowp 0.3.0 (<git sha>) schema 1`. |
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
5. `session import` with `--node n`: the bundle file is streamed over ssh stdin (`vowp session import - …`); `session export --node n` writes the bundle locally from the remote's stdout. `session pull <node> <session>` uses that remote export, verifies the bundle locally and imports the child here; the pull never runs on the remote node. `--same-id` on either command is forwarded to the importer and preserves the source id only when it is absent locally. `session pull --full` instead calls the remote hidden `session dir-export` and streams the manifest plus raw directory files; it stages, verifies and reindexes locally under the source id only when absent. If `--same-id` or `--full` finds the id already local, the pull prints the two heads and exits 4 without touching the local row or log; the full path aborts the snapshot stream and asks the source's read-only `session show --json --events 1` for its head because the directory manifest carries no log position. No pull takes a source log lock; a bundle export writes its usual export capsule on the source, while the directory export is read-only. `session push <node> <session>` is the reverse and also never runs on the remote node: it exports the bundle locally in memory and streams it to the remote `session import -`; `--same-id` reads the remote head first and reports divergence without writing when the id exists there.
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
`v0.2/home-and-surfaces.md` §15. The v0.3 pull slice adds
`session_pull_imports_a_child_and_prints_both_ids`,
`session_pull_refuses_a_corrupt_bundle_before_importing` and
`session_pull_reports_a_failed_remote_export_and_imports_nothing`. The same-id slice adds
`session_pull_same_id_imports_the_origin_id_without_a_parent`,
`session_pull_same_id_reports_divergence_without_mutating_it`,
`session_pull_same_id_refuses_a_corrupt_bundle_before_importing` and
`session_import_same_id_keeps_the_bundle_id_and_refuses_a_second_import`. The full-directory slice
adds `session_pull_full_imports_the_origin_id_with_the_full_log`,
`session_pull_full_reports_divergence_without_mutating_it`,
`session_pull_full_prefix_of_an_existing_session_keeps_the_local_conflict`,
`session_pull_full_refuses_a_corrupt_snapshot_without_leaving_a_directory`,
`session_pull_full_refuses_a_log_with_a_missing_blob`,
`session_pull_full_refuses_an_over_cap_snapshot_before_copying` and
`session_pull_full_reports_a_failed_remote_export_and_imports_nothing`; the default child path
remains `session_pull_imports_a_child_and_prints_both_ids`.

The v0.4 agent slice adds `agent_status_without_enrollment_exits_three`,
`agent_status_prints_json`, `agent_enroll_help_names_code_file` and
`agent_enroll_accepts_code_file_stdin`; the home-side node slice adds
`node_enroll_creates_a_pending_node_and_prints_the_code_once`, `node_enroll_refuses_a_duplicate_node`,
`node_revoke_marks_the_node_and_revokes_credentials`, `node_show_returns_the_redacted_card` and
`node_enroll_is_home_only`. The surface-pairing slice adds
`surface_pair_cli_prints_code_once_and_lists_pending`, `surface_revoke_marks_the_token_revoked`,
`surface_command_requires_a_node_and_session`, `surface_ls_never_prints_a_token`,
`surface_commands_are_home_only`, `surface_commands_are_registered_and_node_is_a_filter`,
`agent_pair_writes_0600_token_and_prints_no_secret`, `agent_pair_refuses_a_replayed_code` and
`agent_pair_surfaces_path_is_private`. The offload-policy slice adds
`job_add_refuses_node_placement_when_offload_is_disabled`,
`job_add_persists_any_placement_and_job_show_reads_it`,
`offload_policy_json_is_the_effective_policy` and
`offload_status_lists_nodes_assignments_and_waiting_reasons` in `tests/offload.rs`. The v0.4
CLI/gateway/agent end-to-end slice adds `agent_paths_add_reaches_the_home_without_a_reconnect`,
`node_enroll_replace_rotates_a_pending_code_and_node_rm_yes_removes_it` and
`opencode_stdout_readiness_reaches_running_and_a_failed_start_has_a_redacted_last_error` in
`tests/agent.rs`; the path-map and stdout-readiness tests use a real gateway and a fake `ssh`
script, the stdout-readiness test also starts the real agent daemon against a fake `opencode`
binary, and the node-enrollment test runs in-process through `vowp_cli::run`.

The ASIF slice adds, in `tests/asif.rs`: `session_export_asif_uses_default_asif_zip_name`,
`session_export_asif_stdout_is_binary_and_refuses_json`,
`session_export_asif_refuses_binary_stdout_on_a_tty`,
`session_export_asif_rejects_tail_and_bundle_only_flags`,
`session_export_asif_human_and_json_shapes`, `session_export_asif_then_import_round_trip`,
`session_export_asif_redact_file_is_read_locally_and_never_echoed`,
`session_export_asif_keep_host_paths_is_opt_in`,
`session_export_asif_reports_losses_and_writes_report_only_when_asked`,
`session_export_asif_strict_exits_five_and_writes_nothing`,
`session_import_asif_creates_a_new_session_and_prints_its_id`,
`session_import_asif_json_shape_and_full_report`, `session_import_asif_requires_an_existing_cwd`,
`session_import_asif_stdin_stages_and_imports`,
`session_import_asif_invalid_package_exits_two_and_writes_nothing`,
`session_import_asif_same_package_twice_exits_four_and_keeps_the_first_session`,
`session_import_asif_multiple_branches_requires_branch`,
`session_import_asif_same_id_and_executor_are_refused` and
`session_import_asif_strict_and_allow_loss_map_to_the_documented_exit_codes`. The remote half,
in `tests/session.rs` with the fake-ssh helpers, adds
`session_export_asif_remote_stream_verifies_bytes_and_hash`,
`session_export_asif_remote_failure_writes_no_local_file`,
`session_export_asif_remote_report_is_refused_with_exit_two`,
`session_import_asif_remote_streams_and_relays_the_response`,
`session_import_asif_remote_failure_creates_no_local_session`,
`session_import_asif_remote_report_is_refused_with_exit_two` and
`session_import_asif_remote_directory_is_refused`.

## 12. Mac agent commands (v0.4)

These commands are the Mac side of the local orchestrator. They use `~/.vowp-agent` (override
with `VOWP_AGENT_HOME`), never open a project store or `vowp-core`, and never put the node private
key or the one-time code in argv, `ps`, a log line or a JSON document. `agent enroll --project`
and `agent paths add|rm|ls` manage the local project path map the offload executor resolves
`PathRef`s against. The hidden
`vowp agent gateway` command is the home-side exception: it runs on the home and opens the node
store. Crate APIs: [`crates/vowp-agent.md`](crates/vowp-agent.md) and
[`crates/vowp-core.md`](crates/vowp-core.md) §9.

### `vowp agent enroll [--target <ssh-target> | --wss <URL>] --code-file <path|-> [--project <PATH>]... [--worktree-root <PATH>] [--tls-fingerprint <sha256:hex>]`
Generates `~/.vowp-agent/node.key` (0600) and `node.pub` when absent, opens
`ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 <ssh-target> vowp agent gateway --stdio`,
sends `Enroll { code, public_key, …, paths }` and waits for `Enrolled`. Each repeatable
`--project <PATH>` maps one local checkout: the project name comes from `[project].name` in its
`vowp.toml` (else the directory name), and the worktree root defaults to
`<checkout>/.vowp/worktrees` unless `--worktree-root` is given. The roots must be absolute and
the local checkout must exist. Without `--project`, enrollment reuses the saved map. It then
reconnects and performs the `AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome`
handshake. The code is a 52-character one-time value; `--code-file -` reads it from stdin.
Success writes `agent.json` (0600), including the path map, and prints the node, home and protocol:

```sh
vowp agent enroll --target home.example --code-file -
vowp agent enroll --target home.example --code-file - --project ~/src/vowp
```

Human output is `enrolled node mac-studio in home home-1 (vowp-agent/1)`; `--json` prints
`{"node":"mac-studio","home":"home-1","protocol":"vowp-agent/1","heartbeat_secs":10}`. The
private key, public key and code are never printed.

`--wss <URL>` stores a WSS transport instead of the SSH target; `--target` becomes optional because
the proxy URL carries the route. The URL must be exactly `wss://host[:port]/agent/v1` with no
query, fragment, user-info or other path. The default `tls = "system"` uses the system trust store
and hostname verification and records the first observed leaf fingerprint in `agent.json`;
`--tls-fingerprint sha256:<64 lowercase hex>` selects `tls = "pin"` and requires `--wss`. A pin or
fingerprint mismatch is a guard (exit 5). Editing `agent.json` by hand is the fallback; no new
credential is introduced.

### `vowp agent paths add --project <PATH> [--name <NAME>] [--worktree-root <PATH>] [--allow <REL>]...`
Adds or updates one project without re-enrolling. The project name defaults to
`[project].name` from the checkout's `vowp.toml`, then the directory name; `--name` overrides it.
The worktree root defaults to `<checkout>/.vowp/worktrees` and is created when missing.
`--allow` is a repeatable normalised relative path below the checkout; with no entry the whole
checkout is allowed. The command validates the roots, writes `agent.json` 0600, and prints the
owner-only mapping `vowp /Users/me/src/vowp worktrees /Users/me/src/vowp/.vowp/worktrees`;
`--json` prints the `AgentPathMap`. A running `vowp agent run` reloads the file on its next
heartbeat and sends `PathMapUpdate` on the live connection. The home persists the row before it
answers `Pong`, so `vowp node show <node> --json` lists the project in `mapped` without a
re-enrollment or a reconnect; a bad map gets an `Error` frame and the connection stays open.
Without enrollment it exits 3 with the enrollment hint.

### `vowp agent paths rm <PROJECT>`
Removes one project from the local map and writes `agent.json` 0600. An unknown project is
exit 3. Human output is `unmapped project <name>`; `--json` prints
`{"project":"vowp","removed":true}`.

### `vowp agent paths ls [--json]`
Lists the local map. Human output is one line per project with its absolute local root, worktree
root and allow list; `--json` prints the `AgentPathMap` array. These roots are owner-only: this
command and `agent status` may print them, but no home snapshot, log or `status.json` cache does.

### `vowp agent pair --code-file <path|-> --surface <id> --session <id> [--app <app>]`

Loads `agent.json` and `node.key`, performs the normal agent handshake and sends
`SurfacePair { code, surface_id, session, app }` over the existing transport. The home returns
`SurfaceIssued { surface_id, token, scopes, expires_at }`; the agent writes the token to
`~/.vowp-agent/surfaces/<surface_id>/token` (0600, with `surfaces/` and the surface directory
0700) and the expiry to `surfaces/<surface_id>/expires_at` (0600). The token is never printed,
logged or passed in argv. `--code-file -` reads the 52-character code from stdin. `--app`
defaults to `agent`; the home accepts that only when `surface pair` did not bind an app. Human
output is `paired surface <id>; token stored at <path>; expires <rfc3339>`; `--json` prints the
`PairedSurface` fields (`surface_id`, `scopes`, `expires_at`, `token_path`) and no token. A bad,
expired or replayed code is a non-retryable refusal (exit 5); a missing enrollment is exit 3.

### `vowp agent run [--foreground] [--role <role>] [--service]`
Loads `agent.json` and `node.key`, connects, and sends a heartbeat every 10 seconds (the interval
`HomeChallenge` names); each heartbeat rewrites `status.json` (0600). `Ping` is answered with
`Pong`. `Revoked` stops with exit 5 and `Superseded` stops with exit 4. A dropped connection,
EOF, a post-handshake `Error` frame or retryable home error reconnects with exponential backoff
(1, 2, 4 … 60 seconds, with jitter; a 60-second stable connection resets it); a `Refused` error
naming a surface token is the exception, marking that surface `needs_pair` while the connection
stays up. An unexpected known frame after the handshake is logged and ignored by the gateway rather
than ending the session.
`--role` is `surface`, `executor` or `both` (default `both`). `--foreground` is accepted and is the
default. `--service` is the service-manager
mode: it checks the state directory and root rule, writes the 0600 `run` marker, installs the
SIGTERM/SIGINT drain flag and exits 0 after a clean drain. It is supported on Linux and macOS only
(exit 5 elsewhere); `agent service install` puts it in the unit.

### `vowp agent status [--json]`
Reads `status.json` and prints the last connection state, node, home, protocol, last heartbeat,
last error, pending spool bytes, key fingerprint and surface-token expiry when one is paired.
When projects are mapped, the human output adds a `paths:` block with each project name, local
root and worktree root; `--json` adds a `paths` array of `AgentPathMap`. These absolute roots are
owner-only output built from `agent.json`: `status.json` stays a names-and-counts cache and never
contains them. Without an enrollment it exits 3 with the `vowp agent enroll` hint. It also exits
0 when the last state is offline or stopped. The local surface state adds a `surfaces:` block and
a `surfaces` array; each entry is `{"surface","prefix","expires_at","state"}` where `state` is
`paired`, `needs_pair` or `revoked`. The prefix is the first 12 scheme-stripped token characters;
the token and its hash are never printed or written to `status.json`. A token expiring within
seven days prints one warning to stderr. `--json` prints:

```json
{
  "connection": "connected",
  "node": "mac-studio",
  "home": "home-1",
  "protocol": "vowp-agent/1",
  "last_heartbeat": "2026-11-14T10:00:00.000Z",
  "last_error": null,
  "pending_spool_bytes": 42,
  "key_fingerprint": "sha256:0123456789abcdef",
  "surface_token_expires_at": null,
  "surfaces": [
    {
      "surface": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
      "prefix": "AbCdEfGhIjKl",
      "expires_at": "2026-12-16T10:00:00.000Z",
      "state": "paired"
    }
  ],
  "paths": [
    {
      "project": "vowp",
      "local_root": "/Users/me/src/vowp",
      "worktree_root": "/Users/me/src/vowp/.vowp/worktrees",
      "allow": []
    }
  ]
}
```

`connection` is one of `unknown`, `connecting`, `connected`, `reconnecting`, `revoked`,
`superseded` or `stopped`. `AgentStatus` is a CLI-local document, not an exported protocol
schema.

### `vowp agent service install [--print] [--force] [--now]` · `uninstall [--yes]` · `status`

`install` writes exactly one launchd `LaunchAgent` (`~/Library/LaunchAgents/dev.vowp.agent.plist`)
or one systemd user unit (`$XDG_CONFIG_HOME/systemd/user/vowp-agent.service`, else
`~/.config/systemd/user/vowp-agent.service`), 0600 and owned by the installing user. The unit text
is `agent run --foreground --service` with `VOWP_AGENT_HOME` and the installing shell's `PATH`; it
contains no token, code, key, provider value or absolute project path. `--print` writes nothing,
invokes no manager and prints the unit and the commands; `--now` also runs the current user's
manager commands. A foreign file at the unit path is exit 4 unless `--force`; an unsupported
platform is exit 5 and a missing manager is exit 7. `install --json` prints
`{"installed","unit","run_marker","linger"}` (the linger hint is omitted on macOS).

`uninstall --yes` stops the unit and removes only a file carrying
`generated by vowp agent service install`; it keeps `~/.vowp-agent` and every token. Without
`--yes` it refuses. `--json` prints `{"removed","unit"}`.

`status` prints the unit path, whether the manager has it loaded, the reported pid and the last
exit status. `--json` prints `{"available","unit","state","loaded","pid","last_exit"}`, where
`state` is `loaded`, `not_loaded` or `unavailable`; a missing manager is `available: false`, not
an error. The manager is read with `launchctl print gui/<uid>/dev.vowp.agent` or `systemctl --user
show vowp-agent.service --property=LoadState,ActiveState,MainPID,ExecMainStatus`.

### `vowp agent doctor [--json]`

Reports local warnings without failing on them: `state_dir`, `service`, `surfaces` and `logins`.
It checks the state-directory owner and mode, the unit and run marker, `needs_pair`/expired/
soon-expiring surface tokens and copied login material. Login material is reported by name only
(`.ssh`, `.claude`, `.env`, `.git-credentials`, `auth.json`, and the two opencode `auth.json`
locations) and file contents are never read. It exits 0 when no check failed and 1 on a failure;
`--json` prints `{"ok","checks":[{"name","status","detail"}]}`. The detail never contains a
token, a key, a path map or a secret value.

### `vowp agent gateway --stdio` (hidden, home)
The home side of the SSH stdio transport. It requires `--stdio`, discovers the node with
`node_ctx::NodeCtx::discover` and calls `vowp_core::agent::gateway::serve`. A control machine is
refused with exit 5 ("the gateway needs the home store; run it on the home machine") and an
uninitialised home with exit 3 ("vowp home is not initialised; run `vowp init`"). An explicit
`--project` or a cwd inside a registered checkout builds the project-scoped `Ctx` and opens
`Ctx::store()` with that project's deps. Everywhere else — notably `$HOME` with zero, one or
several projects — it ensures the reserved `_node` project row and database, opens the home's
global store with `Store::open_global`, and uses the node-level `node_ctx::home_deps`. The gateway
then resolves each session's `sessions.project` binding (`_node` for a project-less session) and
each offload assignment's `agent_assignments.project` binding to that project's store, so run
ingest, result return and bundle settlement use the owning project's database.
Stdout carries only newline-delimited `AgentFrame` JSON; errors go to stderr. The first frame is
`Enroll` or `AgentHello`; enrollment returns `Enrolled` and closes, and the agent reconnects for
`AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome`. Then `Heartbeat` updates presence and
capacity, `Ping` is answered with `Pong`, and a revoked or unknown node gets `Revoked` or a typed
`Error` before the connection closes. Frames are capped at 1 MiB and the handshake times out after
10 seconds. It never logs the code, the signature, the public-key value or a path map.

`vowp agent gateway --listen <addr>` is the hidden WSS listener. It requires a loopback socket
address and ensures the reserved `_node` project before accepting connections. It honors
`[ingress.wss]` for the path, connection cap, frame cap and handshake timeout; the path must be
`/agent/v1`. Every accepted WebSocket opens one `gateway::serve` on its own thread, with the home's
global store and node-level deps. `vowp up` starts this child when `[ingress.wss].enabled = true`
and stops it on shutdown; a missing or unstartable listener warns and leaves the supervisor
running. The listener is plain `ws://` on loopback; TLS and the public port belong to the owner's
reverse proxy. Stdout prints one `gateway: listening on ws://<addr>/agent/v1` line; frames are
never printed.

## 13. Home surface pairing (v0.4 Phase 1)

These commands create and manage the home half of a paired local surface. They are home-only:
a control machine exits 5, an uninitialised home exits 3, and no command opens a project
database. They use the global store (`surfaces`, `surface_tokens`, `enrollment_codes`,
`sessions`, `nodes`). The pairing code and the surface token are secrets: the code is printed
once by `surface pair`, the token is written only by `agent pair` to its 0600 agent file, and
neither appears in another command's output, a log or a JSON document.

### `vowp surface pair --session <id> --node <name> --kind opencode|chat [--app <app>] [--replace <surface-id>]`

Creates a `pending` surface bound to the session and a ten-minute one-time code, then prints the
code once and the exact Mac command. `--node` is the node the surface runs on; it is not an SSH
redirect for this command. `--kind` accepts `opencode` or `chat`; `--app` is an optional
printable name of at most 64 characters. Human output is:

```text
created pending surface 01ARZ3NDEKTSV4RRFFQ69G5FAW on mac-studio (opencode)
session: 01ARZ3NDEKTSV4RRFFQ69G5FAV
code: <52-character code>
expires: 2026-11-14T10:10:00.000Z
run on the Mac:
  vowp agent pair --code-file - --surface 01ARZ3NDEKTSV4RRFFQ69G5FAW --session 01ARZ3NDEKTSV4RRFFQ69G5FAV --app opencode
```

`--json` prints:

```json
{
  "surface": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
  "node": "mac-studio",
  "session": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "kind": "opencode",
  "app": "opencode",
  "code": "…",
  "expires_at": "2026-11-14T10:10:00.000Z",
  "command": "vowp agent pair --code-file - --surface … --session … --app opencode"
}
```

A missing `--node`/`--session`, an invalid id or an unknown kind is exit 2. An unknown node or
session is exit 3. A revoked node is exit 5. A second pending surface for the same node, session,
kind and app is a conflict (exit 4); revoke it before pairing again. The plaintext code is never
written to the home database, a file or the log; only its SHA-256 hash is stored.

`--replace <surface-id>` revokes the old surface and creates the replacement pending surface in
one transaction. The old surface must belong to the same node and session and must not already be
revoked; its queued rotation frame is removed and its token is refused immediately. Human output
starts with `revoked surface <old> and created its replacement`; `--json` adds
`replaced: "<old>"` and otherwise has the fields above. No token is issued until the agent redeems
the new code.

### `vowp surface ls [--node <name>] [--json]`

Lists the surfaces this CLI created. Each row re-reads the surface's current node, kind and
state from the store and the scopes, prefix and expiry of its token rows from `surface_tokens`:

```text
ID                        NODE        KIND      STATE     SCOPES                          PREFIX         EXPIRES          OVERLAP
01ARZ…FAW                 mac-studio  opencode  pending   -                               -              -                -
```

`--node <name>` filters by the surface's node. `--json` prints an array of
`{"id","node","kind","state","scopes","prefix","expires_at","overlap_until","expired","pending","queued_at","delivery_deadline"}`.
`queued_at` and `delivery_deadline` are omitted when absent; `prefix`, `expires_at` and
`overlap_until` are `null` when there is no value. A pending surface has no token, so its scopes are
`[]`, its `prefix` and `expires_at` are `null`, and `queued_at`/`delivery_deadline` are absent. A
revoked surface stays listed with state `revoked`. A token whose expiry has passed shows
`expired` in the `EXPIRES` cell while the surface's state stays `attached`. `PREFIX` is the newest
active token's 12-character display prefix; `OVERLAP` is `-`, an old token's overlap deadline, or
`PENDING <deadline>` for an undelivered rotation. No token or hash is ever printed. The id index
is `<home>/surfaces.json` (0600) because the v4 store read model returns one surface by id; the
ids are not secrets and the state/scopes/expiry always come from the store.

### `vowp surface revoke <id> [--yes]`

Revokes every active token of the surface and marks the surface `revoked` through
`vowp_core::agent::pair::revoke_surface`. The next frame carrying that token is refused. Without
`--yes` it refuses with exit 5; an unknown surface is exit 3 and an already-revoked surface
succeeds with zero tokens revoked. Human output is
`revoked surface <id>; N token(s) revoked`; `--json` prints
`{"surface":"…","status":"revoked","tokens_revoked":N}`. No token is printed.

### `vowp surface rotate <id> [--wait <duration>] [--grace <duration>]`

Mints a fresh 30-day token with the surface's current scopes and shortens every other active
token to `now + grace`; the default grace is ten minutes and `--grace` is capped at 24 hours. A
token that already expires sooner keeps its earlier expiry. The replacement is queued as one
`SurfaceIssued` envelope in `<home>/agent-frames/<node>/<surface>.json` (0600 in a 0700 directory)
and drained by the gateway on the node's next authenticated connection. A second rotation before
delivery replaces the file. Human output is `rotated surface <id> on <node>`, `prefix`, `expires`,
`old token expires` and, while undelivered, `delivery: pending`; `--json` prints
`{"surface","node","prefix","expires_at","old_expires_at","pending","queued_at"}`. The token is
never printed. `--node <name>` names the surface's node, not an SSH target; a different node is
exit 4.

`--wait <duration>` polls the outbox until the frame is drained; if it is still queued, the command
exits 7 with `rotation_pending`. Without `--wait` the shipped behavior stays: exit 0, the frame
stays queued and an offline node writes
`warning: node <n> is offline; the agent must re-pair before <old expiry>` to stderr. `--grace` is
refused for a pending or revoked surface; a pending surface is exit 4, an unknown one exit 3 and a
revoked one exit 5.

The reverse approval flow (the Mac asks and the home opens an approval inbox item) remains a
later v0.4 slice. The `vowp mcp` proxy is implemented: `vowp mcp --agent-socket <PATH> --surface
<ID>` selects `crates/vowp-cli/src/mcp/proxy.rs`, and the 23 passing tests in
`crates/vowp-cli/tests/mcp.rs` are recorded in the roadmap's merged Phase 3 table. Leases and
the Phase 1 local chat are implemented.

## 14. Offload policy and status (v0.4 Phase 2)

The read-only home slice for §7 of `v0.4/local-orchestrator.md`. It decides and explains; it does
not send `AssignRun`, create a worktree or start an agent.

### Policy file and placement grammar

`[offload]` is part of `vowp.toml` and is documented with its defaults in
[`types.md`](types.md) §11. The defaults keep offload disabled: `enabled = false`,
`default = "home"`, `max_remote_runs = 2`, `grace_secs = 600`, `result_transport = "push"`,
`pause_on_home_brake = false`, `require_clean_base = true`, `min_disk_bytes = 5368709120`.
`[offload.projects.<name>]` carries `placement`, `home_only`, `allow_kinds`, `deny_kinds` and
`allow_secrets`; `[offload.nodes.<name>]` carries `max_workers`, `allow_battery` and the required
`labels`. Unknown keys are rejected. `result_transport = "bundle"` is legal in Phase 4; `push`
stays the default, and an assignment's value wins for the whole run with no fallback in either
direction. A bundle assignment carries an empty `remote_url` and needs no push access.

A job's `--placement` is one of:

| Value | Meaning |
|---|---|
| `home` | the job never leaves the home (the default when no placement row exists) |
| `node:<name>` | only that node; if it is offline or ineligible the job waits, with no silent fallback |
| `node:<name>+home` | the named node, with the home as the explicit fallback |
| `any` | the home and every eligible node |

A per-job placement wins over the project default, but it cannot override `home_only`, a denied
kind or a home-only secret. `node:<name>` is refused at `job add` time when offload is disabled.

### `vowp offload policy [--project <p>] [--json]`

Prints the effective policy with every default filled in: enabled, default placement,
`max_remote_runs`, grace, transport, brake and clean-base flags, the disk reserve, one row per
`[offload.projects.*]` entry and one row per configured node (plus the registered node cards).
The global `--project <name|path>` selects the `vowp.toml`; an explicit selection restricts the
project rows to that project. Human output is a scalar block (`offload`, `default`,
`max_remote_runs`, `grace_secs`, `result_transport`, `pause_on_home_brake`,
`require_clean_base`, `min_disk_bytes`) followed by two tables (`PROJECT PLACEMENT HOME_ONLY
ALLOW_KINDS DENY_KINDS ALLOW_SECRETS` and `NODE MAX_WORKERS ALLOW_BATTERY LABELS`). `--json`
prints one object with `enabled`, `default`, `max_remote_runs`, `grace_secs`,
`result_transport`, `pause_on_home_brake`, `require_clean_base`, `min_disk_bytes`, and the
`projects` and `nodes` arrays. No path is printed.

### `vowp offload status [--json]`

Reads the global store and each registered project read-only. Human output is three tables:

```text
nodes:       NAME STATUS PRESENCE MAPPED SLOTS DISK BATTERY LAST_ERROR
assignments: ID PROJECT RUN NODE STATE CREATED DETAIL
ready:       PROJECT JOB KIND PLACEMENT REASON DETAIL
```

`assignments` lists only live states (`queued`, `sent`, `accepted`, `running`, `disconnected`) and
never echoes the assignment payload or prompt. `ready` lists ready jobs whose effective placement
is not `home` (the per-job `job_placement` row, else the project default, else `offload.default`),
evaluated with the pure policy: `REASON` is the §7.5 `WaitingReason` or `eligible`,
and `DETAIL` is the human explanation. The evaluator uses the facts the home stores; a fact a
node reports only at dispatch time (origin match, base fetchability, clean base) is `None` and is
reported as a conservative refusal, never as a pass.

Neither command prints an absolute local path. The JSON document is
`{"generated_at","nodes":[…],"assignments":[…],"ready":[…]}`; each node has
`{"name","status","presence","mapped","slots_busy","slots_max","disk_free_bytes","on_battery",
"thermal_state","last_error"}`, each assignment has the token-free row fields, and each ready row
has `{"project","job","kind","placement","waiting_reason","detail","targets"}`.

Named tests (`tests/offload.rs`): `job_add_refuses_node_placement_when_offload_is_disabled`,
`job_add_persists_any_placement_and_job_show_reads_it`,
`offload_policy_json_is_the_effective_policy` and
`offload_status_lists_nodes_assignments_and_waiting_reasons`.
