# Running VOWP on a VPS

VOWP runs on any Linux server that can run the supported toolchain, `git`, `tmux` and a signed-in
harness. A VPS is not a special kind of node: it is a Linux account with its own VOWP home, its own
project checkout and its own harness logins. This guide takes a fresh server to a supervised project.

The examples use placeholders so they work with any server, provider and harness:

| Placeholder | Replace with |
|---|---|
| `<your-vps>` | The server's hostname or address. |
| `<your-user>` | The dedicated Linux account that runs VOWP. |
| `<your-repo>` | The git URL of the project VOWP will manage. |
| `<your-project>` | The project directory name and the name passed to `vowp init`. |
| `<your-harness>` | A built-in harness: `claude-code` or `opencode`. |
| `<your-model>` | A model identifier that your built-in harness accepts. |
| `<your-mapping>` | The name of a generic `cli` adapter mapping, used as `cli:<your-mapping>` and `[harnesses.<your-mapping>]`. |
| `<your-provider>` | The provider key used by limits and cost: `claude` for Claude Code, the text before `/` for opencode, or `<your-mapping>` for a generic CLI. |
| `<your-node>` | The node name you register on another machine, when you use more than one node. |

> **Status: current.** Every command on this page is implemented. The systemd unit is a generic
> example; adapt the paths to your server.

## Requirements

- Linux on x86_64 or arm64. The v0.1 release supports Linux and macOS; this guide covers Linux.
- Rust from the pinned [`rust-toolchain.toml`](../../rust-toolchain.toml), `git`, `tmux`, and a C
  toolchain for the bundled SQLite build.
- A harness installed and signed in as the dedicated user. VOWP drives the harness's documented
  headless interface and never reads or stores its login.
- A git repository with a remote and a default branch. The merge queue fetches and pushes that remote.
- Optional but recommended: `systemd` for a supervised, restart-on-failure service.

On Debian or Ubuntu, install the system packages as an administrator:

```sh
sudo apt-get update
sudo apt-get install -y build-essential curl git pkg-config tmux
```

On Fedora or RHEL:

```sh
sudo dnf install -y curl gcc git make pkgconf-pkg-config tmux
```

Use your distribution's package manager if it is neither of these. VOWP does not install system
packages itself.

## Create a dedicated user

Run VOWP as an account that owns only the projects and harness logins it needs. Do not run the
supervisor as `root`.

```sh
sudo adduser <your-user>
# On systems without adduser:
# sudo useradd --create-home --shell /bin/bash <your-user>

sudo -iu <your-user>
```

Install your SSH public key for `<your-user>` in `/home/<your-user>/.ssh/authorized_keys` before you
rely on key-only access. Then install the Rust toolchain as that user:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
rustup show
```

The repository pins its toolchain and edition, so `cargo` uses the pinned stable release once
`rustup` is installed. Check the rest of the prerequisites:

```sh
git --version
tmux -V
cargo --version
rustc --version
```

Install and sign in to your harness with its own tooling, as `<your-user>`. Run its login and version
commands exactly as its documentation says; for a generic CLI, that is the command named in your
mapping file.

A harness that needs a provider key reads it from the environment exactly as it does outside VOWP.
VOWP stores only environment-variable **names**, never values. See the
[security model](../security.md#secrets).

## Clone, build and install

```sh
mkdir -p ~/src
git clone <your-repo> ~/src/<your-project>
cd ~/src/<your-project>
cargo build --release --workspace
cargo install --path . --locked
vowp --version
```

`cargo install` writes the binary to `~/.cargo/bin`. If that directory is not on `PATH` for
non-interactive shells, add it in your shell profile or use the absolute path
`/home/<your-user>/.cargo/bin/vowp` in the systemd unit below. The first build takes a few minutes.

## Initialise the project

The project checkout must be a git repository with a clean working tree and a remote it can push to.

```sh
cd ~/src/<your-project>
git status
git remote -v
vowp init --name <your-project>
```

`vowp init` writes `vowp.toml`, installs the framework files (`AGENTS.md`, `CLAUDE.md`, the four
`vowp-*` skills and `opencode.jsonc`), creates `.vowp/`, registers the local node and seeds the
default loops. Commit the generated files on your project branch before the first run so the merge
queue starts from a clean tree.

## Configure profiles and harnesses

A role's **profile** is an ordered ladder of executors. The first rung whose provider is available
runs the work. The executor grammar is:

```text
harness ":" model [ "@" effort ] [ "#" variant ]
```

`@effort` is Claude Code only and `#variant` is opencode only. A built-in harness uses
`<your-harness>:<your-model>`. A generic CLI uses `cli:<your-mapping>`, where the model part is the
mapping name. Edit the `[profiles.*]` and `[harnesses.*]` tables in `vowp.toml` to match what you
installed.

