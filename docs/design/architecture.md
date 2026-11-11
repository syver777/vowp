# architecture.md — components, data flow, on-disk layout, `vowp.toml`, errors, security

Status: current for v0.2. The v0.1 component model below is still the base; the v0.2 home, fleet,
capacity, surfaces and control machine are summarized here and specified in
[`v0.2/home-and-surfaces.md`](v0.2/home-and-surfaces.md). Product rationale: `plan.md`.
Deviations: `decisions.md`. Types: `types.md`. Tables: `schema/global.sql` and `schema/project.sql`.

## 1. Components and data flow

```
                       control machine (laptop / desktop / phone)
                         vowp control init · vowp fleet status · vowp chat
                         opencode desktop / Claude apps over ssh -L
                                      │  ssh (BatchMode, key)
                                      ▼
 vowp (one static binary, crate vowp-cli)          home (the authority)
   ├─ CLI commands ──────────────────────────────► vowp-core (library)  ──► vowp-store  (global + project SQLite, session logs, blobs)
   │                                                 ├ session             ├► vowp-graph  (pure: DAG, state machine, dispatch)
   │  `vowp up` = one node supervisor               ├ loops L1–L9         ├► vowp-tmux   (window manager, private-socket tested)
   │  `vowp chat` / `vowp mcp` = surfaces           ├ gates               ├► vowp-adapters (claude-code | opencode | cli)
   │  `vowp session tail` = read-only follower      ├ ledger              └► vowp-protocol (types, traits, config, Clock)
   │                                                ├ schedules
   │                                                ├ capacity (sampler, admission, brakes, advisor)
   │                                                ├ surface (leases, takeover, app hosts, mirror)
   │                                                ├ fleet (snapshot)
   │                                                ├ lifecycle (classes, gc, backup)
   │                                                ├ vault (secrets, injection, redaction)
   │                                                └ promote
   ▼
 harness child processes (claude -p … / opencode run … / generic cli) — one process per run, headless JSON on stdout
```

The home runs **one** supervisor for every active project. Each tick samples capacity and sweeps
stale leases, applies node requests, then runs every active project's L1–L9 steps in fairness
order, then writes `snapshot.json`, the board and the heartbeat. A project step that fails three
ticks in a row pauses only that project. The supervisor holds a global `Store` and one handle per
active project, never two project DBs on one connection. A control machine holds no truth: reads and
writes go to the home over SSH; `vowp up`/`down`/`board` refuse there.

