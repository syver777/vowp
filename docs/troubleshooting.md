# Troubleshooting

Start with the evidence the supervisor already records: `vowp status`, `vowp doctor`,
`vowp job show`, the session log and `~/.vowp/logs/supervisor.log`. Most "stuck" jobs are waiting on a
dependency, a slot, an ownership lock, a provider limit or a human answer, and the state plus the note
says which.

> **Status: current.** The v0.1 CLI is complete: `vowp init`, `vowp doctor`, the `vowp job` commands,
> `vowp graph`, the supervisor, session, inbox, schedule, cost, backup and reindex commands are all
> implemented.

## First: exit codes

| Code | Meaning | Typical cause |
|---|---|---|
| 0 | Success | — |
| 1 | Internal / unexpected | I/O, database, parse, config, adapter failure, timeout. |
| 2 | Invalid input / usage | Bad id, bad cron, ambiguous prefix, clap error. |
| 3 | Not found | Job, session, node or release does not exist. |
| 4 | Conflict | Job exists, state does not allow the action, supervisor already running. |
| 5 | Guard or refusal | Promotion guard failed; cwd outside the allow-list. |
| 6 | Provider limited / auth needed | The action needs a provider that is limited. |
| 7 | Remote failure | SSH could not connect, or remote `vowp` is missing. |

With `--json`, an error is printed on stdout as
`{"error":{"kind":"not_found","message":"job n99 not found","hint":"vowp job ls"}}` and the exit code is
the same.

## Install and build

**`vowp: unknown command` after a build.** Check the spelling against `vowp --help`. Before `vowp init`,
commands that need the project home exit 3 with "vowp home is not initialised"; run `vowp init` first.
The v0.1 CLI has no command stubs left.

**The wrong binary runs.** Two installs on `PATH`:

```sh
command -v vowp
vowp --version
```

Remove the old copy or put the intended `~/.cargo/bin` first. `cargo run -p vowp-cli --bin vowp -- --version`
runs the workspace binary directly.

**The build fails on the toolchain.** The workspace pins stable Rust edition 2024 in
`rust-toolchain.toml`. Check `rustup show`; if `rustup` is absent, install the pinned channel from the
file. Do not switch the workspace to nightly.

**`cargo install` succeeds but `vowp` is not found.** `cargo install` writes to `~/.cargo/bin` by
default. Add that directory to `PATH`, or install with `--root <dir>` and add `<dir>/bin`.

## `vowp init`

| Symptom | Cause and fix |
|---|---|
| Exit 4, "already initialised" | A `vowp.toml` already exists. `init` writes nothing in this case; use `--force` only when you intend to replace generated files. |
| Files listed as skipped | On a first init, `AGENTS.md` or `opencode.jsonc` already existed; VOWP keeps them without `--force`. `CLAUDE.md` is special: VOWP appends `@AGENTS.md` when it is missing, even without `--force`. |
| `no project: run vowp init` | You are outside a project and more than one project is registered, or none is. `cd` into the repository or pass `--project <name\|path>`. |
| Wrong project selected | Pass `--project`; the default is the nearest ancestor containing `vowp.toml`, else the only registered project. |

## `vowp doctor`

`doctor` exits 0 only when no check is a `fail`. A `warn` is informational.

| Check | Remedy |
|---|---|
| `home` | Make `~/.vowp` writable, or pass the same `--home` to every command. |
| `db` | If it reports a schema version newer than the binary, upgrade the binary; do not delete the database. If it reports corruption, stop the supervisor, restore a backup, and use `vowp reindex` only for session indexes. |
| `git` | Install git and ensure it is on `PATH` for non-interactive shells. |
| `tmux` (warn) | Install tmux before running jobs. `vowp up --no-tmux` skips only the initial supervisor layout; a dispatched job still needs a window. |
| `config` | The error names the key path. Unknown keys are errors (`deny_unknown_fields`); remove or correct the key. |
| `schedules` | A `[[schedules]]` entry did not parse; the check names the schedule and the error. |
| `harness:claude-code` / `harness:opencode` | Install the harness or fix its binary path in `[harnesses.<name>].binary`. A missing harness is a `fail` because a profile that cannot resolve would fail later. |
| `providers` | The check prints each variable **name** with `present` or `absent`; it never prints the value and never calls the network. Set it in the shell that starts the supervisor. |
| `systemd` / `watchdog` (warn) | Run [`scripts/install.sh`](../scripts/README.md) to install the user unit and cron check; a source build does not install them. |
| `supervisor` (warn) | Not running. Start it with `vowp up` if you expect one. |
| `disk` | Free at least 1 GB; runs and blobs grow with tool output. |

