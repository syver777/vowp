# Install

VOWP 0.2 has two roles:

| Role | What it is | What it owns |
|---|---|---|
| **home** | One Linux VPS. | The global database, every project database, all session logs and the one node supervisor. |
| **control machine** | A laptop, desktop or phone that talks to the home. | A snapshot cache, SSH tunnels and the local app configuration. It holds no truth. |

Most people set up the home first, then make the computer they already use the control machine. The
phone needs no install at all: an SSH app and the owner's key are enough (see
[Using your phone](phone.md)).

> **Status: current.** `vowp setup plan`, `setup home`, `setup control`, `vowp init`,
> `vowp project`, `vowp control` and the install script are implemented.

## The recommended path: let an agent run the setup

The setup contract is designed around one copyable prompt. It names a release tag and sends the
agent to [the setup contract](../setup/agent-setup.md), which asks the owner five questions once,
shows the whole plan once and runs it after one approval. The `vowp setup` commands do the work, so
the result does not depend on which harness runs the prompt. If the README you are reading has the
prompt, paste it into any harness; if not, run the same commands yourself.

```sh
vowp setup plan --target <home-ssh-target> \
  --repository <path-on-the-home> \
  --harness <harness> \
  --provider <provider>
```

`--target` is the home's SSH destination (`<user>@<host>` or an SSH config alias), not the `--home`
directory. `--repository`, `--harness`, `--provider` and `--secret` are repeatable and carry names
only; never put a key in an argument. The plan prints every phase, command and file, and changes
nothing. It needs key-only SSH:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 <home-ssh-target> true
```

Then apply the home plan:

```sh
vowp setup home --target <home-ssh-target> --apply --yes \
  --repository <path-on-the-home> \
  --harness <harness> \
  --provider <provider>
