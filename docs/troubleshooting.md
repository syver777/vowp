# Troubleshooting

Start with the evidence the home already records: `vowp status`, `vowp fleet status`, `vowp doctor`,
`vowp job show`, the session log and `~/.vowp/logs/supervisor.log`. Most "stuck" jobs are waiting on
a dependency, a capacity admission, an ownership lock, a provider limit or a human answer, and the
state, the waiting reason and the note say which.

> **Status: current.** The v0.2 CLI is implemented: home and project management, fleet, capacity,
> secrets, lifecycle, backup, migration, control machines, tunnels, orchestrators, MCP and chat are
> all real commands.

## First: exit codes

| Code | Meaning | Typical cause |
|---|---|---|
| 0 | Success | — |
| 1 | Internal / unexpected | I/O, database, parse, config, adapter failure, timeout, verification failure. |
| 2 | Invalid input / usage | Bad id, bad cron, ambiguous prefix, clap error, unknown capacity mode. |
| 3 | Not found | Job, session, node, project, secret, tunnel or orchestrator does not exist. |
| 4 | Conflict | Job exists, state does not allow the action, supervisor already running, lease held, duplicate tunnel, session already switching. |
| 5 | Guard or refusal | A setup hand-off, a missing `--yes`, a control machine refusing local-truth work, a target above the advice without `--override`, a promotion guard, a cwd outside the allow-list. |
| 6 | Provider limited / auth needed | The action needs a provider that is limited. |
| 7 | Remote failure | SSH could not connect, or remote `vowp` is missing. |

With `--json`, an error is printed on stdout as
`{"error":{"kind":"not_found","message":"job n99 not found","hint":"vowp job ls"}}` and the exit code is
the same.

## Install, setup and roles

**`vowp: unknown command` after a build.** Check the spelling against `vowp --help`. Commands that need
a home exit 3 with "vowp home is not initialised"; run `vowp init` in a checkout or point `--home` at
the home.

**The wrong binary runs.** Two installs on `PATH`:

```sh
command -v vowp
vowp --version
```

Remove the old copy or put the intended `~/.local/bin` (or `~/.cargo/bin`) first.
`cargo run -p vowp-cli --bin vowp -- --version` runs the workspace binary directly.

**The build fails on the toolchain.** The workspace pins stable Rust edition 2024 in
`rust-toolchain.toml`. Check `rustup show`; if `rustup` is absent, install the pinned channel from the
file. Do not switch to nightly.

**`cargo install` succeeds but `vowp` is not found.** `cargo install` writes to `~/.cargo/bin` by
default. Add that directory to `PATH`, or install with `--root <dir>` and add `<dir>/bin`. The install
script uses `~/.local/bin`.

**`vowp setup plan` fails on SSH.** It runs a read-only probe over
`ssh -o BatchMode=yes <target>`: OS and architecture, `git`, `tmux`, `curl`, systemd, sudo mode and,
when `vowp` is on the remote `PATH`, `vowp --version`. Test plain SSH first with
`ssh -o BatchMode=yes <target> true`. A missing remote `vowp` is the `install` phase, not a plan
error; a permission failure is a key problem. Fix the key on the home; never put a password in an
argument.

**`vowp setup home` exits 5.** Read the `handoff` object in the JSON (or the text prompt). A hand-off
is a deliberate stop, not a failure:

| Hand-off | What to do |
|---|---|
| sudo password | Run the printed command on the home yourself, then re-run the same setup command. |
| harness login | Complete the harness's own device-code or browser login on the home. |
| secret value | Enter it with `vowp secret set NAME` or `vowp secret request NAME`; never paste it into chat. |
| owner answer | Answer the named question; if it changes the plan, run `setup plan` again. |

**`vowp setup control` says there is no target.** Run `vowp setup home` first, or pass
`--target <home-ssh-target>`. It refuses to retarget an existing control configuration without
`--force`; that guard is deliberate.

**A command says "this is a control machine".** `vowp up`, `vowp down` and `vowp board` need the
home's local truth. Run them on the home. Reads and writes such as `fleet status`, `job add` and
`inbox answer` are routed to the home automatically.