## Supervisor

**`vowp up` exits 4.** A supervisor is already running for this project home. `vowp status` shows the
pid. Do not start a second one; use `vowp down` or `vowp status --check` to decide whether it is alive.

**No heartbeat / `status --check` exits 1.** The heartbeat is older than 180 seconds. Check
`~/.vowp/logs/supervisor.log` and the process. Restart with `vowp down --force` followed by `vowp up`.
A crash is safe: the next start reconciles orphaned runs as `crash` and continues them.

**`vowp down` hangs.** It waits up to 30 seconds for the lock. If the process is unresponsive, use
`vowp down --force`; if the pid is gone, the stale lock is detected on the next `up`. Do not delete
`vowp.db` or the lock by hand unless you have stopped every VOWP process and have a backup.

**The supervisor restarts repeatedly.** Read the last `ERROR` lines in the log. A failing tick step is
caught and counted; five consecutive failures open an inbox notice. Common causes are a missing harness
binary, a corrupt config, a full disk or a repository that cannot fetch.

**`vowp up --once` finishes immediately.** It runs a single tick for tests and cron; use it to reproduce
a loop problem without leaving a supervisor running.

## Jobs

Use `vowp job show <id> --runs --gates` first. The job's state, `parked_reason`, `note`, last run
outcome and gate evidence usually identify the blocker.

| State / symptom | Likely cause | What to do |
|---|---|---|
| `pending` forever | A `needs` job is not `done` (or was cancelled). | `vowp graph` and `vowp job show <id>` show the unmet needs. Cancelled dependencies leave dependents `pending` and flagged `blocked-by-cancelled`; replan or cancel them. |
| `ready` but not running | No free worker slot, an overlapping ownership lock, or every rung of the profile is limited. | `vowp status` shows slots and limits; `vowp job ls` shows other running jobs. Do not raise `max_workers` past what the machine and provider can take. |
| `running` with no new events | The worker may be thinking, or the run may be hung. | After 20 minutes with no event and no commit L1 stalls it; after 8 minutes with zero output it restarts it. Check `vowp session tail <id> --follow`. |
| `running` but the report exists | The run is still attached; VOWP never gates a live run. | Wait for the run to end, or let the stall rules stop it. |
| Report not detected | Missing file, wrong worktree, invalid YAML, wrong `job`/`branch`, a missing commit sha, or `status: partial`/`blocked`. | Read the `report` gate evidence and fix the report. A stale report older than the attempt is ignored; a fix must rewrite it. |
| `gating` forever | A gate command is still running or timed out. | Check the gate timeout (default 1800 s) and the evidence tail. A timed-out gate kills the process group. |
| `fixing` for a long time | A fix or merge child job is still running, or it failed and the parent is waiting. | `vowp job ls --all` and `vowp job show <child>`. A failed child fails the parent and wakes the keeper. |
| `reviewing` with a failed verifier | The verifier could not produce a usable verdict twice. | The job is parked with a question; answer it or retry. Make findings concrete so the next verifier can judge. |
| `merging` stuck | The merge queue is running the repository checks or waiting for the lockfile refresh. | Check the merge budget (2700 s) and `vowp status`. One merge runs at a time per project. |
| `failed` | Retries exhausted or the keeper failed it. | Read the gate evidence and the last run. `vowp job retry <id>` or `vowp job replan <id>`. |
| `parked` | A question, credits, auth, provider block, exhaustion or a manual park. | Read `parked_reason` and `vowp inbox ls`; answer the item or `vowp job retry <id>` for a manual park. |
| `cancelled` | A human cancelled it. | The branch and worktree are kept for inspection. Dependents stay `pending`; replan or cancel them. |

### Changing state safely

