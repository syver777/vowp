# VOWP — Virtual Open Work-to-Production

**VOWP is an open-source (MIT) supervisor for AI coding agents.** It runs the harnesses you already
use as a small engineering team on a home machine you own: one supervisor for every project, an
append-only session log, measured capacity and a single merge queue.

## Set up VOWP 0.2.0

Paste this prompt into Claude Code, opencode or another harness on the computer you want to use as
the control machine:

```text
Set up VOWP 0.2.0 for me. Read
https://raw.githubusercontent.com/syver777/vowp/v0.2.0/docs/setup/agent-setup.md
and follow it exactly. Ask me only what it tells you to ask, show me the full plan once,
and run it after I approve. Never ask me to paste a secret into this chat.
```

The harness reads the [setup contract](docs/setup/agent-setup.md), installs the pinned CLI locally if
it is missing, asks five questions in one message, shows the whole plan without changing the home,
and runs it after one approval. The local per-user CLI install is the only change before that
approval, and the harness tells you about it in the question message. It stops for the human
hand-offs — a printed sudo command (only if one is needed), a harness sign-in, a secret entered
through the hidden prompt or a one-time link — and then runs the verification. The setup commands do
the real work, so the result does not depend on which harness runs the prompt.

What you need before you paste it:

- **A home:** one Linux VPS on `x86_64` or `arm64`, reachable with key-only SSH. The setup can create
  a non-root `vowp` user when passwordless sudo is available; otherwise it runs as the SSH user.
- **A harness:** Claude Code or opencode installed and signed in **on the home**, with its own login
  flow. VOWP drives the documented headless interface and never reads the login.
- **A control machine:** this computer, Linux or macOS with `ssh`. It holds only derived state: a
  snapshot cache, an event spool and its tunnel records. A phone can join later with an SSH app and
  the owner's key.
- **A repository, optionally:** an existing checkout on the home with a `vowp.toml`, or no project
  yet. Setup registers repositories; it does not clone or rewrite them.

> **Status: current — VOWP 0.2.** The v0.2 command surface, including the setup commands, is
> implemented. The release process owns the 0.2.0 tag and published binaries. The
> [roadmap](docs/roadmap.md) lists what is deliberately deferred.

## How it fits together

```text
 control machine(s)                              home (the authority)
 laptop / desktop / phone                        one Linux VPS
 ┌──────────────────────────────┐                ┌──────────────────────────────────────────────┐
 │ opencode desktop / Claude    │                │ one vowp supervisor                          │
 │ vowp chat / vowp mcp         │   ssh (key)    │  global DB   ~/.vowp/vowp.db                 │
 │ snapshot cache, SSH tunnels  │ ─────────────► │  project DBs ~/.vowp/projects/<p>/project.db │
 └──────────────────────────────┘                │  sessions/<id>/log.jsonl                     │
                                                 │  snapshot.json every tick                    │
                                                 │  tmux views, capacity sampler                │
                                                 │  workers / verifiers → stock harnesses       │
                                                 └──────────────────────────────────────────────┘
```

The **home** is the only writer. It owns the global database, one database per project, every session
log, the supervisor and the snapshot. A **control machine** sends commands to the home over SSH and
keeps only derived state. A **surface** is where a person is talking to a session now — an app, the
terminal chat fallback, or the MCP server the app calls. Every session has one append-only log and at
most one surface with the lease to write to it; every other surface follows read-only.

The home opens no public port. The opencode app server and the one-time secret page listen on
loopback and are reached through an SSH tunnel. A phone needs an SSH app and the owner's key, nothing
else. See [Home and surfaces](docs/concepts/home-and-surfaces.md) for the full model.

## What the supervisor does

- **Runs one fleet, not one project.** One supervisor ticks every active project in fairness order.
  A project can be paused, resumed, archived or removed; a broken project parks alone while the rest
  keep running. Project weights decide the tick order.
- **Turns a goal into a job graph.** Jobs carry a brief, owned paths, dependencies and gates. L1
  dispatches ready work into a git worktree and a tmux window; L2 continues a run; L3 verifies it
  independently; the single merge queue rebases, re-runs the repository checks, scans the diff and
  fast-forwards `main`.
- **Keeps the conversation in VOWP.** Sessions are harness-neutral, append-only event logs. A session
  can move between models, harnesses and surfaces with a capsule; a takeover cuts the old surface off
  within the lease rules and leaves it in follow mode.
- **Measures capacity instead of guessing.** The sampler reads the host, every run records its own
  usage, and admission checks the brake, provider limit, target, RAM, CPU and disk before a run
  starts. A job that cannot start stays ready with the reason on the board.
- **Keeps secrets by name.** Values are entered through a hidden prompt or a one-time loopback page,
  never in an argument, a plan or a chat. Injection is by name, and exact-value redaction covers the
  session and snapshot paths described below.
- **Classifies every file.** Production, truth, open work and secrets are never auto-deleted; closed
  sessions, run output, worktrees, samples and caches have bounded retention and a dry-run `gc`.
