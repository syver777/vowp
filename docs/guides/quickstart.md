# Getting started

This walkthrough starts with a home that is already installed (see [Install](install.md)) and ends
with one project running and an orchestrator session you can reach from the control machine or the
phone. It uses one home for every project; there is no separate "laptop-only" mode.

> **Status: current.** `vowp init`, `vowp doctor`, `vowp project`, `vowp job`, `vowp graph`,
> `vowp plan`, `vowp up`, `vowp down`, `vowp status`, `vowp fleet`, `vowp board`, `vowp attach`,
> `vowp orchestrator` commands and `vowp chat` are implemented. The examples below use placeholders:
> replace them with names from your own home.

| Placeholder | Replace with |
|---|---|
| `<your-repo>` | The path to the git repository on the **home**. |
| `<your-project>` | The project name; it names the project database and tmux session too. |
| `<your-harness>` | `opencode` or `claude-code`, installed and signed in on the home. |
| `<your-model>` | A model identifier that harness accepts. |
| `<your-executor>` | `opencode:<provider>/<model>` or `claude-code:<model>`, optionally with `@effort` or `#variant`. |
| `<home-ssh-target>` | The SSH destination of the home: `user@host` or an SSH config alias. |

## 1. Check the home

On the home:

```sh
vowp doctor
vowp fleet status
```

`doctor` must report no `fail`. `fleet status` shows the home, the projects it knows and whether the
supervisor is running. On a control machine, the same `vowp fleet status` goes over SSH to the home and
caches the answer with an `as of` time.

## 2. Register a project

A project is one git repository registered on the home. For a repository that is not initialised yet:

```sh
cd <your-repo>
git status
git remote -v
vowp init --name <your-project>
```

`init` writes `vowp.toml`, the framework files and `.vowp/`, creates the project database under
`~/.vowp/projects/<your-project>/project.db` and registers the project. The working tree should be
clean and the remote should be one the home can fetch and push. Commit the generated files:

```sh
printf '.vowp/\n' >> .gitignore
git add -A
git commit -m "chore: initialise VOWP"
```

For a repository that already has `vowp.toml`, register it without rewriting files:

```sh
vowp project add <your-repo>
vowp project ls
```

The registry commands work from any directory on the home; a path passed to `project add` is a path
on the home, not on the control machine.

## 3. Point the profiles at real executors

Edit `vowp.toml` on the project branch. At minimum, every role needs a ladder rung your home can
actually run:

```toml
[profiles.planner]
ladder = ["<your-executor>"]

[profiles.keeper]
ladder = ["<your-executor>"]

[profiles.worker]
ladder = ["<your-executor>"]

[profiles.verifier]
ladder = ["<your-executor>"]

[providers.<your-provider>]
max_concurrent = 2
```

