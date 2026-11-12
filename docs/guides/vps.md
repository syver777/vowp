# Running a home on a VPS

VOWP 0.2 is VPS-first. One Linux server is the **home**: it owns the global database, one database per
project, every session log and the single node supervisor. A laptop, desktop or phone is a **control
machine**: it talks to the home, caches the last snapshot and opens tunnels, but holds no truth. This
guide takes a fresh server to a supervised fleet and, where useful, points at the control-machine
side.

> **Status: current.** The home, project, fleet, capacity, secret, backup and lifecycle commands on
> this page are implemented. `vowp setup home` installs the same pieces as the manual steps below.

The examples use placeholders:

| Placeholder | Replace with |
|---|---|
| `<your-vps>` | The home's hostname or address. |
| `<your-user>` | The Linux account that runs VOWP. The setup can create a dedicated `vowp` user. |
| `<your-repo>` | The path of a git checkout on the home. |
| `<your-project>` | The project name passed to `vowp init` or `project add`. |
| `<your-harness>` | `opencode` or `claude-code`, installed and signed in on the home. |
| `<your-model>` | A model identifier that harness accepts. |
| `<your-provider>` | The provider key used by limits and cost: `claude` for Claude Code, the text before `/` for opencode, or the mapping name for a generic CLI. |
| `<home-ssh-target>` | The SSH destination of the home (`<your-user>@<your-vps>` or an alias). |

## Before you start

- Linux on `x86_64` or `arm64`.
- A non-root account with key-only SSH. The setup can create the account with sudo; without sudo it
  runs as the SSH user and records basic secret mode.
- `git`, `tmux`, `curl` and a C toolchain for the bundled SQLite build.
- At least one harness installed and signed in **on the home**, with its own login flow.
- A git repository with a remote and a default branch. The merge queue fetches and pushes it.

The recommended way to install the home is the setup path in [Install](install.md):

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 <home-ssh-target> true
vowp setup plan --target <home-ssh-target> \
  --repository <your-repo> --harness <your-harness> --provider <your-provider>
vowp setup home --target <home-ssh-target> --apply --yes \
  --repository <your-repo> --harness <your-harness> --provider <your-provider>
```

`setup home` creates the `vowp` user when sudo allows it, installs the binary, the home, the user
service and the vault, and stops at every human hand-off. It never asks for a secret value in chat.

If you install by hand, the rest of this page is the same sequence: prepare the account, install the
binary and unit, initialise the first project, configure profiles, then start the supervisor.

## Prepare the account and SSH

Run VOWP as an account that owns only the projects, harness logins and home it needs. Do not run the
supervisor as `root`.

```sh
sudo adduser <your-user>
sudo -iu <your-user>
```

Install the owner's public key for `<your-user>` before relying on key-only access:

```sh
ssh -o BatchMode=yes <your-user>@<your-vps> true && echo "key login ok"
```

A missing key must fail instead of prompting. Keep the host key in `known_hosts`; do not use
`sshpass` or store a password in a script. If you need a key, create one with
`ssh-keygen -t ed25519` and copy the **public** key with `ssh-copy-id`.

## Install the binary and the service

From a checkout of this repository as `<your-user>`:

```sh
cargo build --release --workspace
./scripts/install.sh --bin target/release/vowp
vowp --version
```

The script installs `~/.local/bin/vowp`, the user unit `vowp.service` and the five-minute watchdog.
`--no-service` leaves the unit installed but does not enable or start it; `--no-watchdog` skips the
watchdog script and its crontab entry (it implies `--no-cron`); `--no-cron` alone only leaves the
crontab unchanged. Use `--linger` so the user manager stays available after logout. The unit runs
`vowp up --foreground` with `VOWP_HOME=%h/.vowp`; there is one service for the whole home, not one
per project.

Check the prerequisites as that user:

```sh
git --version
tmux -V
cargo --version
rustc --version
```

Install and sign in to the harness with its own tooling. A harness that needs a provider key reads it
from the environment exactly as it does outside VOWP; put the value in the environment, the
harness's own store, or a VOWP secret named in `env_pass`, never in `vowp.toml`. VOWP stores the
values it manages in its secret store and can also forward a listed `env_pass` name from the process
environment; see [Secrets](#secrets) and the [security model](../security.md#secrets).

## Register projects

A project is one git repository on the home. The first project can create the home:

```sh
cd <your-repo>
git status
git remote -v
vowp init --name <your-project>
```

`vowp init` writes `vowp.toml`, the framework files, `.vowp/jobs` and `.vowp/checkpoints`, creates
`~/.vowp/vowp.db` and `~/.vowp/projects/<your-project>/project.db`, registers the local node and the
project, and seeds the default loops. Commit the generated files before the first run:

```sh
printf '.vowp/\n' >> .gitignore
git add -A
git commit -m "chore: initialise VOWP"
```

For every further repository that already has `vowp.toml`:

```sh
vowp project add <your-repo>
vowp project ls
```

`project add` validates the checkout, registers it, creates its database, loop rows and worker slots,
and does not start a supervisor; the next `up` picks it up. The registry commands work from any
directory on the home. `_node` is reserved and cannot be a user project.

Manage the fleet:

```sh
vowp project pause <your-project>       # stop new dispatch; running work finishes
vowp project resume <your-project>
vowp project archive <your-project>     # no ticks; the database stays
vowp project weight <your-project> 2    # fairness weight, 1..=100
vowp project rm <your-project> --yes    # archive the database and remove the registry row
```

`project rm` keeps production records and moves the project's directory under
`~/.vowp/archive/projects/`. It is not a way to erase history.

## Configure profiles and harnesses

A role's **profile** is an ordered ladder of executors. The first rung whose provider is available
runs the work. The executor grammar is:

```text
harness ":" model [ "@" effort ] [ "#" variant ]
```

`@effort` is Claude Code only and `#variant` is opencode only. A built-in harness uses
`<your-harness>:<your-model>`; a generic CLI uses `cli:<your-mapping>`, where the model part is the
mapping name. Edit `[profiles.*]` and `[harnesses.*]` in `vowp.toml` to match what is installed on
the home.