**A command says "vowp home is not initialised".** You are on a machine with neither a home nor a
control config. Run `vowp setup plan`/`setup home` for the home, or `vowp control init`/`setup
control` for this machine. `--home <dir>` must point at an initialised home.

**A version mismatch after an upgrade.** Upgrade both the home and the control machine, and run
`vowp doctor` on the home. A database written by a newer binary is refused rather than guessed at.

## Home and supervisor

**`vowp up` exits 4.** A supervisor already holds the node lock. `vowp status` shows the pid. There is
one supervisor for the whole home, so do not start a second one for another project; use the service
or `vowp down`.

**No heartbeat / `status --check` exits 1.** The heartbeat is older than 180 seconds. Check
`~/.vowp/logs/supervisor.log` and the process. Restart with `vowp down --force` followed by `vowp up`
(or `systemctl --user restart vowp`). A crash is safe: the next start reconciles orphaned runs as
`crash` and continues them.

**`vowp down` hangs.** It waits up to 30 seconds for the lock. If the process is unresponsive, use
`vowp down --force`; if the pid is gone, the stale lock is detected on the next `up`. Do not delete
`vowp.db` or the lock by hand unless you have stopped every VOWP process and have a backup.

**The supervisor restarts repeatedly.** Read the last `ERROR` lines in the log. A failing tick step is
caught and counted per project; a project is paused with a `broken:` reason and an inbox notice so the
other projects keep running. Common causes are a missing harness binary, a corrupt config, a full disk
or a repository that cannot fetch.

**`vowp up --once` finishes immediately.** It runs a single node tick for tests and cron, including
every active project and the snapshot write. Use it to reproduce a loop problem without leaving a
supervisor running.

**`doctor` fails `snapshot`.** The supervisor has not written a valid `~/.vowp/snapshot.json` yet, or
`~/.vowp/tmp` is not writable. Start `vowp up`, check the disk, and do not hand-edit the snapshot.

**`doctor` fails `projects`.** A project database is missing or does not open. Run `vowp project ls`;
for a checkout that exists but is not registered, `vowp project add <path>`. Restore a backup instead
of hand-creating a database.

## Projects and the fleet

| Symptom | Cause and fix |
|---|---|
| `no project: run vowp init` | You are outside a project and the home has zero or several projects. `cd` into the checkout or pass `--project <name\|path>`. |
| `project add` says the path is missing | The path is interpreted **on the home**, not on the control machine. Use the server path. |
| `project add` says a name conflict | Another registry row uses that name or path. `vowp project ls --all`; rename the checkout or archive the old row. |
| A project is paused `broken:` | Read the inbox notice and the log, fix the cause, then `vowp project resume <name>`. |
| `project rm` refuses | It needs `--yes`. It archives the database under `~/.vowp/archive/projects/`; it does not erase production records. |
| A ready job never runs | `vowp fleet queue` shows the reason: capacity, an ownership lock, a provider limit or a paused project. |
| `fleet status` says `stale` | The home is unreachable. Check SSH; the cached answer is labelled `as of`. `--refresh` requires SSH, `--cached` never opens it. |
| `fleet inbox` is empty but a job is parked | The item may be project-scoped and the project archived, or already answered. Try `vowp fleet inbox --all` and `vowp job show <id>`. |

## Jobs

Use `vowp job show <id> --runs --gates` first. The job's state, `parked_reason`, `note`, last run
outcome and gate evidence usually identify the blocker.