For `claude-code` or `opencode`:

```toml
[node]
max_workers = 2          # start small on a small server
max_verifiers = 1

[profiles.planner]
ladder = ["<your-harness>:<your-model>"]
on_quota = "next_then_probe_back"

[profiles.keeper]
ladder = ["<your-harness>:<your-model>"]

[profiles.worker]
ladder = ["<your-harness>:<your-model>"]

[profiles.verifier]
ladder = ["<your-harness>:<your-model>"]

[harnesses.<your-harness>]
binary = "<path-to-your-harness>"
env_pass = ["<YOUR_PROVIDER_API_KEY>"]   # names only, never values

[providers.<your-provider>]
max_concurrent = 2
```

For a generic CLI, key the harness table by the mapping name and use `cli:<your-mapping>` in the
ladder:

```toml
[profiles.planner]
ladder = ["cli:<your-mapping>"]
on_quota = "next_then_probe_back"

[profiles.keeper]
ladder = ["cli:<your-mapping>"]

[profiles.worker]
ladder = ["cli:<your-mapping>"]

[profiles.verifier]
ladder = ["cli:<your-mapping>"]

[harnesses.<your-mapping>]
mapping = ".vowp/adapters/<your-mapping>.toml"
env_pass = ["<YOUR_PROVIDER_API_KEY>"]   # names only, never values

[providers.<your-mapping>]
max_concurrent = 2
```

All four roles must have a profile. Points to check:

- **Ladder order.** Rung 0 is preferred. Add more rungs only for providers you are willing to use;
  VOWP never switches to a provider that is not in the ladder.
- **`on_quota`.** `next_then_probe_back` moves to the next rung and probes the limited one after its
  reset; `next` stays on the next rung; `park` asks a human. All three are supported.
- **Built-in harnesses.** `claude-code` and `opencode` are built in. Their table keys are those names,
  and the executor is `<harness>:<model>`.
- **Generic CLI.** The executor is `cli:<your-mapping>`, and the table is
  `[harnesses.<your-mapping>]` with `mapping = ...`. Do not write `[harnesses.cli]` or a
  `<mapping>:<model>` executor; the parser only accepts `cli` as the harness name.
- **Provider table.** Use the provider key from the executor. For Claude Code it is `claude`; for
  opencode it is the text before the first `/`; for a generic CLI it is the mapping name from
  `cli:<mapping>`.
- **Secrets.** Put provider values in the environment, not in `vowp.toml`. `env_pass` lists the
  variable names to forward to the harness child process.
- **Optional price table.** If a harness reports usage without a cost, you can add
  `[providers.<your-provider>.prices."<your-model>"]` with the provider's current per-million-token
  prices. Without it, VOWP reports the cost as `unknown` rather than guessing.

To try more than one harness or model, add rungs to a ladder in preference order. A session can also
be moved to a different executor later with `vowp session switch`; see the
[sessions guide](sessions.md).

## Check the node

```sh
vowp doctor
```

`doctor` prints `ok`, `warn` or `fail` per check and exits 0 only when nothing failed. The checks
that matter most on a VPS:

