# Getting started

This walkthrough takes one git repository from "nothing installed" to a supervisor with a first job
running, then stops it cleanly. Every command shown is implemented in 0.1.0.

> **Status: current.** `vowp init`, `vowp doctor`, `vowp job`, `vowp graph`, `vowp plan`, `vowp up`,
> `vowp status`, `vowp board`, `vowp attach`, `vowp inbox` and `vowp down` are implemented. The
> build-from-source steps in [Install](install.md) work today. For a runnable three-job graph with
> fake adapters, see the [hello-vowp example](../../examples/hello-vowp/README.md).

The examples use placeholders:

| Placeholder | Replace with |
|---|---|
| `<your-repo>` | The local path to the git repository VOWP will manage. |
| `<your-project>` | The project name passed to `vowp init`; it also names the tmux session. |
| `<your-harness>` | A harness the profile ladder can use: `claude-code`, `opencode`, or a generic `cli:<mapping>`. |
| `<your-model>` | A model identifier that your harness accepts. |
| `<your-provider>` | The provider key used by limits and cost: `claude` for Claude Code, or the text before `/` for opencode. |

## 1. Install

You need Rust from [`rust-toolchain.toml`](../../rust-toolchain.toml), `git`, `tmux`, and a harness
installed and signed in with its own tooling. The [install guide](install.md#prerequisites) covers
harness setup and the other requirements. Clone this repository and, from its root, run:

```sh
cargo build --workspace
cargo install --path .
vowp --version
```

The last command must print the binary version; the first public release is `vowp 0.1.0`. If it does
not print a version, stop here and read the [install guide](install.md).

## 2. Prepare a repository

VOWP manages an existing git repository. Clone or open the repository you want VOWP to manage; it
needs a remote and a default branch. VOWP creates job worktrees outside the repository and the merge
queue pushes branches, so a repository without a remote can run gates but cannot complete the merge
path.

```sh
cd /path/to/<your-repo>
git status
git remote -v
```

The working tree should be clean, and the current branch should be the one you want VOWP to merge into.

## 3. Initialise the project

```sh
vowp init --name <your-project>
```

On a fresh repository this prints `initialised project <your-project> at /path (11 files written,
0 skipped)`. It writes `vowp.toml`, installs the framework files (`AGENTS.md`, `CLAUDE.md`, four
skills and `opencode.jsonc`), creates `.vowp/`, registers the local node and project, and seeds the
default loops as active. Existing framework files are skipped and listed; an already initialised
project exits 4 unless you pass `--force`.

The generated files are untracked, and `vowp job add` writes briefs while the supervisor writes
checkpoints under `.vowp/` in the main checkout. Ignore that runtime directory and commit the rest;
the merge queue requires a clean main checkout:

```sh
printf '.vowp/\n' >> .gitignore
git add -A
git commit -m "chore: initialise VOWP"
```

Read `vowp.toml` before continuing:

- `[project].check_cmds` is what the merge queue runs on the rebased tree.
- `[profiles.*].ladder` is the model order for each role. Replace the examples with `<your-harness>`
  and `<your-model>` pairs your machine can actually run.
- `[providers.*].max_concurrent` prevents one provider from being hammered; each table key is a
  `<your-provider>` name.

The generated defaults and the full file list are in the
[install guide](install.md#prepare-a-project-repository).

## 4. Check the node

```sh
vowp doctor
```

Fix every `fail` before starting the supervisor:

| Check | What to do |
|---|---|
| `home` | Make `~/.vowp` writable, or pass `--home <dir>` consistently. |
| `db` | Move an unsupported or corrupt database aside; `vowp reindex` rebuilds session indexes, not decisions. |
| `git` | Install git and make sure it is on `PATH`. |
| `tmux` | Install tmux before running jobs. `--no-tmux` skips the supervisor layout, but a dispatched job still needs a window. |
| `config` | Fix `vowp.toml`; the error names the key path. |
| `schedules` | Fix the named `[[schedules]]` entry; its cron, interval or timezone did not parse. |
| `harness:claude-code` / `harness:opencode` | Install the harness, or change the profile ladder to one that is installed. |
| `providers` | Set the named environment variable in the shell that starts the supervisor. The value is never printed. |

A missing harness is a `fail`, not a warning: a profile that cannot resolve would only fail later.
`providers`, `systemd`, `watchdog` and `supervisor` can warn without blocking a first run.

## 5. Add a first job

The CLI can create a job directly. The body is the brief; the front-matter (`job`, `owns`, `gates`, …)
is generated from the arguments.

```sh
vowp job add \
  --id hello-docs \
  --title "Write the hello page" \
  --kind docs \
  --owns "docs/hello.md,README.md" \
  --gates "report,owns_respected,secret_scan,review" \
  --brief - <<'BRIEF'
# Goal

Create `docs/hello.md`, a one-page introduction to this project. It must explain what the project does,
how to run it, and where the tests live.

# Read first

- `README.md` and the existing `docs/` pages.
- The project's `AGENTS.md`.

# Deliverables

- `docs/hello.md`, linked from `README.md`.
- A report at `.vowp/jobs/hello-docs/report.md` with the standard front-matter.

# Tests required

- `git diff --check` passes.
- Every relative link in the new page resolves to a file in the repository.

# Out of scope

- Any file outside `docs/hello.md` and `README.md`.
- Code changes.

# Done when

The page exists, the links resolve, and the report has `status: done`.
BRIEF
```

The gate list includes `review`, so the job is reviewed independently before it can merge. The worker
still runs the brief's checks even though they are not separate `cmd` gates.

## 6. Inspect the graph

```sh
vowp graph
vowp job show hello-docs --gates --runs
```

`vowp graph` prints one line per job with its layer, state and dependencies. `vowp job show` prints the
job and its edges; `--runs` adds recorded runs with outcomes and costs, and `--gates` adds recorded gate
results with evidence. Both lists are empty until the job has run; the configured gate list lives in
the brief.

If you would rather let an agent propose the graph first:

```sh
vowp plan "Add a hello page that explains the project" --dry-run
```

This starts a planner turn on the planner profile, so it needs a signed-in harness and model.
`--dry-run` prints the proposed YAML and creates no jobs; it still records the planner session and run.
Review the YAML, then run the same command without `--dry-run` to let the planner create jobs through
the CLI.

## 7. Start the supervisor

```sh
vowp up
```

By default this ensures the tmux session `vowp-<your-project>` with the `board`, `supervisor`, `planner`,
`keeper` and `inbox` windows, then detaches the supervisor. Use `vowp up --foreground` to run the loop in
the current terminal, or `vowp up --once` to run exactly one tick (useful for tests and cron).

Check it:

```sh
vowp status
vowp board
```

`vowp status` shows the supervisor pid, heartbeat age, jobs by state, slots, limits, cost today and the
open inbox count. `vowp board` is the refreshing view; `vowp board --once` prints one snapshot. With tmux
installed:

```sh
vowp attach hello-docs
```

This attaches to the job window; `vowp attach` with no job opens the board window. The job window is a
read-only follower of the session log, so it keeps working across a model switch.

## 8. Watch the lifecycle

A healthy first job moves through:

```
pending → ready → running → gating → reviewing → merging → done
```

- If the worker stops before writing a report, L2 continues it up to twice.
- If a gate fails, a fix job runs in the same worktree; the original re-runs its gates.
- If the verifier fails, a fix job gets the concrete findings and the original is re-gated.
- If a merge conflict or repository-wide check fails, a `merge` or `fix` job handles it.
- If the worker needs a human, the job is parked and an inbox item appears.

Check the inbox at any time:

```sh
vowp inbox ls
vowp inbox answer <id> "Use the existing title style"
```

Answering unparks only the asking job. Everything else keeps running.

## 9. Stop cleanly

```sh
vowp down
```

`down` writes a stop request and waits up to 30 seconds for the supervisor lock to disappear. Running
runs are interrupted and re-queued as `stalled`, not failed. The tmux windows are left for inspection;
add `--kill-windows` to close them, or `--force` to send SIGTERM after the wait.

Restarting later with `vowp up` reconciles from the database and session logs; nothing needs to be
replayed by hand.

## Where to go next

- [Install](install.md) — harness setup, `PATH` install, release archives and the per-user script.
- [Writing jobs](jobs.md) — briefs, gates, reports and the verify/fix loop.
- [Operating a node](operating.md) — logs, loops, backups and the watchdog.
- [Sessions](sessions.md) — switch model or harness mid-job.
- [Running on a VPS](vps.md) — a dedicated user, systemd, SSH and firewall.
- [hello-vowp example](../../examples/hello-vowp/README.md) — a three-job graph with fake adapters.
- [Troubleshooting](../troubleshooting.md) — when a job does not move.