```

`--yes` is the single approval for the plan that was just shown. `setup home` is idempotent and stops
cleanly at a hand-off instead of guessing:

| Phase | What it does |
|---|---|
| `preflight` | Checks SSH, OS/architecture, disk, `git`, `tmux` and `curl`; reads no secret. |
| `user` | Creates a non-root `vowp` user when sudo allows it; otherwise records no-sudo mode. |
| `install` | Installs the pinned binary and verifies the version. |
| `home` | Creates the home, the global database and the node registry, then runs `vowp doctor`. |
| `service` | Installs the user unit `vowp.service`, enables linger and starts it. |
| `vault` | Creates the vault user when sudo allows it, otherwise uses basic secret mode. It never stores a value. |
| `projects` | Registers the repositories the owner named. A repository must already have `vowp.toml`; `vowp init` is offered, not forced. |
| `tunnels` | Nothing by default. Tunnels are opened from the control machine when needed. |

A hand-off exits 5 with a `handoff` object: a sudo command the owner runs, a harness login, a secret
value entered with `vowp secret set NAME` or `vowp secret request NAME`, or an owner answer. Relay the
question, wait for the owner, then run the same command again. Never ask for a secret in chat.

When the home phase succeeds, make this computer the control machine:

```sh
vowp setup control --apply
vowp control status
vowp fleet status
```

`setup control` reuses the target from the home run (or takes `--target`) and writes
`~/.vowp-control/`; it installs no supervisor. `fleet status` fetches the home's snapshot and caches
it with an `as of` time, so the answer is still useful when SSH is down.

## Prerequisites

For a home:

- Linux on `x86_64` or `arm64`.
- `git`, `tmux`, `curl` and a C toolchain for the bundled SQLite build. Debian/Ubuntu:
  `sudo apt-get install -y build-essential curl git pkg-config tmux`; Fedora/RHEL:
  `sudo dnf install -y curl gcc git make pkgconf-pkg-config tmux`.
- A non-root account with key-only SSH. The setup can create a dedicated `vowp` user when sudo is
  available; without sudo it runs as the SSH user and records basic secret mode.
- At least one harness installed and signed in **on the home**, with its own login flow. VOWP drives
  the harness's documented headless interface and never reads its login.
- One or more git repositories the home can fetch and push. The merge queue needs a remote.

For a control machine:

- Linux or macOS, `ssh`, and key-only access to the home.
- The same `vowp` binary for the control commands. It can be installed per user; no service is needed.
- Optional: the opencode desktop app or a Claude app for the app surfaces. `vowp chat` works in any
  terminal and needs nothing extra.

Check the basics on a machine you build on:

```sh
git --version
tmux -V
cargo --version
rustc --version
ssh -V
```

## Build and install by hand

From the repository root, with Rust from [`rust-toolchain.toml`](../../rust-toolchain.toml):

```sh
cargo build --release --workspace
./scripts/install.sh --bin target/release/vowp
vowp --version
```

The install script puts the binary in `~/.local/bin` and, unless told otherwise, installs the user
unit and the five-minute watchdog. Preview it first if you like:

```sh
./scripts/install.sh --dry-run
./scripts/install.sh --no-service --no-watchdog   # control machine: no enabled service, no watchdog
```

Use `--linger` to keep the user service available after logout. On a control machine,
`--no-service --no-watchdog` is the normal choice; the home runs the supervisor, not the laptop.
`--no-service` leaves the unit installed but does not enable or start it; `--no-watchdog` skips the
watchdog script and its crontab entry (it implies `--no-cron`). The unit is `vowp.service`, one
service for the whole home, and its `ExecStart` is `vowp up --foreground`.

If you build with `cargo install` instead, the binary lands in `$CARGO_HOME/bin` (usually
`~/.cargo/bin`); add that directory to `PATH` or pass `--root <dir>`.

## Create the home without the setup command

The setup command is only a wrapper around the same local commands. On the home, as the runtime user:

```sh
cd /path/to/<your-first-repo>
vowp init --name <your-project>
vowp doctor
```

`vowp init` writes `vowp.toml`, the framework files, `.vowp/jobs` and `.vowp/checkpoints`, creates the
home if it does not exist, registers the project and its database, and seeds the default loops. Commit
the generated files before the first run. For each further repository that already has a `vowp.toml`:

```sh
vowp project add /path/to/<your-project>
vowp project ls
```

The install script does not run `vowp init` for you. Run it once per checkout, or let `setup home
--repository <path>` register a checkout that is already initialised.

## Where things go

| Path | Contents |
|---|---|
| `~/.vowp/vowp.db` | The global database: nodes, the project registry, sessions, surfaces, leases, capacity, limits and the catalog. The fleet snapshot is a separate file. |
| `~/.vowp/projects/<name>/project.db` | One database per project: jobs, edges, runs, gates, workers, loops, schedules, inbox, ledger and releases. |
| `~/.vowp/sessions/<id>/log.jsonl` | Authoritative conversation content. Append-only; never edited by hand. |
| `~/.vowp/snapshot.json` | Last fleet snapshot, written every tick. |
| `~/.vowp/orchestrators/<name>/` | Project-local app config, instructions, lease token and host state for an orchestrator. |
| `~/.vowp/secrets/` | Basic-mode secret files (mode 0600); vault mode keeps them under the vault user. |
| `~/.vowp/backups/` | Backup directories and the pre-migration backup. Secrets are never included. |
| `~/.vowp-control/` | Control-machine config, snapshot cache, spool, surface state and tunnel records. |
| `<repo>/vowp.toml` | Project configuration, committed. |
| `<repo>/.vowp/` | Job briefs and reports, checkpoints and runtime files. Never the database. |

Treat `~/.vowp` and `~/.vowp-control` as sensitive: they can contain prompts, tool output and file
contents. The [security model](../security.md) describes the limits, and
[Operating a home](operating.md) covers backup and lifecycle.

## Verify the home

```sh
vowp doctor
vowp fleet status
vowp setup verify
```

`doctor` exits 0 only when no check failed. On a control machine, `vowp fleet status` proves that the
SSH route and the snapshot cache work; `vowp control status` shows the cached `as of` time. If a
harness or provider check fails, fix it on the home, not on the laptop.

`setup verify` is the end-to-end fixture check: a gc dry run, a fixture project through one fake
harness turn and the real merge queue, a fresh snapshot, and one orchestrator chat turn. It creates
the fixture under the home's `setup-verify/` area and removes it on every exit path; it does not touch
your repositories. On a control machine it runs the home checks over SSH and refreshes the local
snapshot cache. It prints `ok`, `warn` or `fail` per check and exits 1 when a check fails.

Next: [Getting started](quickstart.md) for the first project and the first conversation, or
[Running a home on a VPS](vps.md) for the server details.
