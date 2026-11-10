# Install

VOWP is a single Rust binary. The simplest install is the prebuilt release for your machine; the
same binary can also be installed with `cargo install vowp-cli` or the npm wrapper. The repository also
ships a per-user [install script](../../scripts/README.md) that installs the binary, the systemd user
unit and the cron watchdog. For a server deployment, see [Running VOWP on a VPS](vps.md).

> **Status: current.** The prebuilt installer, the build commands, `vowp init`, `vowp doctor` and the
> install script work today; every command referenced on this page is implemented.

## One-line install (macOS and Linux)

This downloads the release archive for your OS/architecture from the latest GitHub release, verifies
its SHA256, and installs `vowp` into `~/.local/bin`. It does not need Rust, Node or root:

```sh
curl -fsSL https://raw.githubusercontent.com/syver777/vowp/main/scripts/install.sh | sh -s -- --download
```

If the installer reports that `~/.local/bin` is not on your `PATH`, add it in your shell profile.
Then check:

```sh
vowp --version
```

The [`scripts/install.sh`](../../scripts/install.sh) script also supports `--version`, `--prefix`,
`--dry-run`, and a source checkout can install the systemd unit and watchdog as well.

### Alternative installs

Use one of these instead of the curl installer; they all install the same `vowp` binary.

```sh
# From crates.io (needs Rust)
cargo install vowp-cli --version 0.1.0

# From npm (needs Node; npm 12 blocks install scripts by default)
npm install -g @syver_august/vowp --allow-scripts=@syver_august/vowp
```