| Check | If it reports a problem |
|---|---|
| `home` | Make the node home (`~/.vowp` by default) writable by `<your-user>`. |
| `db` | Move an unsupported or corrupt database aside; do not delete a database from a newer binary. |
| `git` / `tmux` | Install the missing program or fix `PATH`; tmux is a warning, but jobs need it. |
| `config` | Fix the named key in `vowp.toml`. |
| `harness:claude-code` / `harness:opencode` | Install the harness or set `[harnesses.<name>].binary` to its absolute path. |
| `providers` | Set the named environment variable in the environment that starts the supervisor. An absent variable is a warning, but the harness may still need it. |

A missing harness is a `fail` because dispatch would fail later. A missing tmux is a `warn`, but a
dispatched job still needs a window.

## First plan and run

Let the planner propose a graph before you create jobs:

```sh
vowp plan "<your goal>" --dry-run
```

`--dry-run` prints the proposed YAML and creates no jobs. Review it, then run the same command
without `--dry-run` to let the planner create the jobs. You can also add one job directly:

```sh
vowp job add \
  --id hello-docs \
  --title "Write the hello page" \
  --kind docs \
  --owns "docs/**" \
  --gates "report,owns_respected,secret_scan,review" \
  --brief - <<'BRIEF'
# Goal

Write `docs/hello.md` with a short introduction to the project.

# Done when

The page exists and the report has `status: done`.
BRIEF

vowp graph
vowp job show hello-docs --gates
```

Run one supervisor tick as a smoke test, then start it for real:

```sh
vowp up --once
vowp status
```

`--once` runs a single tick and exits. For the first long run, either leave `vowp up` running in a
detached tmux layout or use the systemd unit below. `vowp up` exits 4 if a supervisor is already
running for this node home.

## Run `vowp up` under systemd

A user unit keeps the supervisor running across SSH disconnects and server reboots. Create
`~/.config/systemd/user/vowp-<your-project>.service` as `<your-user>`:

```ini
[Unit]
Description=VOWP supervisor for <your-project>
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/home/<your-user>/src/<your-project>
Environment=HOME=/home/<your-user>
Environment=PATH=/home/<your-user>/.cargo/bin:/usr/local/bin:/usr/bin:/bin
EnvironmentFile=-/home/<your-user>/.config/vowp/<your-project>.env
ExecStart=/home/<your-user>/.cargo/bin/vowp up --foreground
Restart=always
RestartSec=10
KillSignal=SIGTERM
TimeoutStopSec=90

[Install]
WantedBy=default.target
```

The optional `EnvironmentFile` is where you can put provider variables for the service; make it
readable only by `<your-user>` (`chmod 600`). VOWP itself never reads that file. Enable the unit and
keep it running after logout:

```sh
mkdir -p ~/.config/systemd/user ~/.config/vowp
chmod 700 ~/.config/vowp
systemctl --user daemon-reload
systemctl --user enable --now vowp-<your-project>
systemctl --user status vowp-<your-project>

sudo loginctl enable-linger <your-user>
```

If `systemctl --user` cannot reach the user manager over SSH, set
`XDG_RUNTIME_DIR=/run/user/$(id -u)` in that shell and try again; lingering keeps the manager
available when you are logged out.

`vowp up --foreground` is the process systemd supervises; it does not detach. Use the journal for
startup errors:

```sh
journalctl --user -u vowp-<your-project> -f
```

When systemd owns the process, stop or restart it with `systemctl --user stop|restart
vowp-<your-project>`. Do not start a second `vowp up` for the same node home.

## Status, logs and attach

From the project checkout as `<your-user>`:

```sh
vowp status
vowp status --check
vowp board --once
vowp attach
vowp attach <job-id>
```

`status` shows the supervisor, heartbeat, jobs by state, slots, limits, cost and open inbox items.
`status --check` exits 1 when the heartbeat is stale, which is the watchdog contract. `board` is the
refreshing view; `--once` prints one snapshot.