| State / symptom | Likely cause | What to do |
|---|---|---|
| `pending` forever | A `needs` job is not `done` (or was cancelled). | `vowp graph` and `vowp job show <id>` show the unmet needs. Cancelled dependencies leave dependents `pending` and flagged `blocked-by-cancelled`; replan or cancel them. |
| `ready` but not running | No free slot, an overlapping ownership lock, a capacity brake, or every rung of the profile is limited. | `vowp fleet queue` and `vowp capacity show` explain it; do not force a second worker. |
| `running` with no new events | The worker may be thinking, or the run may be hung. | After 20 minutes with no event and no commit L1 stalls it; after 8 minutes with zero output it restarts it. Check `vowp session tail <id> --follow`. |
| `running` but the report exists | The run is still attached; VOWP never gates a live run. | Wait for the run to end, or let the stall rules stop it. |
| Report not detected | Missing file, wrong worktree, invalid YAML, wrong `job`/`branch`, a missing commit sha, or `status: partial`/`blocked`. | Read the `report` gate evidence and fix the report. A stale report older than the attempt is ignored; a fix must rewrite it. |
| `gating` forever | A gate command is still running or timed out. | Check the gate timeout (default 1800 s) and the evidence tail. A timed-out gate kills the process group. |
| `fixing` for a long time | A fix or merge child job is still running, or it failed and the parent is waiting. | `vowp job ls --all` and `vowp job show <child>`. A failed child fails the parent and wakes the keeper. |
| `reviewing` with a failed verifier | The verifier could not produce a usable verdict twice. | The job is parked with a question; answer it or retry. Make findings concrete so the next verifier can judge. |
| `merging` stuck | The merge queue is running the repository checks or waiting for the lockfile refresh. | Check the merge budget (2700 s) and `vowp status`. One merge runs at a time per project. |
| `failed` | Retries exhausted or the keeper failed it. | Read the gate evidence and the last run. `vowp job retry <id>` or `vowp job replan <id>`. |
| `parked` | A question, credits, auth, provider block, exhaustion, capacity or a manual park. | Read `parked_reason` and `vowp fleet inbox`; answer the item or `vowp job retry <id>` for a manual park. |
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

## Capacity, limits and waiting

| Symptom | Meaning | Action |
|---|---|---|
| `waiting: RAM … short` | Admission would exceed the measured reserve. | Let running work finish or lower the owner target; do not raise `max_workers` blindly. |
| `waiting: provider …` | The provider's concurrency or a 429 is limiting dispatch. | Wait for the reset or switch a profile rung. |
| `brake: …` in `capacity show` | Memory pressure, swap, disk or a provider limit tripped the brake. | The brake clears by itself; running work is never killed. Free disk or memory if it persists. |
| `capacity set` exits 5 | A target above the advice needs `--override`. | Add `--override` only when you accept the risk; the board shows the override. |
| `capacity set` exits 2 | No `--target` or `--mode`, or an unknown mode. | Use `--mode auto` or `--mode fixed --target N`. |
| A job was OOM-killed | The kernel killed the run, not VOWP. | It is recorded, the job is parked with a notice, and its estimate rises. Reduce the target or the job's memory use. |
| `capacity report` shows no estimates | Fewer than five samples exist. | The labelled default (1.5 GB RAM, 1 CPU) is used until real samples accumulate. |

Provider limit kinds:

| Limit | Meaning | Action |
|---|---|---|
| `quota` | A subscription window is exhausted. | VOWP records the reset (+2 min), switches to the next rung with a capsule and probes back. If no rung remains, the job parks with `ProviderBlocked`; answer after the reset. |
| `credits` | The provider account is out of credit. | Top up, then `vowp inbox answer <id> resolved`. VOWP never switches to another paid provider automatically. |
| `auth` | Login expired. | Log in with the harness's own tooling, then answer the inbox item `resolved`. |
| `rate` | 429/overloaded. | Backoff with jitter and a lowered concurrency cap; the continue does not consume a continue. Wait for the reset. |
| `context` | Context window overflow. | A capsule and a fresh native session on the same harness; no action unless it repeats. |
| `unavailable` | Three consecutive crash/error runs with no limit signal. | The provider is marked unavailable for 15 minutes, then probed. Check the harness binary and network. |

Check `vowp capacity show` for the target, advice and brakes, and `vowp fleet inbox` for the action a
human must take. An inbox answer unparks only the jobs waiting on that item.

## Sessions, surfaces and orchestrators