- **Sets itself up with a plan.** The setup plan prints every phase, command and file. The home and
  control commands apply it idempotently, stop at a human hand-off with exit 5, and the verify step
  proves a fixture job merges, the snapshot is readable from the control machine and an orchestrator
  answers through the chat fallback.

## Fleet and capacity

```text
 measure ──────────────► admit ──────────────► brake
 each tick              before each run        stops new runs
 host + per-run         target, RAM, CPU,      memory pressure,
 usage + estimates      disk, provider         swap growth, disk,
                                               provider limits
        │
        └──────────────► advise ──────────────► owner decides
                         every 30 min and       capacity set --target N
                         on events              (the AI only proposes)
```

Capacity is measured from the machine, admitted deterministically and advised by a model that never
changes the target itself. The default mode is `auto`, derived from the latest host sample and the p90
estimates. The owner can set a fixed target; a target above the advice needs an explicit override and
is shown as an owner override, and the brakes still apply. A brake only stops new admissions — it
never kills a running job. A job that waits says why (`waiting: RAM 1.2 GB short`), and the same
sentence appears in the queue and the snapshot.

On a host without `/proc` (macOS or a minimal container), the sampler says `fallback`: auto proposes
1, RAM and CPU checks are skipped, and the disk check still runs. There is no cost cap in v0.2; cost
is reported separately as receipts, labelled estimates or `unknown`. See
[Capacity](docs/concepts/capacity.md).

## Data lifecycle

Every file VOWP creates has a class, and the class decides whether it may ever be deleted:

| Class | Examples | Default |
|---|---|---|
| **Production** | releases, rollback snapshots, serving banks, prod pointers | **Never auto-deleted.** |
| **Truth** | global and project databases, capsules, passing gate evidence, decisions and ledger, backups, archived project data | **Never auto-deleted**; backed up. |
| **Open work** | unmerged or unpushed worktrees, active sessions, orchestrator sessions, the live lock and heartbeat | **Never auto-deleted.** |
| **Secrets** | the secret store and vault files | **Never auto-deleted**, never backed up in plain text. |
| **Closed sessions** | `log.jsonl` and blobs after the session closes | Kept 30 days, then compacted to a capsule + summary. |
| **Run output** | `runs/*.jsonl`, `*.stderr` | 14 days; compressed after 1 day. |
| **Worktrees** | a merged job's worktree; a failed, cancelled or replanned job's worktree | Removed after merge; kept 7 days after a failed terminal state. |
| **Samples** | host samples | 24 hours; no minute rollup in this build. |
| **Cache** | disposable `tmp/` files | 24 hours. |

`gc` is a dry run that lists files by class; `gc --apply` removes only the expired
disposable classes after compacting closed sessions. It never removes production, truth, open work or
secrets, never the main checkout, and never deletes a session row. A nightly schedule runs
`gc --apply`. Backups include the global database, every project database and the session content,
never secrets. See [Data lifecycle](docs/concepts/data-lifecycle.md).

## Secrets, honestly

VOWP refers to a secret by **name** only. `secret set NAME` reads the value from a hidden prompt;
`secret request NAME` opens a one-time page on `127.0.0.1` with a ten-minute expiry and returns only a
name, URL and expiry. The AI sees `NAME saved, ends in …last4`, never the value.

- **Vault-user mode** stores files under the separate `vowp-vault` user when sudo can create it.
- **Basic mode** stores them under `~/.vowp/secrets/` owned by the runtime user at mode 0600. Basic
  mode does not separate the value from the agent's user; the harness runs as that same user.
- Injection is by name through a harness's `env_pass` list. The promoter does not add a secret
  environment to deploy commands in this build.
- Exact-value redaction covers session-service and remote-ingest events, the snapshot, capsules and
  the finished report and run mirror. It does **not** cover the supervisor log, gate command evidence,
  ledger rows, the switch turn path or the `session watch` importer in this build, so a command that
  prints a secret can still persist it there.
- **The honest limit:** any agent that can run arbitrary code inside a process that holds a secret can
  print it. Vault mode keeps the files out of the runtime user's direct readable tree, but the helper
  that reads them is a broad trust boundary. Neither mode encrypts the store at rest. Harness logins
  stay under the harness's own control, and a secret pasted into a chat is already in the model
  provider's transcript — rotate it and use the safe path. Secrets are never included in a backup.

The [security model](docs/security.md) states the full coverage and the remaining gaps.

## Why VOWP

- **Stock harnesses, no forks.** Claude Code and opencode are driven through their documented
  headless modes. VOWP never scrapes a TUI, sends keystrokes to answer a dialog, or reads and forwards
  login tokens.
- **The database is the truth for decisions; files are the truth for content.** Jobs, gates, runs,
  costs and leases live in the global and project databases. Briefs, reports, session logs and blobs
  stay in plain files on the home. Kill everything, restart, and the supervisor reconciles.
- **Deterministic loops, LLM judgment.** A plain program ticks and never "decides to stop". Models are
  used for planning, diagnosis, review, handoff and capacity advice.