`attach` needs a terminal, so use SSH's TTY allocation from another machine:

```sh
ssh -t <your-user>@<your-vps> 'cd /home/<your-user>/src/<your-project> && vowp attach'
```

If tmux is unavailable, `vowp attach <job-id>` tails the job's latest session log. To follow that
log yourself, find its session id with `vowp session ls --job <job-id>` and run
`vowp session tail <session-id> --pretty --follow`. The supervisor log is structured text:

```sh
tail -n 100 ~/.vowp/logs/supervisor.log
```

## Keep the tmux socket private

VOWP runs its own tmux server, not the account's default server. The socket name is derived from the
node home, and every internal tmux command runs with `-L <socket>` and a private
`TMUX_TMPDIR=<node-home>/tmux`. At startup VOWP records the socket and server pid in
`<node-home>/tmux-server`, and `vowp down --kill-windows` refuses a server that does not match that
record.

Operate it through VOWP:

- Use `vowp down --kill-windows` to close this project's windows. Do not run `tmux kill-server`.
- Do not run `tmux -L default`; VOWP refuses the literal `default` socket.
- Do not point `TMUX_TMPDIR` at a shared directory such as `/tmp` for VOWP. Let VOWP use its node
  home.
- If you write your own tests or scripts, use a unique `tmux -L <unique>` socket **and** a private
  `TMUX_TMPDIR` created before the first tmux command. See
  [Contributing](../../CONTRIBUTING.md#run-the-test-suite-safely).

This keeps the supervisor away from any interactive tmux session you or another user may have.

## SSH and firewall

VOWP opens no listening socket in v0.1. The only inbound service you need is SSH; commands run
locally or over SSH with `--node`. Keep the firewall closed to everything else.

On a server with `ufw`:

```sh
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH
sudo ufw enable
sudo ufw status
```

If your provider has its own firewall, apply the same rule there: allow SSH from your addresses and
nothing else. Do not expose the VOWP home, the database or the tmux socket over the network.

Use key authentication and batch mode for automation:

```sh
ssh -o BatchMode=yes <your-user>@<your-vps> vowp --version
```

A missing key must fail instead of prompting. Harden SSH in the usual way for your distribution
(disable password authentication once keys work, and keep the host key in `known_hosts`). The account
needs outbound access to your git remote and to the harness's provider; VOWP itself makes no
provider calls.

## Multiple nodes

To drive this server from a laptop, register it as a node on the machine that holds the project:

```sh
vowp node add <your-node> <your-user>@<your-vps>
vowp node ls
vowp --node <your-node> status
```

The remote account needs its own VOWP binary, its own initialised project and its own harness login.
The [multiple nodes guide](multi-node.md) covers registration, `--node`, SSH details and moving a
session between nodes.

## Troubleshooting a first run

| Symptom | Check |
|---|---|
| `vowp: command not found` under systemd | `PATH` and `ExecStart` in the unit; use absolute paths. |
| `doctor` fails `harness:claude-code` / `harness:opencode` | The harness is installed and signed in as `<your-user>`; check `[harnesses.<name>].binary`. |
| `doctor` reports a provider `absent` | Set the variable in the environment that starts the unit, or in its `EnvironmentFile`. |
| No tmux session after `vowp up` | Read `~/.vowp/logs/supervisor.log`; install tmux and check the private socket directory. |
| A job is parked | Read `vowp inbox ls` and `vowp job show <id>`; a provider limit or a question is the usual cause. |
| Permission denied on the node home | Run every command as `<your-user>`; `~/.vowp` must be owned by that account. |

The [troubleshooting guide](../troubleshooting.md) covers the full set of exit codes, gates, provider
limits and tmux failure modes.

Next: [multiple nodes](multi-node.md) for SSH and cross-node sessions, or
[troubleshooting](../troubleshooting.md) when a job does not move.