- `vowp job retry <id>` only works from `failed` (→ `ready`), `parked` (→ `ready`) or `stalled`
  (→ `running`). From any other state it exits 4. This is deliberate: a running job is owned by its
  runner.
- `vowp job park <id> --reason "…"` stops the run and parks it; `retry` resumes.
- `vowp job cancel <id>` is terminal and keeps the worktree. Do not cancel a job merely because it is
  slow; use park or let the keeper diagnose.
- Keeper commands (`nudge`, `restart`, `replan`, `fail`) enqueue a request row. The `retry`, `park` and
  `cancel` mutations apply directly when no supervisor is running; keeper requests wait for the next
  tick, so start the supervisor before expecting a nudge to take effect.

## Gates

| Gate | Failure and fix |
|---|---|
| `report` | Missing/invalid front-matter, wrong job or branch, missing commit, or `status` not `done`. Fix the report; do not weaken the gate. |
| `cmd` | Exit code differs from `expect_exit`, the timeout fired, or `expect_pattern` did not match. Run the exact command in the job worktree and read its output. |
| `owns_respected` | The diff or uncommitted files touch paths outside `owns`. Move the change, split the job, or add the path to `always_allowed` only if it is genuinely shared. |
| `pushed` | The remote branch is missing or its sha differs from the local branch. The branch may not have been published; the merge queue normally handles this. |
| `review` | No verifier has passed for this attempt. Look at the verify job and its findings. |
| `secret_scan` | A rule matched an added line or a secret-looking path. Remove the value, use a placeholder, and refer to the secret by name. The evidence stores the rule name, not the value. |
| `deploy` | The deploy command failed or the probe did not match. Nothing beyond the guard is retried automatically; a refused promotion is staged for a human. |

If a gate is wrong, fix the gate definition in the job or project config, not the evidence. A gate that
only checks a file exists is rejected by validation for a reason.

## Providers and limits

| Limit | Meaning | Action |
|---|---|---|
| `quota` | A subscription window is exhausted. | VOWP records the reset (+2 min), switches to the next rung with a capsule and probes back. If no rung remains, the job parks with `ProviderBlocked`; answer after the reset. |
| `credits` | The provider account is out of credit. | Top up, then `vowp inbox answer <id> resolved`. VOWP never switches to another paid provider automatically. |
| `auth` | Login expired. | Log in with the harness's own tooling, then answer the inbox item `resolved`. |
| `rate` | 429/overloaded. | Backoff with jitter and a lowered concurrency cap; the continue does not consume a continue. Wait for the reset. |
| `context` | Context window overflow. | A capsule and a fresh native session on the same harness; no action unless it repeats. |
| `unavailable` | Three consecutive crash/error runs with no limit signal. | The provider is marked unavailable for 15 minutes, then probed. Check the harness binary and network. |

Check `vowp status` for active limits and reset times, and `vowp inbox ls` for the action a human must
take. An inbox answer unparks only the jobs waiting on that item.

## Sessions

**`session switch` exits 6.** The target provider is limited. Pick another rung, or wait for the reset.

**`session switch` exits 4.** The session is already switching, or a turn is running. An active turn
without `--force` is a conflict; stop it or pass `--force` explicitly. The drain phase is used by
automatic switches (failover and context pressure) and by a forced manual switch.

**The target probe fails.** The session stays on the source and is still readable. Check that the target
harness is installed and the executor string parses. v0.1 probes only the local adapter registry; for a
remote target, run `vowp --node <n> doctor` yourself before switching.

**The capsule is invalid twice.** The previous capsule is kept and an `error` event is appended. A switch
without any previous capsule aborts. Check the capsule executor's output; do not hand-edit a capsule
into the log.

**Export/import is rejected.** `unknown bundle format` means the file is not `vowp-bundle-v1`;
`bundle too new` means its protocol version is newer than the binary; `checksum mismatch` means it was
edited or truncated. Nothing is created on rejection. Re-export from the source.

**`attach` falls back to the session log.** tmux is absent or the window is gone.
`vowp attach <job-id>` tails the job's latest session log. To follow it yourself, find the session
id with `vowp session ls --job <job-id>` and run `vowp session tail <session-id> --pretty --follow`.