- **Any model, any machine, any surface.** A model is a rung in a profile ladder. The same session can
  run on Claude Code on a laptop or opencode on the home, and continue from another app.
- **Evidence over claims.** A job is done when its gates pass with recorded evidence — command, exit
  code, duration, output and verifier findings — not when an agent says so.
- **Guarded, reversible Work → Production.** Promotions snapshot what is live, ship only the intended
  change, record the rollback and require a human yes by default.
- **Humans are never a bottleneck for unrelated work.** A question parks one job and lands in an
  inbox; every other project keeps moving.

## Documentation

| Start here | What it covers |
|---|---|
| [Setup contract](docs/setup/agent-setup.md) | The exact prompt contract: ask once, show the plan once, run it, stop at hand-offs, verify, report. |
| [Concepts overview](docs/concepts/overview.md) | The vocabulary, the moving parts and where state lives. |
| [Home and surfaces](docs/concepts/home-and-surfaces.md) | One supervisor for many projects, leases, takeover, remote execution, app hosts, MCP and chat. |
| [Capacity](docs/concepts/capacity.md) | Measurement, admission, brakes, targets and the advisor. |
| [Data lifecycle](docs/concepts/data-lifecycle.md) | Classes, `gc`, compaction, backup and restore, migration. |
| [Graph and loops](docs/concepts/graph-and-loops.md) | Jobs, edges, states, gates, L1–L9 and the merge queue. |
| [Sessions and switching](docs/concepts/sessions-and-switching.md) | Event logs, capsules, executors, surfaces and handoff. |
| [Providers and failover](docs/concepts/providers-and-failover.md) | Profile ladders, shared provider limits and cost receipts. |
| [Work → Production](docs/concepts/work-to-production.md) | Environments, guards, approval, deploy, probe and rollback. |
| [Install](docs/guides/install.md) | The setup path, prerequisites, the manual binary install and the on-disk layout. |
| [Getting started](docs/guides/quickstart.md) | The first project, the first job, the fleet view and an orchestrator. |
| [Writing jobs](docs/guides/jobs.md) | Briefs, reports, gates, ownership and the verify/fix loop. |
| [Operating a home](docs/guides/operating.md) | `up`/`down`, fleet views, capacity, logs, backup, `gc` and migration. |
| [Sessions](docs/guides/sessions.md) | Chat, takeover, switch, capsule, export/import and `session watch`. |
| [Running a home on a VPS](docs/guides/vps.md) | The server account, harnesses, systemd, SSH and firewalls. |
| [Control machines and extra nodes](docs/guides/multi-node.md) | Routing, snapshot cache, tunnels, executor nodes and cross-node sessions. |
| [Schedules](docs/guides/schedules.md) | Cron and interval schedules, missed runs, pause and resume. |

Also: [security model](docs/security.md), [troubleshooting](docs/troubleshooting.md),
[contributing](CONTRIBUTING.md), the [v0.2 specification](docs/design/v0.2/home-and-surfaces.md),
the [CLI reference](docs/reference/cli.md) and the [roadmap](docs/roadmap.md).

## Limits of v0.2

- **One home.** There is no replication, leader election or multi-home scheduling.
- **No public API or page.** There is no HTTP+SSE daemon, web window or phone browser board. App hosts
  and the secret page are loopback-only and reached through SSH.
- **SSH is the transport.** A control machine needs key-based, batch-mode SSH to the home. A phone
  needs an SSH app and the key.
- **Apps keep their own limits.** VOWP can configure and host an app; it cannot close another vendor's
  window or force an app build to expose MCP tools.
- **One operator.** Multi-user accounts, quotas, roles and audit are not in v0.2.
- **Linux home, Linux/macOS control.** Windows is not supported, and there is no native VOWP desktop
  app; the apps are the surfaces.

## Repository layout

| Path | Contents |
|---|---|
| `crates/vowp-protocol` | Shared types, events, config, adapter/service traits, `Clock`, JSON Schema export. |
| `crates/vowp-store` | Global and per-project SQLite schemas, migrations, JSONL session logs, blobs, reindex, backup. |
| `crates/vowp-graph` | DAG, state machine, ownership overlap, dispatch planning, rendering. |
| `crates/vowp-tmux` | Safe tmux window manager and its in-memory fake. |
| `crates/vowp-adapters` | Claude Code, opencode and generic CLI adapters; usage scopes and classification. |
| `crates/vowp-core` | Sessions, loops, gates, ledger, schedules, capacity, surfaces, vault, lifecycle, promotion. |
| `crates/vowp-cli` | The `vowp` binary, including setup, control, fleet, capacity and MCP. |
| `framework/` | Templates, skills and agent presets installed by project initialisation. |
| `examples/` | Runnable examples, including the three-job `hello-vowp` graph. |
| `scripts/` | Per-user install, systemd unit, watchdog, uninstall and their tests. |
| `docs/` | User concepts and guides, the setup contract, reference and design specs. |

## License

MIT. See [`LICENSE`](LICENSE).