The [GitHub release](https://github.com/syver777/vowp/releases/tag/v0.1.0) also carries the four
prebuilt archives and their checksums. A Homebrew tap is not published.

## Prerequisites

| Requirement | Why |
|---|---|
| Linux (x86_64/arm64) or macOS (arm64/x86_64) | The v0.1 platforms. |
| Rust from [`rust-toolchain.toml`](../../rust-toolchain.toml) | Only needed for source builds and `cargo install`; the prebuilt installer does not need it. The workspace pins a stable toolchain and edition 2024. |
| `git` | Worktrees, branches, the merge queue and release checks. |
| `tmux` | Job and board windows. `--no-tmux` skips the initial supervisor layout, but a dispatched job still needs a window, so install tmux before running jobs. |
| A harness such as [Claude Code](https://code.claude.com/docs/en/overview) or [opencode](https://opencode.ai/docs/) | The agent that actually does the work. Install and sign in with the harness's own tooling. |

Check the basics:

```sh
git --version
tmux -V
cargo --version
rustc --version
```

VOWP does not need a provider key of its own and never reads a harness login. If a harness needs an
environment variable (for example a provider key), set it in your shell exactly as you already do for
that harness. `vowp doctor` reports only whether the **name** is present; it never prints the value and
never calls the network. See the [security model](../security.md).

## Build from source

From the repository root:

```sh
cargo build --workspace
cargo run -p vowp-cli --bin vowp -- --version
```

The last command prints the binary version:

```
vowp 0.1.0
```

For a release build:

```sh
cargo build --release --workspace
```

## Put `vowp` on your `PATH`

The binary package is `vowp-cli`; the executable is `vowp`. Run this from the repository root:

```sh
cargo install --path .
vowp --version
```

`cargo install` writes under `$CARGO_HOME/bin` (usually `~/.cargo/bin`) by default. If that directory is
not on your `PATH`, add it in your shell profile, or pass `--root <dir>` and add `<dir>/bin` instead.

## Install the systemd unit and watchdog

The repository ships a per-user installer for the binary, the systemd user unit and the five-minute
cron watchdog. It never needs root and supports a dry run:

```sh
./scripts/install.sh --dry-run
./scripts/install.sh
```

Use `--no-service` or `--no-cron` to skip either part, and `--linger` to keep the supervisor running
after logout. The installed paths and the uninstall command are documented in
[`scripts/README.md`](../../scripts/README.md).

## Prepare a project repository

VOWP manages an existing git repository; it does not create one for you. The project must have a
default branch and, for the merge queue, a remote it can fetch and push to. If you only want to try the
supervisor on a local repository, an `origin` remote is still required by the merge path.

The first project command is:

```sh
vowp init --name my-project
```

On a fresh repository it prints `initialised project my-project at /path (11 files written, 0 skipped)`.
It writes `vowp.toml` with `schedules = []`; `[node] name = "local"`, `max_workers = 4`,
`max_verifiers = 2`, `tick_secs = 60`, `cwd_allow = []` and `tz_offset_minutes` set to the machine's
current UTC offset; `[project] remote = "origin"`, `default_branch = "main"`,
`lock_files = ["Cargo.lock"]`, `always_allowed = ["Cargo.lock"]` and `push = true`; the four profiles
(the planner starts with a Claude Code rung then an opencode rung; `keeper`, `worker` and `verifier`
start with the opencode rung; all use `on_quota = "next_then_probe_back"`); `[capsule]
min_interval_secs = 120`, `max_input_chars = 200000` and `tail_turns = 2`; and `[merge] rebase = true`,
`check_cmds = []`, `timeout_secs = 2700` and `secret_scan = true`. The list here is a key subset: the
generated file also contains empty `[providers]`, `[harnesses]`, `[env]`, `[gates]` and `[loops]` tables,
and `[project].check_cmds = []`.

It also installs the embedded framework files from [`framework/`](../../framework/README.md)
(`AGENTS.md`, `CLAUDE.md`, four `.claude/skills/vowp-*/SKILL.md` files, `opencode.jsonc`, and the
brief/report templates), creates `.vowp/jobs`, `.vowp/checkpoints` and `.vowp/templates`, appends
`.vowp/tmp/` to `.gitignore`, creates the node home (`vowp.db`, `sessions/`, `logs/`, `runs/`,
`backups/`, `tmp/`), registers node `local` and the project, and seeds nine **active** loop rows:
`l1-supervisor` 60 s, `l2-worker`, `l3-verify`, `l4-planner` 1800 s, `l5-keeper` 1200 s,
`l6-failover`, `l7-inbox`, `l8-watchdog` 300 s and `l9-schedules` 60 s (0 means event-driven).

On a fresh repository, `--no-framework` writes only `vowp.toml` and `.gitignore` (2 files). Existing
framework files are skipped and listed, with one exception: an existing `CLAUDE.md` keeps its content and
gains `@AGENTS.md` when that line is missing (the file counts as written); if it already contains the
line it is skipped. A second `init` exits 4 with `project already initialised ...; use --force`, and
`--force` overwrites the generated files. `vowp up` also ensures the loop rows on
startup, so an existing project picks up the current defaults.

## Verify the node

```sh
vowp doctor
```

`doctor` checks, with `ok`, `warn` or `fail` per item:

- `home`: the VOWP home is writable;
- `db`: the database opens, passes `integrity_check` and has a supported schema;
- `git` and `tmux`: the versions (`tmux` is a warning when absent);
- `config`: `vowp.toml` parses and validates;
- `schedules`: every `[[schedules]]` cron, interval and timezone parses;
- `harness:claude-code` and `harness:opencode`: adapter probes;
- `profiles`: every profile ladder resolves to an installed harness;
- `providers`: provider environment variable **names** are `present` or `absent` (never values, never a
  network call);
- `systemd` and `watchdog`: the user unit and cron watchdog are installed (warn only);
- `supervisor`: a supervisor is running (warn only);
- `disk`: at least 1 GB of free disk space.

Exit code 0 means no `fail`. A missing harness is a `fail`; a missing tmux is a `warn`.

## Where things go

| Path | Contents |
|---|---|
| `~/.vowp/` | Node state: database, session logs, blobs, runs, logs, releases.log, backups, inbox copies. |
| `<repo>/vowp.toml` | Project configuration, committed. |
| `<repo>/AGENTS.md`, `CLAUDE.md`, `opencode.jsonc` | Framework files installed by `vowp init`, committed. |
| `<repo>/.vowp/` | Job briefs and reports, checkpoints and optional graph snapshot. |
| `<repo>/../<repo>-wt/<job-id>` | Job worktrees on branch `job/<job-id>`. |

Treat `~/.vowp` as sensitive: session logs can contain prompts, tool output and file contents. Back it
up like any other work data, and never commit it.

Next: [quickstart](quickstart.md), or [Running VOWP on a VPS](vps.md) for a server deployment.