**`session watch` exits 1.** The transcript directory is unreadable. Pass `--claude-dir <dir>` and check
permissions. The importer never writes to Claude's files.

**`session watch` exits 2.** It was run in a terminal without `--once`. Continuous following is not
available in v0.1; pass `--once` (or run it from cron).

## Schedules

- A bad cron or an interval under 60 seconds exits 2. v0.1 accepts exactly five whitespace-separated
  cron fields; six-field cron with seconds is rejected.
- `tz` is `UTC` or a fixed offset such as `+02:00`; IANA names are v0.2.
- A schedule that was missed while the node was down fires once with `run_once`, or records a skip with
  `skip`. It never replays every missed occurrence.
- A paused schedule does not fire; `vowp schedule resume <name>` enables it. `vowp loops` shows the L9
  loop itself.
- A command action that fails records `failed: exit N` and opens at most one inbox notice per schedule
  per day.

## Costs

`vowp cost` shows `receipt`, `price-table`, `unknown` or `mixed` per row. An unknown cost is
`?` and is counted separately; it is never assumed free. If every row is unknown, the harness is not reporting
usage and no `[providers.<p>.prices]` entry exists for the model. Add a price table only for models you
know; otherwise leave it unknown.

## Database, backup and reindex

- **Schema too new.** The database was written by a newer binary. Upgrade the binary; if you must go
  back, restore a pre-upgrade backup. VOWP refuses to guess at a newer schema.
- **Corruption.** Stop the supervisor, copy `vowp.db` aside, and restore a backup. `vowp reindex` only
  rebuilds session indexes from `log.jsonl`; it does not repair jobs or decisions.
- **Missing session rows.** `vowp reindex` re-registers session directories. A session directory copied
  from another node opens after a reindex; its node becomes the local node name.
- **Backups.** `vowp backup` is an online SQLite backup and is safe while the supervisor runs. It does
  not copy session directories; back up `~/.vowp` separately if you need the logs.

## tmux

| Symptom | Fix |
|---|---|
| `tmux: command not found` | Install tmux. `vowp up --no-tmux` skips the initial layout but does not let jobs run without windows. |
| No `vowp-<project>` session | `vowp up` creates it; check the log for a tmux error. |
| A job window is gone | The window is a view; the session log is intact. `vowp attach <job-id>` tails the job's latest session log. |
| Windows from an old project remain | `vowp down --kill-windows` closes only tagged windows for this project. VOWP never kills untagged windows. |

## Remote nodes

| Symptom | Fix |
|---|---|
| Exit 7 | Test `ssh -o BatchMode=yes <target> vowp --version` from the same user and shell. |
| `vowp: command not found` remotely | Fix the remote non-interactive `PATH`, or install to a standard location. |
| Version mismatch | Upgrade both binaries; a newer database is refused rather than guessed at. |
| Host key prompt | Add the host key to `known_hosts`; batch mode must not prompt. |
| An argument was mangled | VOWP single-quote escapes the whole remote command; check for a shell alias or wrapper on the remote that rewrites arguments. |

## What to collect for a report

When you need help, collect:

```sh
vowp status --json
vowp inbox ls
vowp doctor --json
vowp job show <id> --runs --gates
vowp graph
tail -n 200 ~/.vowp/logs/supervisor.log
```

Also include the binary version (`vowp --version`), the operating system, and the exact command and exit
code. **Redact secrets and sensitive prompts**: replace values with `<redacted>`, refer to variables by
name, and do not attach a full `~/.vowp` directory to a public issue. If the problem is a vulnerability,
use the private advisory link in the [security model](security.md), not a public issue.

## When to stop instead of working around

Do not:

- delete `vowp.db`, the session logs or the lock file to "reset" a stuck supervisor;
- widen `owns` or add paths to `always_allowed` to make `owns_respected` pass;
- disable `secret_scan` or `merge.check_cmds` to get a merge through;
- retry a refused promotion with `--yes` when the environment requires human approval;
- run a second supervisor for the same project home.

Those actions destroy the evidence that the next tick, the verifier or the owner needs. Park the job,
write the question in the report or inbox, and keep unrelated work moving.