**`vowp chat` says it needs the home that owns the session.** You are on a control machine and the
local route did not apply. Run `vowp chat <id>` from the control machine (it routes over SSH), or SSH
to the home and run it there. Do not start a local supervisor to get around the message.

**`session switch` exits 6.** The target provider is limited. Pick another rung, or wait for the reset.

**`session switch` exits 4.** The session is already switching, or a turn is running. An active turn
without `--force` is a conflict; stop it or pass `--force` explicitly. The drain phase is used by
automatic switches (failover and context pressure) and by a forced manual switch.

**The target probe fails.** The session stays on the source and is still readable. Check that the target
harness is installed and the executor string parses. For a remote target, run
`vowp --node <n> doctor` yourself before switching.

**Opening a session asks "Active on laptop … Take over?"** Another surface holds the lease. Answer `y`
or pass `--takeover`; declining exits 4. To read without taking the lease, pass `--follow` (or use
`/follow` in chat): that is the read-only mode. Non-TTY input defaults to no and exits 4.

**A surface says "This surface is read-only".** Another surface took the lease, or this chat was
started with `--follow`/`/follow`. An interactive chat then follows until Ctrl-C and accepts no
slash commands; a scripted (non-TTY) chat accepts `/quit`, `/status` and `/follow`. To take the lease
back, reopen with `vowp chat <id> --takeover`. The old surface keeps printing new events but every
write is refused.

**The old app does not show the takeover notice.** VOWP cannot close another vendor's window. Its VOWP
MCP write tools refuse with the notice; the app's own display may lag. Use `vowp chat` to confirm the
current holder and continue.

**`vowp chat` has no executor.** Create the session with `--executor`, or use `/switch <executor>` in
chat. A session also falls back to the first rung of the `worker` profile; if that profile is empty,
the switch is required.

**A session is blocked by a provider limit.** Use `/switch <executor>` or wait for the reset. The log
and capsule remain readable.

**The capsule is invalid twice.** The previous capsule is kept and an `error` event is appended. A
switch without any previous capsule aborts. Check the capsule executor's output; do not hand-edit a
capsule into the log.

**Export/import is rejected.** `unknown bundle format` means the file is not `vowp-bundle-v1`;
`bundle too new` means its protocol version is newer than the binary; `checksum mismatch` means it was
edited or truncated. Nothing is created on rejection. Re-export from the source.

**`attach` falls back to the session log.** tmux is absent or the window is gone. `vowp attach
<job-id>` tails the job's latest session log. To follow it yourself, find the session id with
`vowp session ls --job <job-id>` and run `vowp session tail <session-id> --pretty --follow`.

**`session watch` exits 1 or 2.** Exit 1 means the transcript directory is unreadable; pass
`--claude-dir <dir>` and check permissions. Exit 2 means it was run in a terminal without `--once`;
continuous following is not available. Pass `--once` or run it from cron.

**`orchestrator ls` shows `degraded`.** The app host could not start. Read
`~/.vowp/orchestrators/<name>/` (the `host.json`, `keeper.log` and app config), fix the harness or
port, then `vowp orchestrator resume <name>`. A degraded host does not stop the node supervisor.

**The opencode app reports the MCP server connected but the model never calls VOWP tools.** Some
opencode builds expose MCP configuration without making the tools available to the model. Use Claude
Code (after the project-local `.mcp.json` approval) or `vowp chat` as the tool path; the transcript
mirror and lease still work on opencode.

**The opencode join link or pair code is lost.** Run `vowp orchestrator new` again with `--force`, or
resume and re-read the card with `vowp orchestrator ls --json`. Treat the opencode pair link as a
secret: it is single-use and expires. Open a fresh tunnel rather than sharing a stale one. Claude
Remote Control has no pair link; re-open `https://claude.ai/code` or the Claude mobile app instead.

**`vowp tunnel` says it is opened from the control machine.** Run it on the machine that will use the
forward; the home refuses it. A tunnel that dies immediately usually means the remote port is not
listening or `ExitOnForwardFailure` refused it.

**`session tail` says the session is compacted.** `vowp gc --apply` replaced the full log with a
capsule and `summary.md`. Use `vowp session show <id> --capsule`, or export the capsule. The full-log
replay is intentionally gone.