For `claude-code` or `opencode`:

```toml
[node]
max_workers = 4          # a per-project preference; capacity decides admission
max_verifiers = 2

[profiles.planner]
ladder = ["claude-code:<your-model>@medium", "opencode:<your-provider>/<your-model>"]
on_quota = "next_then_probe_back"

[profiles.keeper]
ladder = ["opencode:<your-provider>/<your-model>"]

[profiles.worker]
ladder = ["opencode:<your-provider>/<your-model>"]

[profiles.verifier]
ladder = ["opencode:<your-provider>/<your-model>"]

[harnesses.opencode]
env_pass = ["<YOUR_PROVIDER_API_KEY>"]   # names only, never values

[providers.<your-provider>]
max_concurrent = 2
```

For a generic CLI, key the harness table by the mapping name and use `cli:<your-mapping>` in the
ladder:

```toml
[profiles.worker]
ladder = ["cli:<your-mapping>"]

[harnesses.<your-mapping>]
mapping = ".vowp/adapters/<your-mapping>.toml"
env_pass = ["<YOUR_PROVIDER_API_KEY>"]   # names only

[providers.<your-mapping>]
max_concurrent = 2
```

All four roles must have a profile. Points to check:

- **Ladder order.** Rung 0 is preferred. Add only providers you are willing to pay or quota-limit;
  VOWP never switches to a provider that is not in the ladder.
- **`on_quota`.** `next_then_probe_back` moves to the next rung and probes the limited one after its
  reset; `next` stays on the next rung; `park` asks a human.
- **Built-in harnesses.** `claude-code` and `opencode` are built in; their table keys are those
  names. Generic CLI uses `cli:<mapping>` and `[harnesses.<mapping>]`; do not write `[harnesses.cli]`.
- **Providers.** Use the provider key from the executor: `claude` for Claude Code, the text before the
  first `/` for opencode, the mapping name for a generic CLI.
- **Secrets.** Put values in the secret store or the environment, not in `vowp.toml`. `env_pass` lists
  the names to inject into the harness child process; VOWP takes the store value when set and
  otherwise forwards the environment value.
- **Costs.** If a harness reports usage without a cost, add
  `[providers.<your-provider>.prices."<your-model>"]` with current per-million-token prices.
  Without it, VOWP reports the cost as `unknown` rather than guessing.

A session can move to another executor later with `vowp session switch` or `/switch` in
`vowp chat`; see [Sessions and surfaces](sessions.md).

## Check the home

```sh
vowp doctor
```

`doctor` prints `ok`, `warn` or `fail` per check and exits 0 only when nothing failed. The checks that
matter most on a home:

| Check | If it reports a problem |
|---|---|
| `home` | Make `~/.vowp` writable by `<your-user>`, or pass the same `--home` consistently. |
| `db` | The global database is `$VOWP_HOME/vowp.db` (default `~/.vowp/vowp.db`). Never delete it; restore a backup or run `vowp migrate --dry-run` for a v0.1 home. |
| `projects` | A project database is missing or does not open. Run `vowp project ls`; a checkout may need `vowp project add`. |
| `snapshot` | The supervisor has not written a parseable `snapshot.json` yet, or `tmp/` is not writable. Start the supervisor. |
| `git` / `tmux` | Install the missing program or fix `PATH`. tmux is a warning, but jobs need windows. |
| `config` | Fix the named key in `vowp.toml`; unknown keys are errors. |
| `harness:claude-code` / `harness:opencode` | Install the harness or set `[harnesses.<name>].binary` to its absolute path. |
| `providers` | Set the named environment variable for the service. The value is never printed. |
| `systemd` / `watchdog` | Install the unit and cron check with `./scripts/install.sh`, or accept the warning. |
| `supervisor` | Not running. Start `vowp up` or the service if you expect one. |
| `disk` | Free at least 1 GB; runs and blobs grow with tool output. |

A missing harness is a `fail` because dispatch would fail later; missing tmux is a `warn`, but a
dispatched job still needs a window.

## Start the supervisor

```sh
vowp up                          # ensure the tmux layout, detach the supervisor
vowp up --foreground             # run the loop in this process (what systemd runs)
vowp up --no-tmux                # skip windows; jobs still need tmux
vowp up --once                   # exactly one node tick, then exit
```

There is **one supervisor for every active project**. `up` creates a `vowp-home` tmux session with
`board` and `capacity`, plus one `vowp-<project>` session for every non-archived project (including
paused ones). `--once` is useful as a smoke test; it runs the node steps, every project in
round-robin order and the snapshot write. `up` exits 4 if a supervisor already holds the node lock;
`vowp status` shows the pid.

`up` runs the v0.1-to-v0.2 migration automatically for an old home, after taking a backup. Pass
`--no-migrate` only when you intentionally want the refusal instead. The migration refuses while a
supervisor is running.

When the web-window daemon starts, `vowp up` prints its loopback URL and the control-machine
command for that actual port:

```text
web window: http://127.0.0.1:54321/
remote window: vowp tunnel open window --remote-port 54321
  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)
```

On the laptop or phone, configure the home once as a control machine
(`vowp control init --name home <home-ssh-target>`), run the printed
`vowp tunnel open window --remote-port 54321`, and open the local loopback URL it prints. Pass
`--local-port <n>` when you need a fixed local port. The daemon binds `127.0.0.1` on the home;
the existing managed SSH forward is the only route, no public port is opened, and the firewall
still allows only SSH. A missing daemon binary warns and prints no tunnel hint while the
supervisor keeps running; `up --once` starts no daemon.

Reads need no token. Set `VOWP_API_TOKEN` in the environment that runs `vowp up` (for example a
mode-0600 `EnvironmentFile=` on the user unit) and enter the value in the page's unlock panel to
enable the inbox answer and session switch writes. The page keeps it in `sessionStorage`, never
in the URL; `vowp up` never prints it. Rotating it means restarting the daemon with the new value
and unlocking again.

Under systemd, do not also start a detached `vowp up`. The unit owns the process:

```sh
systemctl --user status vowp
journalctl --user -u vowp -f
systemctl --user restart vowp
systemctl --user stop vowp
```

If `systemctl --user` cannot reach the user manager over SSH, set
`XDG_RUNTIME_DIR=/run/user/$(id -u)` in that shell. `loginctl enable-linger <your-user>` keeps the
manager available when you are logged out.

## Status, fleet and windows

```sh
vowp status
vowp status --check
vowp fleet status
vowp fleet queue
vowp fleet inbox
vowp board --once
vowp attach <job-id>
```

`status` reports the supervisor, heartbeat, jobs across projects, slots, limits, cost today and the
open inbox. `status --check` exits 1 when the heartbeat is older than three minutes; that is the
watchdog contract. `fleet status` is the fleet view, `fleet queue` lists queued jobs and why they are
waiting, and `fleet inbox` lists open questions and decisions.

`attach` needs a terminal, so use SSH's TTY allocation from another machine:

```sh
ssh -t <home-ssh-target> 'cd /path/to/<your-repo> && vowp attach <job-id>'
```

If tmux is unavailable, `vowp attach <job-id>` tails the job's latest session log. To follow it
yourself, find the session id with `vowp session ls --job <job-id>` and run
`vowp session tail <session-id> --pretty --follow`.

## Capacity

You do not set a hard maximum. The supervisor measures the home, admits work against a reserve,
brakes when the machine is under pressure, and asks the AI keeper for advice. The owner decides the
target; the code enforces it.

```sh
vowp capacity show
vowp capacity report --since 7d
vowp capacity set --target 5 --reason "five workers during the migration"
vowp capacity set --target 8 --override --reason "owner override for the release"
vowp capacity set --mode auto
```