`<your-provider>` is `claude` for Claude Code, the text before `/` for opencode, or the mapping name
for a generic CLI. `[harnesses.<name>].env_pass` lists environment-variable **names** only; never put
a value in `vowp.toml`. The full grammar and the generic-CLI form are in
[Running a home on a VPS](vps.md#configure-profiles-and-harnesses).

## 4. See the fleet

Start the one supervisor for the home:

```sh
vowp up
vowp status
vowp fleet status
vowp fleet queue
```

`vowp up` ensures `vowp-home` (the `board` and `capacity` windows) and one `vowp-<project>` session
for every non-archived project (including paused ones), then detaches the supervisor. Run it from the
home, or from a checkout to register that checkout on the way. `vowp up --foreground` is what the systemd unit runs; `--once` runs one tick
for a smoke test.

`vowp up` also starts the loopback web window as a separate `vowp-daemon` child, prints
`web window: http://127.0.0.1:<port>/`, and prints the control-machine command for that actual
port:

```text
web window: http://127.0.0.1:54321/
remote window: vowp tunnel open window --remote-port 54321
  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)
```

Open the first URL on the home. From a laptop or phone, configure the home once as a control
machine (`vowp control init --name home <home-ssh-target>`), run the printed
`vowp tunnel open window --remote-port 54321`, and open the local loopback URL it prints. Pass
`--local-port <n>` when you need a fixed local port. The daemon binds `127.0.0.1` only and the
existing managed SSH forward is the only route; no public port is opened. `vowp down` stops the
window together with the supervisor. The window serves one project: run `up` from that checkout,
or pass `--project <name>`, when the home has several. If the daemon binary is not installed next
to `vowp` or on `PATH`, `vowp up` warns and continues without the window; the supervisor is not
optional.

Reads need no token. The writes on the pages use the `VOWP_API_TOKEN` the supervisor inherited:
set it in the environment that runs `vowp up`, enter it in the page's unlock panel, and the page
keeps it in `sessionStorage` and never in the URL. It unlocks the inbox answer and session switch
writes only.

`vowp status` is the short screen; `vowp fleet status` is the fleet view, and
`vowp fleet queue` explains why a ready job is not running. `vowp board --once` prints the board once.
With tmux:

```sh
vowp attach
vowp attach <job-id>
```

The job window is a read-only follower of the session log; it is not a control channel.

## 5. Add a first job

```sh
vowp job add \
  --id hello-docs \
  --title "Write the hello page" \
  --kind docs \
  --owns "docs/hello.md,README.md" \
  --gates "report,owns_respected,secret_scan,review" \
  --brief - <<'BRIEF'
# Goal

Create `docs/hello.md`, a one-page introduction to this project. Explain what the project does,
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

vowp graph
vowp job show hello-docs --gates --runs
```

A healthy job moves through `pending → ready → running → gating → reviewing → merging → done`. If it
needs a human it parks and opens an inbox item:

```sh
vowp inbox ls
vowp inbox answer <id> "Use the existing title style"
```

The full brief, gate and report rules are in [Writing jobs](jobs.md).

## 6. Create the orchestrator

The orchestrator is a long-lived session that reads the fleet snapshot, reports plainly and asks
before it changes anything. Create one on the home:

```sh
vowp orchestrator new --name main --app chat --executor <your-executor>
vowp orchestrator ls
```

For an app surface, use `--app opencode` or `--app claude-code`; the command prints the desktop,
phone and `vowp chat` join instructions and writes only project-local configuration under
`~/.vowp/orchestrators/<name>/`. It never edits your global app config. Use the printed session id:

```sh
vowp chat <session-id>
```

`vowp chat` takes the session lease and gives you a plain terminal surface. The useful slash
commands are:

| Command | Effect |
|---|---|
| `/status` | Show the session, executor, node and lease state. |
| `/takeover` | Take the lease from another live surface in an interactive chat. After `/follow`, reopen with `vowp chat <id> --takeover`. |
| `/follow` | Drop to read-only follow mode. |
| `/switch <executor>` | Move the next turn to another harness or model. |
| `/capsule` | Write a handoff capsule now. |
| `/quit` | Leave the chat; the session stays readable. |

If the session has no executor and the worker profile has no rung, its first turn needs
`/switch <executor>`.
From a control machine, run the same `vowp chat <session-id>`; the CLI routes the command to the home
over SSH. From a phone, SSH to the home and run it there; see [Using your phone](phone.md).

## 7. Stop cleanly

On the home:

```sh
vowp down
```

`down` writes a stop request and waits up to 30 seconds for the lock. Running runs are interrupted and
re-queued as `stalled`, not failed; the tmux windows are left for inspection unless you pass
`--kill-windows`. The orchestrator session stays on the home and can be resumed later.

Restarting with `vowp up` reconciles from the databases and session logs; nothing needs to be replayed
by hand.

## Where to go next

- [Install](install.md) — the setup commands, prerequisites and the control machine.
- [Sessions and surfaces](sessions.md) — leases, takeover, app surfaces, switches and capsules.
- [Operating a home](operating.md) — capacity, backups, garbage collection and upgrades.
- [Running a home on a VPS](vps.md) — systemd, projects, SSH and firewall.
- [Troubleshooting](../troubleshooting.md) — when a job or a session does not move.