## Secrets

**`vowp secret set` exits 5 with "run it in a terminal or use `vowp secret request`".** It needs a TTY
for the hidden prompt. From a control machine, the CLI uses `ssh -t` for `secret set`; from a phone,
use an SSH app with a PTY. Otherwise use the one-time page.

**The one-time page URL does not open.** The page is bound to `127.0.0.1` on the home. From a control
machine, `vowp secret request NAME` opens (or reuses) a local tunnel and prints the local URL. From a
phone, use `vowp secret set NAME` instead. The page is single-use and expires after 10 minutes.

**`vowp secret upgrade` exits 5 with a command.** Run that one printed sudo command on the home, then
re-run `vowp secret upgrade`. Without sudo, VOWP stays in basic mode; injection and redaction still
work, but the runtime user owns the secret files.

**A secret value appears in a log or report.** Rotate the secret and remove the value from the
artifact. Redaction replaces injected exact values with `[REDACTED:<NAME>]` on the session-service and
remote-ingest event paths, in the snapshot, in capsules, and in a finished report and run mirror. It
is not applied to the supervisor log, gate command evidence, ledger rows, the `session switch` harness
events or the `session watch` importer, so a value a command printed can remain in those files;
restrict access to `~/.vowp`. The full list is in the [security model](security.md#redaction). If the
value is in a path that should be redacted, stop and report the file and the secret **name** only. Do
not paste the value into a public issue.

**A backup says `secrets/: never backed up`.** That is correct. Restore the home, then set each secret
again with `vowp secret set NAME`. Never put a secret in a backup or an encrypted archive as a
workaround.

## Migration, backup and GC

**`MigrationRequired` / "schema v0.1 database".** Stop the supervisor, then:

```sh
vowp migrate --dry-run
vowp migrate --yes
```

`vowp up` also migrates automatically after taking a backup. `--no-migrate` turns the automatic step
into a clear conflict. Do not delete `vowp.db` to get past the message.

**A migration failed.** The original is kept and a pre-migration backup is printed. Read the failing
table or file in the output; nothing is swapped until verification passes. To return to v0.1:
`vowp migrate --rollback <backup>`. Session logs are not changed by migration or rollback.

**A backup is smaller than expected.** Check `manifest.json` and the `skipped` list. A `.db` path is
the legacy global-only copy; use a directory path for a full backup. Secrets are always skipped.

**Restore refuses to overwrite.** `--to-home` must be empty or absent unless `--force` is passed. Do
not force over a home you still need; restore into a fresh directory and inspect it first.

**`vowp gc` lists files but deletes nothing.** That is the default dry run. `--apply` is required.
Even then, production, truth, open work and secrets are never deleted; seeing them in the report is
expected.

**`vowp gc --apply` did not remove a worktree.** A merged worktree is removed after merge; a failed or
cancelled one is kept for seven days. The branch stays. Check the job's terminal state and the age in
the report.

**`vowp gc --class ...` exits 2.** The class name is wrong. Use `production`, `truth`, `open_work`,
`secrets`, `closed_session`, `run_output`, `worktree`, `sample` or `cache` (a hyphen is accepted for
`open-work`).

## Schedules

- A bad cron or an interval under 60 seconds exits 2. VOWP accepts exactly five whitespace-separated
  cron fields; six-field cron with seconds is rejected.
- `tz` is `UTC` or a fixed offset such as `+02:00`; IANA names are not supported.
- Schedules are per project. With several projects and no selected checkout, pass `--project`; use
  `vowp schedule --all ls` to see every project's rows.
- A schedule that was missed while the node was down fires once with `run_once`, or records a skip with
  `skip`. It never replays every missed occurrence.
- A paused schedule does not fire; `vowp schedule resume <name>` enables it. `vowp loops` shows the L9
  loop itself.
- A command action that fails records `failed: exit N` and opens at most one inbox notice per schedule
  per day.

## Costs

`vowp cost` shows `receipt`, `price-table`, `unknown` or `mixed` per row. An unknown cost is `?` and is
counted separately; it is never assumed free. If every row is unknown, the harness is not reporting
usage and no `[providers.<p>.prices]` entry exists for the model. Add a price table only for models you
know; otherwise leave it unknown.

## Database and reindex

- **Schema too new.** The database was written by a newer binary. Upgrade the binary; if you must go
  back, restore a pre-upgrade backup. VOWP refuses to guess at a newer schema.
- **Corruption.** Stop the supervisor, copy the home aside, and restore a backup. `vowp reindex` only
  rebuilds session indexes from `log.jsonl`; it does not repair jobs or decisions.
- **Missing session rows.** `vowp reindex` re-registers session directories. A session directory copied
  from another node opens after a reindex; its node becomes the local node name.
- **Backups.** `vowp backup` is safe while the supervisor runs and includes the session content. It
  does not include secrets; restore those with `vowp secret set`.

## tmux

| Symptom | Fix |
|---|---|
| `tmux: command not found` | Install tmux. `vowp up --no-tmux` skips the initial layout but does not let jobs run without windows. |
| No `vowp-home` or `vowp-<project>` session | `vowp up` creates them; check the log for a tmux error. |
| A job window is gone | The window is a view; the session log is intact. `vowp attach <job-id>` tails the job's latest session log. |
| Windows from an old project remain | `vowp down --kill-windows` closes only tagged windows for this home. VOWP never kills untagged windows. |
| A socket or `default` refusal | VOWP uses its private socket. Do not run `tmux -L default` or `tmux kill-server`; use `vowp down --kill-windows`. |

## Remote nodes, tunnels and the control machine

| Symptom | Fix |
|---|---|
| Exit 7 | Test `ssh -o BatchMode=yes <target> vowp --version` from the same user and shell. |
| `vowp: command not found` remotely | Fix the remote non-interactive `PATH`, or install to `~/.local/bin`. |
| `this is a control machine` | The command needs local truth. Run `up`, `down` or `board` on the home. |
| `no control configuration` | Run `vowp control init --name home <home-ssh-target>` on this machine. |
| Control writes fail | Writes never use the cache. Fix SSH or the control config; the cached `fleet status` is read-only. |
| Tunnel dies immediately | The app host or remote port is not listening, or `ExitOnForwardFailure` refused it. Check the host and the port. |
| Host key prompt | Add the host key to `known_hosts`; batch mode must not prompt. |
| An argument was mangled | VOWP single-quote escapes the whole remote command; check for a shell alias or wrapper on the remote. |
| Version mismatch | Upgrade both binaries; a newer database is refused rather than guessed at. |

## What to collect for a report

When you need help, collect:

```sh
vowp status --json
vowp fleet status --json
vowp fleet queue --json
vowp fleet inbox --json
vowp capacity report --json
vowp doctor --json
vowp job show <id> --runs --gates
vowp graph
vowp gc --json
vowp control status --json
tail -n 200 ~/.vowp/logs/supervisor.log
```

Also include the binary version (`vowp --version`), the operating system, the home/control role, and
the exact command and exit code. **Redact secrets and sensitive prompts**: replace values with
`<redacted>`, refer to variables by name, and do not attach a full `~/.vowp` directory to a public
issue. If the problem is a vulnerability, use the private advisory link in the
[security model](security.md), not a public issue.

## When to stop instead of working around

Do not:

- delete `vowp.db`, the project databases, the session logs or the lock file to "reset" a stuck
  supervisor;
- widen `owns` or add paths to `always_allowed` to make `owns_respected` pass;
- disable `secret_scan` or `merge.check_cmds` to get a merge through;
- retry a refused promotion with `--yes` when the environment requires human approval;
- run a second supervisor for the same home, or a second `VOWP_HOME` per project on a v0.2 home;
- paste a secret into chat, a report, an issue or a backup;
- hand-edit a capsule, session log, secret file or database.

Those actions destroy the evidence that the next tick, the verifier or the owner needs. Park the job,
write the question in the report or inbox, and keep unrelated work moving.