Data flow for one job (all names are the spec's):

1. `vowp job add` (or the planner via the same CLI) → `Store::insert_job` → row in `jobs`, `edges`; brief written to `.vowp/jobs/<id>/brief.md`.
2. **L1 tick** (`vowp-core::loops`) loads jobs+edges into a `vowp_graph::Graph`; `ready_jobs` + `plan_dispatch` choose jobs; for each choice the supervisor
   creates the git worktree `job/<id>`, a session (`SessionOps::create_session`), a tmux window `job-<id>` running `vowp __window job <id>` (read-only: resolves the job's latest session and follows its log, `vowp session tail --pretty --follow`, so it keeps working across switches)
   (read-only view), and a **runner thread** calling `SessionOps::run_turn`.
3. `run_turn` picks the executor from the job's profile ladder (`Profile::pick` with the `limits` table), spawns the adapter (`Adapter::start`), and for every
   `HarnessEvent` calls `SessionLog::append` (JSONL authoritative) and updates `runs` / `ledger` rows at the end. The harness never sees tmux.
4. Run ends → L1 harvests: outcome classified (`ok|limit|credits|auth|context|rate|crash|stalled|…`), `limits` updated, then the job state machine
   (`vowp_graph::transition`, table in `loops.md` §2) moves the job: report present → `gating` → gates (`gates` module) → `reviewing` (verify job, L3) → `merging`
   (merge queue) → `done`.
5. Failures follow the loops: L2 continue (≤2) → keeper (L5); limit → L6 failover with capsule; question/credits/auth → inbox (L7) and the job `parked`.
6. L9 schedules and L8 watchdog keep the node running unattended.

Nothing in this flow is stored only in memory: killing every process and running `vowp up` again reconciles from the DB (`loops.md` §1 step 1) and continues.

### 1.1 Crates and who may depend on whom

| Crate | Responsibility | Depends on |
|---|---|---|
| `vowp-protocol` | all shared types, `Adapter`/`AdapterRun`/`SessionOps` traits, `Clock`, `Log`, `VowpConfig`, v0.2 capacity/surface/snapshot/catalog/vault types, JSON Schema export | — |
| `vowp-store` | split schema + migrations, row CRUD, `SessionLog`, blobs, reindex, cross-DB reconcile, backup | protocol |
| `vowp-graph` | `Graph`, state machine, ownership overlap, dispatch planning, render/export | protocol |
| `vowp-tmux` | `Tmux` + `WindowManager` trait + `FakeWindows` | — |
| `vowp-adapters` | `ClaudeCodeAdapter`, `OpencodeAdapter`, `CliAdapter`, `classify`, `proc` | protocol |
| `vowp-core` | `session`, `loops`, `gates`, `ledger`, `schedules`, `promote`, `capacity`, `surface`, `fleet`, `lifecycle`, `vault` | all above |
| `vowp-cli` | binary `vowp`: clap commands, `--node` over ssh, home/control routing, `chat`, `mcp`, `setup` | core + all |

## 2. Process and threading model

- **Plain threads, blocking I/O.** No async runtime (decision D9). A home runs at most one **node
  supervisor process** (`vowp up`, `systemd --user` unit) for all active projects; a lock file
  `~/.vowp/supervisor.lock` (`create_new` + pid, stale check by `kill -0`) enforces it. Every other
  `vowp …` invocation is a short-lived process that talks to the global DB and, when it acts on a
  project, one project DB (WAL, `busy_timeout=5000`).
- Supervisor threads:
  - **main/node thread**: owns the global `Store` and one handle per active project; runs the node
    pass (capacity sample, stale-lease sweep, lifecycle tick), applies node requests, then every
    active project's L1–L9 steps in fairness order, then writes the snapshot and heartbeat. It wakes
    sooner on a runner `Wake` (`mpsc`).
  - **runner threads**, one per active run (workers, verifiers, planner, keeper, capsule writer,
    probes, app hosts): each opens **its own `Store` connections** (rusqlite `Connection` is not
    `Sync`) and its own `SessionLog` writer for its session (one writer per session, guaranteed by the
    session lockfile). Each spawns two reader threads inside the adapter (stdout lines → parser,
    stderr → ring buffer). The capacity sampler is one node thread; a run's cgroup or `/proc` scope is
    sampled by the thread that owns the run.
  - **gate/merge threads**: command gates and the merge queue run as threads executing child processes with timeouts; results are posted back through the DB (`gate_results`, job state) and a `Wake`.
  - **lease/ingest threads**: `vowp chat`, `vowp mcp`, `vowp session ingest` and the app hosts run in
    the client process; they write only through the home's session service and ingest. The lease
    sweep and takeover push run on the node thread.
  - Shutdown: `SIGTERM`/`SIGINT` are caught in `vowp-cli` with the `signal-hook` crate (`signal_hook::flag::register`, safe API, no `unsafe` in our code) which sets the supervisor's `CancelToken`; `vowp down` instead writes the `~/.vowp/stop` file, which the node thread polls each tick (and the loop also wakes on it). Cancelled → runner threads `interrupt()` then `stop()` their adapter runs → loop exits. On crash the next start reconciles.
- Async is **not** justified in the core: every stream is a child-process pipe handled by two
  threads. The `vowp mcp` stdio server, the one-time secret paste page and the tunnel supervisor use
  plain threads and `std::net`; the planned v0.3 HTTP+SSE daemon (axum/tokio) will wrap `vowp-core`
  behind its own runtime without changing these crates.
- The home is the only writer of session logs and databases. Remote execution streams events to
  `vowp session ingest`; the executor owns its harness binary and injected values, the home receives
  names and events, never a secret value.
- All time goes through `Clock`; all randomness (backoff jitter) through an injected `Rng` closure in loops so tests are deterministic.

## 3. On-disk layout of a home

`VOWP_HOME` defaults to `~/.vowp` (override with env `VOWP_HOME` or `[node] home`; **tests always pass an explicit temp dir**).

```
~/.vowp/
├─ vowp.db                     global SQLite (WAL: vowp.db-wal, vowp.db-shm) — schema/global.sql, user_version 2
├─ projects/<name>/project.db  one SQLite per project — schema/project.sql, user_version 1
├─ config.toml                 optional node-level defaults (type `NodeFile`, types.md §11: [node] [profiles] [providers] [harnesses] [capsule] only; no [project])
├─ supervisor.lock             pid of the running supervisor
├─ heartbeat                   unix seconds (ASCII) rewritten every tick via tmp+rename (watchdog reads it; loops.md §10)
├─ snapshot.json               versioned fleet snapshot (`vowp-snapshot/1`, types.md §16.3) written every tick
├─ board.json                  v0.1 compatibility copy: one active non-reserved project, or (with no active project) one non-broken paused project
├─ stop                        presence = "shut down" request
├─ migration.json              present only while a v0.1 → v0.2 migration is in flight
├─ sessions/<sessionId>/       one dir per universal session (portable: copy it to another node, `vowp reindex`)
│    ├─ log.jsonl              append-only EventEnvelope lines (authoritative)
│    ├─ blobs/<sha256>         spilled tool output / attachments (> 8192 bytes)
│    ├─ capsules/<at_seq:010>-<ulid>.json   (at_seq zero-padded to 10 digits)
│    ├─ adapters.json          {"natives":[{"harness","native_session_id","node","first_seq","created_at"}]} (append-only, rebuildable)
│    └─ .lock                  single-writer lock (pid)
├─ orchestrators/<name>/       project-local app config, lease token (0600), host state and mirror cursor
├─ secrets/<NAME>              basic-mode secret files (dir 0700, file 0600); never in a backup
├─ runs/<runId>.jsonl          raw harness stdout lines (display/debug; capped at 50 MB, compressed after 1 day, rotated after 14)
├─ runs/<runId>.stderr         last 64 KB of stderr
├─ logs/supervisor.log         structured loop log (see §6)
├─ backups/                    `vowp backup` units (global + every project DB + capsules + catalog); 7 daily + 4 weekly
├─ archive/projects/<name>-<ts>/   `vowp project rm` moves the project directory here
└─ inbox/<inboxId>.md          human-readable copy of every inbox item (also in the DB)
```

A control machine uses `~/.vowp-control/` instead of a home:

```
~/.vowp-control/
├─ control.json                role, home name, ssh target, remote home (0600)
├─ nodes.json                  node-registry mirror
├─ snapshot.json               last snapshot from the home
├─ snapshot.meta.json          {"fetched_at":"…","source":"remote|local|cache"}
├─ spool/<session>/<run>/      unacked remote-exec events and acks
├─ surfaces.json               local surface state
└─ tunnels.json                managed forwards and pids (0600)
```

Project directories (inside each project checkout, committed unless noted):

```
<repo>/
├─ vowp.toml                   project config (§5)
├─ AGENTS.md  CLAUDE.md  .claude/skills/…      installed by `vowp init` from the embedded framework templates
├─ opencode.jsonc                              agent definitions with permission presets
└─ .vowp/
     ├─ jobs/<jobId>/brief.md  report.md       job contract and result (committed on the job branch)
     ├─ graph.yaml                             optional `vowp graph --export .vowp/graph.yaml` snapshot
     ├─ checkpoints/<ts>.md                    planner summaries (every 2 h)
     └─ releases.log                           one line per promotion/rollback (promote module)
```
Job worktrees live **outside** the repo: `<worktree_root>/<jobId>` (default `<repo>/../<repo-name>-wt/<jobId>`), branch `job/<jobId>`, removed after merge.
`~/.vowp` is node state, never committed. `.gitignore` lines added by `vowp init`: `.vowp/tmp/`.

## 4. Nodes, `--node` and the control machine

`vowp node add <name> <ssh-target>` inserts a `nodes` row. Any command accepts `--node <name>`: the CLI re-executes itself on that node as
`ssh -o BatchMode=yes <target> vowp <same args minus --node>`-style passthrough (stdout/stderr/exit code are forwarded unchanged; `-t` is added only for
`attach`, `session chat`, `chat` and a refreshing `board`). `vowp node add <name> -` registers a **surface-only executor node**: a laptop that
dials the home, which never dials it. The remote needs `vowp` on its `PATH`.

A **control machine** is not a node with a smaller home. It has `~/.vowp-control/control.json`
(0600) and routes every project/fleet command to the configured home; `vowp up`, `down` and `board`
refuse with exit 5 because the control machine has no local truth. A path accepted by
`vowp project add` is a path on the **home**. `vowp control init <ssh-target>` records the route;
`vowp fleet status` caches the home's snapshot and prints `as of HH:MM` when SSH is down;
`vowp tunnel` manages `ssh -L` forwards from the machine that uses them. Full rules:
`v0.2/home-and-surfaces.md` §8.

## 5. `vowp.toml`

### 5.1 Schema (all tables optional unless marked; unknown keys are errors: `deny_unknown_fields`)

| Table | Key | Type | Default | Meaning |
|---|---|---|---|---|
| `[node]` | `name` | string | `"local"` | name of this node (matches `nodes.name`) |
| | `max_workers` / `max_verifiers` | int | 4 / 2 | per-project slot preference in v0.2; the measured capacity manager and the owner target decide admission |
| | `tick_secs` | int | 60 | L1 tick |
| | `cwd_allow` | [string] | `[]` (project path, worktree root and the VOWP orchestrator root are always allowed) | extra allowed working directories |
| | `home` | string | `~/.vowp` | VOWP_HOME |
| | `tz_offset_minutes` | int | 0 | used to read clock-time resets in limit messages (e.g. `resets 11pm`) |
| **`[project]`** | `name` | string, **required** | — | project name (slug) |
| | `path` | string | dir containing vowp.toml | main checkout |
| | `remote` / `default_branch` | string | `origin` / `main` | merge target |
| | `check_cmds` | [string] | `[]` | repo-wide checks run **only by the merge queue** on the rebased tree (never as per-job gates; per-job `cmd` gates are listed explicitly in the job's `gates`, e.g. `cargo test -p vowp-graph`) |
| | `lock_files` | [string] | `["Cargo.lock"]` | files regenerated, not merged, on rebase conflicts |
| | `always_allowed` | [string] | `["Cargo.lock"]` | paths any job may touch (owns_respected) |
| | `worktree_root` | string | see §3 | |
| | `push` | bool | true | merge queue pushes `main` |
| `[profiles.<name>]` | `ladder` | [executor string], **required** | — | ordered rungs `harness:model[@effort][#variant]` |
| | `on_quota` | `next_then_probe_back`\|`next`\|`park` | `next_then_probe_back` | |
| | `node` | string | project home | run this profile's sessions on that node |
| `[providers.<p>]` | `max_concurrent` | int | 4 | concurrent runs for provider `p` (lowered after 429s) |
| | `on_credits_exhausted` | `"park"` | `park` | the only v0.1 option |
| `[providers.<p>.prices."<model>"]` | `input_per_m`, `output_per_m`, `cache_read_per_m`, `cache_write_per_m` | float USD per 1M tokens | — | price table for `cost.source = "price-table"` |
| `[harnesses.<name>]` | `binary`, `extra_args`, `env_pass`, `permission_mode`, `mapping` | | PATH lookup | per-harness overrides; `mapping` = generic cli adapter file |
| `[env.<name>]` | `checkout`, `deploy`, `guards`, `approval`, `serving_bank`, `probe`, `rollback` | | | Work→Production (`promote` module) |
| `[gates]` | `<set name> = [Gate…]` | | | named gate sets referenced by `vowp job add --gates <set>` |
| `[loops.<name>]` | `interval_secs`, `state`, plus `LoopBounds` keys | | built-in defaults | overrides for loops, e.g. `[loops.l1-supervisor] stall_minutes = 20` |
| `[[schedules]]` | `name`, `cron` \| `every`, `tz`, `missed`, `action` | | | L9 (see `loops.md` §11) |
| `[capsule]` | `executor`, `min_interval_secs`, `max_input_chars`, `tail_turns` | | cheapest capable = last rung of `keeper` | |
| `[merge]` | `rebase`, `check_cmds`, `timeout_secs`, `secret_scan` | | true / project.check_cmds / 2700 / true | |

### 5.2 Annotated example

This block is the canonical example. It is copied byte-for-byte to `crates/vowp-protocol/tests/fixtures/vowp.example.toml` and parsed in a test.

```toml
# vowp.toml — project configuration (committed). Node-wide defaults may live in ~/.vowp/config.toml.

[node]
name = "vps"                 # this node's name
max_workers = 4              # concurrent worker slots (each is a tmux window)
max_verifiers = 2
tick_secs = 60               # L1 supervisor tick
tz_offset_minutes = 120      # to read "resets 11pm" style limit messages (Europe/Oslo summer: 120)

[project]
name = "vowp"
remote = "origin"
default_branch = "main"
check_cmds = ["cargo fmt --all --check", "cargo clippy --workspace --all-targets -- -D warnings", "cargo test --workspace"]
lock_files = ["Cargo.lock"]
always_allowed = ["Cargo.lock"]

# ---- profiles: a role's model ladder; rung 0 is preferred ---------------------------------------------
[profiles.planner]
ladder = ["claude-code:model-a@medium", "opencode:provider/model-b#max"]
on_quota = "next_then_probe_back"      # on a Claude limit: capsule -> DeepSeek; probe Claude after the reset

[profiles.keeper]
ladder = ["opencode:provider/model-b#max"]

[profiles.worker]
ladder = ["opencode:provider/model-b#max"]

[profiles.verifier]
ladder = ["opencode:provider/model-b#max"]

# ---- providers ---------------------------------------------------------------------------------------
[providers.deepseek]
max_concurrent = 6
on_credits_exhausted = "park"          # never silently switch to another paid provider

[providers.deepseek.prices."deepseek-flash"]   # only used when a harness reports no cost receipt
input_per_m = 0.15
output_per_m = 0.60
cache_read_per_m = 0.003

[providers.claude]
max_concurrent = 1                     # the planner shares the owner's subscription

# ---- harness overrides ------------------------------------------------------------------------------------
[harnesses.claude-code]
permission_mode = "auto"
env_pass = []                          # NAMES of env vars to forward (never values)

[harnesses.opencode]
extra_args = []                        # extra flags appended after the adapter's own argv (the adapter already passes --auto, do not repeat it)

[harnesses.codex]                      # a generic cli adapter entry, driven by a mapping file
mapping = ".vowp/adapters/codex.toml"

# ---- named gate sets (used as defaults or via `vowp job add --gates rust_build`) -------------------------
[gates]
rust_build = [
  { report = { fields = ["status", "branch", "commits", "tests"] } },
  { cmd = { run = "cargo test --workspace", expect_exit = 0, timeout_secs = 1800 } },
  { owns_respected = true },
  { secret_scan = true },
  { review = { by = "verifier" } },
]

# ---- environments (Work -> Production) --------------------------------------------------------------------
[env.work]
checkout = "~/myapp"
deploy = ["./scripts/deploy-work.sh"]

[env.prod]
checkout = "~/myapp-prod"              # fast-forward-only clone of main
deploy = ["vercel deploy --prod"]
guards = ["clean_tree", "head_equals_remote", "build", "no_active_jobs"]
approval = "human"                     # human | policy
serving_bank = "./scripts/bank-serving.sh {target}"   # snapshot what is live BEFORE deploying
probe = "./scripts/probe-prod.sh"

# ---- loop overrides ---------------------------------------------------------------------------------------
[loops.l1-supervisor]
stall_minutes = 20
silent_minutes = 8
same_action_max = 3

[loops.l4-planner]
interval_secs = 1800                   # planner heartbeat (30 min)

# ---- schedules (L9) ------------------------------------------------------------------------------------------
[[schedules]]
name = "morning-report"
cron = "0 7 * * 1-5"                   # 5-field cron, evaluated in `tz` (UTC or a fixed offset in v0.1)
tz = "+02:00"
missed = "run_once"                    # run_once | skip
action = { kind = "loop", loop_name = "l4-planner" }

[[schedules]]
name = "nightly-backup"
cron = "0 1 * * *"
action = { kind = "command", argv = ["vowp", "backup"], timeout_secs = 600 }

# ---- capsule writer and merge queue --------------------------------------------------------------------------------
[capsule]
executor = "opencode:provider/model-b#max"
min_interval_secs = 120
tail_turns = 2

[merge]
rebase = true
timeout_secs = 2700
secret_scan = true
```

### 5.3 Validation rules (`VowpConfig::validate`)

1. `profiles.*.ladder` non-empty, no duplicate rung, every executor parses; the four role names `planner keeper worker verifier` must exist **after merging** project over node config (otherwise `Config` error naming the missing profile).
2. Each rung's provider (`Executor::provider()`) with `[providers.<p>]` present must have `max_concurrent ≥ 1`.
3. `[[schedules]]`: exactly one of `cron` / `every`; `cron` has exactly **5** whitespace-separated fields (structural check in `VowpConfig::validate`, no `cron` crate in vowp-protocol; the full parse is `vowp_core::schedules::ScheduleSpec::parse`, run by `vowp doctor`, `vowp schedule add` and the supervisor at startup — a string that passes the structural check but fails the parse is a `Config` error there); `every` matches `^\d+[smhd]$` and is ≥ 60 s; `tz` is `UTC` or `±HH:MM`; names unique.
4. `[env.*]`: `checkout` non-empty; `approval = "policy"` requires `guards` non-empty.
5. Gate sets: every `Gate::validate()` passes; `deploy`/`script` gates must run a command (never a file-exists check): a `cmd`/`script`/`deploy` `run` that is only `test -f …`/`[ -f … ]`/`ls …` is rejected with `anti-pattern: gate must execute and check results`.
6. `loops.<name>`: key must be a known loop name (`l1-supervisor … l9-schedules`).
7. `owns`-style globs elsewhere (`always_allowed`) compile with `globset`.
8. Unknown keys anywhere → error with the key path (serde `deny_unknown_fields`).
9. `[node]`: `max_workers ≥ 1`, `max_verifiers ≥ 1`, `tick_secs ≥ 1` (error path `node.max_workers` etc.).

Resolution order for any setting: CLI flag > `vowp.toml` > `~/.vowp/config.toml` > built-in default.

v0.2 adds no required `vowp.toml` keys. `[node] max_workers`/`max_verifiers` remain per-project
preferences; the capacity manager's measured target, the owner target and the brakes decide what is
admitted. Schedules and loops are project rows; the node-level lifecycle schedule is the per-project
`gc-nightly` command schedule.

## 6. Error handling and logging conventions

- **Typed errors per crate** (`thiserror`), each with `kind() -> ErrorKind`. The CLI maps `ErrorKind` to exit codes (`cli.md` §1.1): `InvalidInput`→2, `NotFound`→3, `Conflict`→4, `Guard|Refused`→5, `Limit|Auth`→6, `Remote`→7, everything else →1.
- A failing **tick step** never aborts the tick: each loop step runs inside `catch_unwind` + `Result`; the error is logged, `loops.fail_streak += 1`, a `ledger` row `state`/`note` is written, and after 5 consecutive failures an inbox `notice` is opened. In v0.2 a project that fails three ticks in a row is paused alone (`broken: <error>`) with a node-inbox notice; other projects keep running.
- **No silent fallback**: an unknown limit text is `Unknown error` handled by D15 (3 consecutive → unavailable 15 min), never "assume it's fine".
- **Log format** (`~/.vowp/logs/supervisor.log`, one line each): `<ts> <LEVEL> <target> <message>`; targets `l1 l2 l3 l4 l5 l6 l7 l8 l9 merge gates session adapter tmux cli capacity surface fleet lifecycle vault mcp setup`. Levels `ERROR WARN INFO DEBUG` (`VOWP_LOG=debug`). Rotation: the CLI renames the file to `.1` over 10 MB, keeping 3.
- **Never logged**: prompts, env values, tool outputs, anything matching a secret pattern. Run/limit `detail` strings are redacted by `vowp_adapters::classify::redact` before storage.
- **Machine-readable output**: every CLI command supports `--json`; errors in `--json` mode are `{"error":{"kind":"not_found","message":"job n99 not found","hint":"vowp job ls"}}` on stdout with the matching exit code.

## 7. Security model (design summary §13, made concrete)

| Rule | Enforcement |
|---|---|
| Home authority | only the home writes its databases and session logs. A control machine, app or MCP server sends `vowp` commands or streams ingest events; it never opens SQLite over the network. |
| Loopback only | the supervisor and fleet commands open no public socket. The opencode app server binds `127.0.0.1` and is joined through an SSH tunnel and a one-time pair link; the secret paste page binds `127.0.0.1` for 10 minutes or one use. |
| One writer per session | a lease row exists at most once per session; every write path checks the token and records the actor. A moved surface keeps read access (follow mode) and loses writes. Ingest validates the token before accepting an event. |
| No credential values in files | `env_pass` and setup answers hold **names**; `HarnessConfig` has no value fields. Basic mode stores `~/.vowp/secrets/<NAME>` (0600); vault mode stores under `/home/vowp-vault/secrets/<NAME>` through the helper. No command takes a value as an argument; the paste page never returns it. |
| Exact-value redaction | VOWP knows the values it injected and replaces exact occurrences with `[REDACTED:<NAME>]` in events, capsules, report artifacts, raw run mirrors and the snapshot. Gate evidence and ledger details are not redacted in this build (documented follow-up); the secret-scan gate remains a separate backstop. Backups never include secrets. |
| Stock binaries only | adapters spawn `claude` / `opencode` / a configured cli exactly as a user would; no token reading (`~/.claude` is never opened except the optional read-only transcript importer for `vowp session watch`, which reads session JSONL only), no spoofing, no login flow in VOWP. |
| cwd allow-list | `Adapter::start` callers (`session::run_turn`) check `cwd` is inside the job worktree, the project path, `worktree_root`, `[node] cwd_allow` or the VOWP-owned orchestrator root `<VOWP_HOME>/orchestrators/<name>/`; anything else, including paths elsewhere under `VOWP_HOME`, is `Refused`. |
| Worktree isolation | one worktree per job; ownership globs enforced by the `owns_respected` gate and by dispatch-time locks (`vowp-graph`). |
| tmux safety | tagged windows only, `vowp-` sessions only, no respawn/send-keys API exists (`vowp-tmux.md`). |
| Guarded promotion | `promote` runs guards → serving bank → diff → approval (human default) → deploy → probe; refusals are staged (`ReleaseState::Refused`), never retried with a workaround. |
| Nothing public | no command publishes, deploys or flips visibility without an `approval` inbox item answered by a human. |
| Secret scan | gate `secret_scan` (rules by name only) on every job diff and again on the merge queue's rebased tree before the push. |
| Provider spend | a rung not listed in the profile is never used; `credits` ⇒ park + inbox, never switch (`on_credits_exhausted = "park"`). Admission also respects the provider `max_concurrent` and 429 backoff. |
| Honest limit | any agent that can run arbitrary code in a process holding a secret can print it. The design keeps values out of files an agent can read, transcripts and git; it cannot make a value unreadable to code that is given it. |

## 8. Compatibility and versioning

- DB: the global DB uses `PRAGMA user_version = 2` (`schema/global.sql`); each project DB uses
  `user_version = 1` (`schema/project.sql`). A v0.1 single DB is migration 1 and is split by
  `vowp migrate` (also run by `vowp up` with a backup). A newer DB than the binary → refuse to open
  (`StoreError::SchemaTooNew`). `vowp migrate --rollback <backup>` restores the pre-migration file.
- Store API: the public `Store` API stays source-compatible; the split is inside `vowp-store`.
  A handle is bound to the global DB plus exactly one project DB (reserved project `_node` for
  project-less sessions and node history). Never attach two project DBs to one connection.
- Event log: `EventEnvelope` is frozen for v0.1; v0.2 adds `SessionRole::Orchestrator`,
  `SwitchReason::Takeover` and remote-exec framing without changing the envelope. Additions are new
  optional fields or new event types (readers ignore unknown *fields*; unknown *types* are a hard
  error in v0.1 and tolerated as opaque in v0.2).
- Capsule: `version` field; v0.1 and v0.2 = 1.
- Bundles: `format: "vowp-bundle-v1"` (`sessions.md` §6). Importing a v0.1 bundle into v0.2 keeps
  working; the child-session rule is unchanged.
- Snapshot: `format: "vowp-snapshot/1"` (`vowp_protocol::snapshot::SNAPSHOT_FORMAT`), exported by
  `vowp schema snapshot`. Additions are optional fields; a reader rejects an unknown major format.
- Protocol: `vowp_protocol::PROTOCOL_VERSION = 1` is bumped only on an incompatible wire change.

## 9. Named tests

The full per-feature list is in `v0.2/home-and-surfaces.md` §15; the architecture-level proofs are
`crates/vowp-store/tests/{split,crash_split,migration}.rs` (split, ordered writes, reconcile,
migration row counts), `crates/vowp-core/tests/{node_supervisor,fleet_snapshot,capacity_measure,capacity_admit,capacity_advisor,surface_lease,remote_exec,vault,lifecycle,app_hosts,mirror}.rs`,
`crates/vowp-cli/tests/{capacity,control,maint,core,status,up,setup,setup_verify,mcp,orchestrator,chat,session}.rs`.
All use temporary homes, private tmux sockets and fake harnesses/SSH; none touches the network or the
real `~/.vowp`.