- `auto` is the default: the measured advice becomes the target.
- `fixed` uses the owner's number. A target above the advice needs `--override`; the board then shows
  `owner override: 8 (advised 5)`.
- The brakes stop new admissions when memory pressure, swap, disk or a provider limit says so. They
  never kill running work.
- A queued job shows its reason, such as `waiting: RAM 1.2 GB short` or `waiting: provider deepseek
  429`.
- `[node].max_workers` and `max_verifiers` are per-project preferences, not a product-imposed cap.

The full model is in [Operating a home](operating.md#capacity) and the
[capacity specification](../design/v0.2/home-and-surfaces.md#6-capacity-measured-admitted-braked-advised).

## tmux safety

VOWP runs its own private tmux server, not the account's default server. The socket name is derived
from the home, and every internal command runs with `-L <socket>` and
`TMUX_TMPDIR=<home>/tmux`. At startup VOWP records the socket and server pid in
`<home>/tmux-server`; `vowp down --kill-windows` refuses a server that does not match that record.

- Use `vowp down --kill-windows` to close this home's windows. Do not run `tmux kill-server`.
- Do not run `tmux -L default`; VOWP refuses the literal `default` socket.
- Do not point `TMUX_TMPDIR` at a shared directory such as `/tmp` for VOWP. Let VOWP use its home.
- If you write scripts or tests, use a unique `tmux -L <unique>` socket **and** a private
  `TMUX_TMPDIR` created before the first tmux command. See
  [Contributing](../../CONTRIBUTING.md#checks-before-a-pull-request).

This keeps the supervisor away from any interactive tmux session you or another user may have.

## Secrets

Secrets are referred to by name. There are two modes:

| Mode | When | Storage |
|---|---|---|
| **vault user** | The `vowp-vault` user and helper exist (sudo allows them). | `/home/vowp-vault/secrets/<NAME>`, mode 0600. |
| **basic** | No sudo or no vault user. | `~/.vowp/secrets/<NAME>`, mode 0600, owned by the runtime user. |

```sh
vowp secret set <NAME>          # hidden prompt; the value is never echoed or in argv
vowp secret ls                  # names and last four characters only
vowp secret request <NAME>      # one-time loopback paste page, for a desktop owner
vowp secret rm <NAME> --yes
vowp secret upgrade             # prints the one sudo command that creates the vault user
```

On a control machine, `secret set/ls/rm/upgrade` route to the home over SSH; `secret request` opens a
local tunnel so the browser sees a control-machine loopback URL. A harness receives only the names
listed in `env_pass`; VOWP injects those names into that child's environment, from the secret store
when set and otherwise from the process environment.

Redaction is partial and timed. Event payloads and `raw` appended by the session service or the
remote ingest path are redacted before writing, as is the snapshot; a capsule is redacted after the
writer returns; the job report and latest run mirror are redacted after each turn finishes.
Supervisor logs, gate command evidence, ledger detail rows, the switch turn path and the
`session watch` importer are written unredacted today, and a report or run mirror is still raw while
the turn is running. Treat the home and its databases as sensitive; avoid commands that print secret
values.

The honest limit: any agent that can run arbitrary code in a process that holds a secret can print
it. **Basic mode does not separate the value from the agent's user**: `~/.vowp/secrets/<NAME>` is mode
0600 but owned by the runtime user, and a harness spawned as that user can read it. **Vault mode
keeps the files out of the runtime user's direct readable tree**: they live under `vowp-vault` at
mode 0600. It is not isolation from code that can invoke the helper: the sudoers rule lets the
runtime user run the helper as `vowp-vault` with no argument restriction, and `__vault-exec` can
read any stored name and run the caller's command as `vowp-vault`. Treat the helper as the trust
boundary; `env_pass` is not an allow-list for it. Neither mode encrypts the file at rest. VOWP keeps
values out of configuration and git, and redacts exact injected values on the event paths above, but
it does not keep them out of every database row or every log and cannot make a secret unreadable to
code that is given it. Harness logins stay under the harness's own control. The full statement is in
the [security model](../security.md#secrets).

## Back up and collect garbage

```sh
vowp backup
vowp backup --to /mnt/backup/
vowp backup --to rsync://backup.example/vowp --encrypt age1...
vowp backup --restore /mnt/backup/<timestamp> --to-home /srv/vowp-restored
vowp gc
vowp gc --apply
```

A backup is one directory with the global database, every project database, the authoritative session
content and a manifest with hashes and row counts. The default rotation keeps seven daily and four
weekly backups. Secrets are never backed up; restore them with `vowp secret set`.

`vowp gc` is a dry run by default. `--apply` collects only `closed_session`, `run_output`,
`worktree`, `sample` and expired `cache` entries; it never deletes production, truth, open work or
secrets. The supervisor creates a nightly `gc-nightly` schedule in each project. See
[Operating a home](operating.md#backups-and-restore) for the restore drill and compaction behaviour.

## Multiple projects and worker capacity

v0.2 runs several projects in one home. There is no separate `VOWP_HOME` per project and no second
supervisor:

```sh
vowp project add /srv/projects/alpha
vowp project add /srv/projects/beta --weight 2
vowp project ls
vowp up
```

Each project has its own `projects/<name>/project.db`, its own tmux session and its own jobs. The node
supervisor ticks them round-robin, sharing the home's CPU, memory, harness logins and provider limits;
capacity admission and the project weight decide who runs next. A broken project parks itself and
opens an inbox notice without stopping the others. `vowp project pause` stops new dispatch while
running work finishes.

The old "one project per `VOWP_HOME`" workaround belongs to v0.1. Do not use it on a v0.2 home.

## Multiple nodes and the control machine

A laptop or phone does not need its own supervisor. Configure it as a control machine, then read and
write through the home:

```sh
vowp control init --name home <home-ssh-target>
vowp control status
vowp fleet status
```

Every project/fleet command on the control machine routes to the home over SSH; `vowp up`, `down` and
`board` refuse there because they need local truth. The rest of the machine-to-machine details,
including tunnels and executor nodes, are in
[Control machines and extra nodes](multi-node.md).

## Upgrade and migration

1. `vowp down` (or `systemctl --user stop vowp`).
2. Install the new binary over the old one.
3. `vowp doctor` to check the schema and the projects.
4. `vowp up`.

A v0.1 home migrates to the split global + per-project layout on the first `vowp up`, after a backup
under `~/.vowp/backups/pre-0.2-<timestamp>.db`. To inspect first:

```sh
vowp migrate --dry-run
vowp migrate --yes
```

`vowp migrate --rollback <backup>` restores the pre-migration home. Migration refuses while a
supervisor is running; session logs are not changed by it. The database written by a newer binary is
refused rather than guessed at.

## SSH and firewall

The home opens no public listening socket. The only inbound service is SSH. The opencode app host,
the one-time secret page and the v0.3 web window daemon bind loopback and are reached through an
SSH tunnel; never expose them on a public interface. Claude Remote Control opens no local port: it
connects outbound to claude.ai.

On a server with `ufw`:

```sh
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH
sudo ufw enable
sudo ufw status
```

Apply the same rule in the provider's firewall: allow SSH from your addresses and nothing else. Keep
key-only authentication after keys work, and keep the host key in `known_hosts`. The account needs
outbound access to the git remote and to the harness's provider.

## Troubleshooting a first run

| Symptom | Check |
|---|---|
| `vowp: command not found` under systemd | `PATH` and `ExecStart` in the unit; install with the script so the path is `~/.local/bin/vowp`. |
| `vowp setup home` exits 5 | Read the `handoff` object: sudo command, harness login, secret name or owner answer. Finish that, re-run the same command. |
| `vowp up` exits 4 | A supervisor is already running. `vowp status` shows it; use the service instead of a second `up`. |
| `vowp up` prints no `web window:` line | The daemon binary is missing or did not start; the warning names the reason. Install `vowp-daemon` beside `vowp` or set `VOWP_DAEMON_BIN`. |
| The window URL does not open from the laptop | Open the tunnel's local loopback URL, not the home's `127.0.0.1` address; run `vowp tunnel ls` from the machine that opened it. |
| `doctor` fails a harness | The harness is installed and signed in as `<your-user>`; check `[harnesses.<name>].binary`. |
| `doctor` reports a provider `absent` | Set the variable in the service environment; VOWP only reports the name. |
| No tmux session after `up` | Read `~/.vowp/logs/supervisor.log`; install tmux and check the private socket directory. |
| A job is parked | Read `vowp inbox ls` and `vowp job show <id>`; a provider limit or a question is the usual cause. |
| A project is paused `broken:` | Fix the reason in the inbox, then `vowp project resume <name>`. |
| Permission denied on the home | Run every command as the runtime user; `~/.vowp` must be owned by that account. |
| `MigrationRequired` | Stop the supervisor and run `vowp migrate --dry-run`, then `vowp migrate --yes`. |

The [troubleshooting guide](../troubleshooting.md) covers exit codes, gates, capacity, sessions and
remote failures. Next: [Control machines and extra nodes](multi-node.md), or
[Sessions and surfaces](sessions.md) for the app and phone side.
