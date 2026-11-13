# Home and surfaces

VOWP is **VPS-first**: one **home** on a Linux VPS owns the truth, and many **surfaces** talk to it.
The home runs one supervisor for every active project. People reach it from the apps they already use,
from a terminal fallback, or from a phone over SSH. Every session has one log on the home; only the
surface holding the lease can write to it.

> **Status: current.** The commands and behaviour on this page are implemented on `main`. The binding
> home and surface specification is
> [`docs/design/v0.2/home-and-surfaces.md`](../design/v0.2/home-and-surfaces.md); v0.3 adds the
> loopback API, the window and session pull.

## The home owns the truth

The home is normally a Linux VPS that stays on. It owns:

- the **global database**, `~/.vowp/vowp.db`, with the project registry, the session index, surfaces and
  leases, provider limits, capacity history, the lifecycle catalog and the node inbox and ledger;
- **one database per project**, `~/.vowp/projects/<name>/project.db`, with that project's jobs, edges,
  runs, gate results, loops, schedules, inbox, ledger and releases;
- every **session log** under `~/.vowp/sessions/<id>/`, including capsules and blobs;
- the **node supervisor**, the capacity sampler, the tmux windows and `snapshot.json`.

A `Store` handle is bound to the global database plus exactly one project database. The supervisor
holds one handle per active project and never attaches two project databases to one connection. Keep
that rule in mind when reading code: a fleet-wide query opens the project databases in turn, not all at
once.

The split has practical consequences:

- archiving or removing a project is one directory operation;
- a corrupt project database cannot stop the rest of the fleet;
- a project has one backup unit;
- the "what is this file and can it be deleted" question has a class-level answer (see
  [data lifecycle](data-lifecycle.md)).

The home is the only writer. A control machine, app, MCP server or remote executor sends a command or
streams events to the home; it never edits a database file or appends a log on its own.

### Projects

A project is a git checkout with a `vowp.toml`, registered in the home. `vowp init` in the checkout
still writes the project configuration and framework files; it also registers the project. The
registry commands are:

| Command | What it does |
|---|---|
| vowp `project add <path> [--name] [--weight]` | Registers an existing checkout. Idempotent for the same path and name; does not start a supervisor. |
| vowp `project ls [--all]` | Lists projects with state, weight, path and update time. The reserved `_node` project is hidden unless `--all`. |
| vowp `project pause <name>` | Stops new dispatch. Running work finishes on the maintenance tick. |
| vowp `project resume <name>` | Returns a paused project to `active`. An archived project is refused. |
| vowp `project archive <name>` | Stops all ticks for the project and keeps its database. |
| vowp `project rm <name> --yes` | Archives the project directory under `~/.vowp/archive/projects/` and removes the registry row. It refuses while the project is running and for `_node`. |
| vowp `project weight <name> <1-100>` | Sets the fairness weight. Higher weights are ticked first; within a weight the project admitted last moves to the back. |

A paused project still gets maintenance work — requests, reconcile, harvest, stalls, state changes,
gates, verify, merge and the board — but no dispatch, triggers or work-creating schedules. An archived
project gets no tick. If a project's step fails three ticks in a row, the node supervisor pauses only
that project with a `broken: <error>` reason and writes a node-inbox notice; every other project keeps
running.

## One supervisor, many projects

`vowp up` starts one `NodeSupervisor` process for the whole home. The lock is still
`~/.vowp/supervisor.lock`; a second `vowp up` exits 4. `vowp up` on a control machine refuses with exit
5 because the control machine has no truth to run.

Each tick does a node pass, then every active project's L1–L9 steps in fairness order:

```text
0. node sample        capacity sample, stale-lease sweep, lifecycle tick
1. global             apply drained node requests
2. projects           each active project in fairness order, each isolated by catch_unwind
3. snapshot           write snapshot.json after the project steps
4. board              write board.json for one active project, or the only paused project
5. heartbeat          update ~/.vowp/heartbeat and the supervisor pid
```

The order comes from the fairness weight and the last admission. Higher-weight projects go first;
within one weight the project admitted last moves to the back of its group, so a project that just got
work does not stay first. A group with no recorded last admission starts from a per-tick rotating offset,
and an unreadable registry falls back to name order with that offset. Every active project gets one L1
tick per node tick. Within a project, job priority decides dispatch order.

`vowp up --once` runs startup plus exactly one node tick, which is useful for a cron check and for
tests. `vowp down` writes the stop file and waits; `--force` signals the supervisor and
`--kill-windows` also removes the tmux windows. A systemd user unit with linger keeps the home alive
across reboots.

### tmux layout

The windows are **views**, never control channels. VOWP tags its windows and only kills tagged
windows; it never sends keystrokes, never respawns a pane and never reads screen text for a decision.

| Session | Windows |
|---|---|
| `vowp-home` | `board` (every project), `capacity` |
| `vowp-<project>` | `board`, `supervisor`, `planner`, `keeper`, `inbox`, `job-<id>` |

When tmux is absent, `vowp up --no-tmux` runs without windows. `vowp attach` attaches to the
project's board window; `vowp attach <job-id>` attaches to that job's window, or tails the job's
latest session log when tmux is unavailable.

## The control machine

A control machine is a laptop, desktop or phone that talks to the home. It runs no supervisor, no
loops, no graph and no worktrees. vowp `control init <ssh-target>` creates `~/.vowp-control/` with
`control.json` (mode 0600), `nodes.json`, `surfaces.json`, `tunnels.json` and the `spool/` directory.
The snapshot cache (`snapshot.json` and `snapshot.meta.json`) is filled later by vowp `fleet status` or
vowp `setup control`.

Routing is deterministic:

1. `--home <dir>` means "this is the home".
2. `VOWP_CONTROL_HOME`, or an existing `~/.vowp-control/control.json`, means "this is a control
   machine".
3. `VOWP_HOME`, or an existing `~/.vowp/vowp.db`, means "this is the home".
4. Otherwise the machine is uninitialised and project/fleet commands fail with a hint to run setup.

On a control machine:

- reads such as vowp `fleet status`, vowp `fleet queue`, vowp `fleet inbox`, vowp `capacity show`
  and vowp `project ls` are sent to the home over `ssh -o BatchMode=yes`; the remote exit code is
  returned unchanged (an SSH failure is exit 7);
- writes such as vowp `project add`, vowp `inbox answer`, vowp `capacity set`, vowp `job add` and
  vowp `job park` always go to the home; a write never succeeds against the cache;
- vowp `fleet status` caches the home's `snapshot.json` and prints `as of HH:MM` when the home is
  unreachable; `--refresh` requires SSH and `--cached` never opens a connection;
- `vowp up`, `vowp down` and `vowp board` refuse because they need local truth.

A path given to vowp `project add` is a path **on the home**, not on the control machine.

### Tunnels and the phone

vowp `tunnel open <name> --remote-port <n> [--local-port <n>]` starts a managed
`ssh -L 127.0.0.1:<local>:127.0.0.1:<remote>` forward, records its pid and ports in `tunnels.json`
(0600), and prints the chosen local port. vowp `tunnel ls` prunes dead records and vowp `tunnel close <name>`
stops one. A tunnel is opened from the machine that will use it; the home refuses `tunnel` because a
forward there would point at itself.

The phone needs an SSH app and the owner's key. It runs `vowp chat` on the home over SSH. There is no
VOWP app to install, no browser board and no public port.

## The read model

Every tick the supervisor writes `~/.vowp/snapshot.json` to a temporary file, fsyncs it and renames it
into place, so a reader never sees a partial document. The schema is `vowp-snapshot/1`, and
`vowp schema snapshot` prints its JSON Schema. The snapshot is derived: deleting it loses nothing, and
vowp `fleet status` recomputes from the databases when the file is missing or older than three ticks.

The snapshot carries:

- the supervisor state (running, pid, last tick, fail streak, version);
- every project with state, weight, jobs by state, running jobs, blocked jobs, queue with waiting
  reasons, last merge, recent gate failures and cost today;
- total capacity: target and advice, brake state, reserves and the running run count. The
  snapshot's `measured` host sample is `null` and `running` carries zero RAM/CPU in this build;
  vowp `capacity show` is the measured report;
- provider limits, open inbox items, recent sessions (the session card's `active_surface` field is
  reserved and always `null` in this build), and orchestrator cards.

Read commands:

| Command | What it shows |
|---|---|
| vowp `fleet status [--refresh] [--cached]` | The node summary and snapshot wrapper. On a control machine it can fall back to the cache. |
| vowp `fleet queue [--project] [--state]` | Every queued job with its waiting reason. |
| vowp `fleet inbox [--project] [--all]` | Node and project inbox items, oldest first. |
| vowp `fleet capacity` | The snapshot capacity card: target, advised target, run count, brake and `measured` (currently none). Use vowp `capacity show` for the full measured report. |
| vowp `status` | The compact v0.1-compatible status plus the home and project rows. |
| vowp `board [--once]` | The refreshing terminal board. With one project the output stays byte-compatible with v0.1. |

The snapshot never contains a secret value, a lease token or a full event payload. Titles, job ids and
inbox item titles are included; inbox bodies are omitted (the snapshot's inbox rows have no body field).
The registered checkout path is included, so treat the snapshot like the rest of the home state.

## Sessions, surfaces and leases

The v0.1 session model is unchanged: one append-only log, one sequence, capsules, switch and
export/import. v0.2 adds a transport, a lease and app-host integration. The rule is:

> **Any surface can continue the same session; only the lease holder writes.**

Surfaces include `vowp chat`, the opencode app, a Claude app and the `mcp` server those apps call. A
surface registers once and heartbeats its lease every 5 seconds. If the
heartbeat is older than 60 seconds the lease is stale and another surface may take it. A clean exit
releases the lease.

When another surface holds a live lease, opening the session asks before taking over. A takeover:

1. atomically replaces the lease row;
2. pushes a `LeaseRevoked` notice on the old surface's ingest stream, so it stops writing immediately;
3. drains a running turn for up to five seconds, then interrupts it;
4. writes a capsule and, when the session already has an executor, records a `switch` with reason
   `takeover`;
5. appends the takeover `note` after the switch releases the log lock;
6. leaves the old surface in **follow mode**, reading new events but refusing every write. A pre-attach
   session gets the note only.

The old surface prints the notice and keeps showing new events:

```text
Taken over by phone at 14:05. This surface is now read-only (follow mode).
```

A pre-attach session with no executor gets the note only; there is no executor to switch to.

### `vowp chat`

`vowp chat <session>` is the guaranteed fallback and the reference implementation of the lease rules.
It works in a narrow terminal with no colour and supports:

| Command | Action |
|---|---|
| `/switch <executor> [--node n]` | Switches the executor, with the normal capsule handoff. |
| `/status` | Shows the session, executor, lease and run state. |
| `/takeover` | Takes the lease from the current surface. |
| `/follow` | Releases the lease and follows read-only. |
| `/capsule` | Writes a handoff capsule now. |
| `/quit` | Leaves, releasing the lease. |

`vowp chat --follow` starts read-only. `--takeover` takes the lease before the first turn.
`--executor` and `--node` choose where the first turn runs.

### Remote execution

A turn may run on the surface's own machine instead of the home. The home remains the only writer:

1. the surface asks the home to begin a turn, with the lease token in a request document rather than on
   the command line;
2. the home writes the framing events and a `runs` row, then returns a grant with the run id, prompt,
   executor, node, environment **names**, lease token and tail sequence;
3. the client runs vowp `__exec --grant <file>` on its own machine; the grant file is mode 0600 and the
   token is never in argv or `ps`;
4. the adapter writes every event to a local spool **before** sending it, then streams JSON lines to
   vowp `session ingest` over SSH;
5. ingest assigns the sequence, appends with the supplied event id, indexes it, heartbeats the lease and
   acknowledges the event. A duplicate id is acknowledged, not appended; the same id with different
   content is a conflict;
6. the client keeps a bounded in-flight window and replays from the first unacked event after an SSH
   drop. The spool survives a killed client, not just a killed SSH connection.

The executor node owns its harness binary and any secret it injects; the home never sends a secret
value over the wire, only environment-variable names. The run row and snapshot name the executor node.
A laptop that dials the home is registered once with vowp `node add <name> -`; the home never dials it.

## App surfaces and orchestrators

The primary surfaces are the apps people already use. VOWP hosts the session and gives the app a
project-local configuration; it does not replace the app.

### opencode

An orchestrator can run inside an opencode server on the home:

- the host uses a private `XDG_STATE_HOME`/`XDG_DATA_HOME` under the orchestrator directory, never the
  owner's global opencode state;
- it binds `127.0.0.1` only;
- `opencode pair` produces a one-time link that expires after five minutes; treat it as a secret;
- the desktop joins through `ssh -L` and the pair link; the phone runs the opencode terminal against the
  same server over SSH.

No public port is opened and no owner hostname is stored on the home. The join instructions keep
generic placeholders such as `<home-ssh-target>` and `<port>`; run them on the control machine, whose
vowp `tunnel` command already knows the configured home target.

### Claude Code

Claude Remote Control starts only when a claude.ai login already exists on the home. VOWP runs
`claude auth status` and reads only its `loggedIn`/`authMethod` metadata; it never opens login or
credential files. The project-local `.mcp.json` needs a one-time owner approval; until then
`claude mcp list` reports `Pending approval`. Without a login, `orchestrator new --app claude-code`
records a degraded orchestrator and exits non-zero instead of pretending the host is running.

### Orchestrator sessions

An orchestrator is an ordinary session with role `orchestrator`, either fleet-wide or bound to one
project. vowp `orchestrator new [--name] [--project] --app opencode|claude-code|chat [--executor]`:

1. creates `~/.vowp/orchestrators/<name>/` — never the user's global app config;
2. creates the session locally with the app's configuration and a short instructions template
   (`AGENTS.md`);
3. writes `orchestrator.json` (0600) with the app, session id, project, executor and state;
4. writes `opencode.json` or `.mcp.json` registering the `mcp` server with the lease token file;
5. starts the app host where the app supports one, acquires the lease and prints join instructions for
   the desktop, the phone and `vowp chat`.

vowp `orchestrator ls` lists cards; vowp `orchestrator stop` stops the host and releases the lease (the session
stays); vowp `orchestrator resume` starts it again. The node supervisor resumes orchestrators by itself
after a home restart: startup starts every orchestrator whose state file says `running`. A live host
whose token still owns the lease is reused; a live host that lost its lease (another surface may hold
it) is left alone and logged. A host that cannot start is recorded as `degraded`, its error is logged,
and the remaining orchestrators still resume, so the home stays up. The session and state file remain,
and vowp `orchestrator resume <name>` retries a degraded one once the host is available. A failed
vowp `orchestrator new` also records `degraded`, releases the lease and returns the host error (a
non-zero exit). `--app chat` creates no background host: its join command starts the chat surface
directly.

The instructions template tells the orchestrator to read the snapshot, report plainly with times,
propose before acting, apply only after the owner's yes, never open a database or read a secret, and
never claim a tool or join is working without checking.

### Transcript mirror and resume

A running app host's transcript is mirrored into the canonical session log at three moments: when the
orchestrator is created (vowp `orchestrator new`), when it is resumed (vowp `orchestrator resume`) and
when the supervisor resumes running orchestrators at startup (`Orchestrator::resume_all`). Each pass
reads the app's new entries since the cursor, appends each one at most once (the native-id map skips
duplicates) and writes `~/.vowp/orchestrators/<name>/mirror.json` with the last mirrored app id and the
update time. A transcript read or cursor write failure is logged and does not stop the host; the cursor
is still written so the next pass has a starting point. This is not a continuous watcher: between those
calls the app owns its transcript and the next create/resume/startup pass catches up.

`resume_context`, which builds the capsule and last turns for a new app, is implemented and tested in
the core but has no production caller yet; the MCP `session_resume` tool reads the session log and
capsule directly.

## MCP tools

vowp `mcp --session <id> --surface <id> --token-file <path>` is a stdio MCP server with plain threads
and no async runtime. It gives an app typed tools instead of shell parsing:

| Read | Write |
|---|---|
| `fleet_status`, `fleet_queue`, `inbox_list`, `capacity_report`, `session_tail` | `inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_set`, `tunnel_open`, `tunnel_close`, `secret_request`, `session_handoff`, `session_resume` |

Every write reads the lease token from the file on each call, checks the lease and records the actor as
`session:<id>/surface:<surface-id>`. Read tools work in follow mode; write tools refuse after a
takeover. `tunnel_open` and `tunnel_close` exist only on a control-machine surface; on the home they
refuse, because a tunnel must be opened by the machine that uses it.

**Caveat.** opencode 2.0.28 can load the MCP server and report it `connected` without exposing its
tools to the model. The config and the server are real, but VOWP does not promise that an opencode
orchestrator can call the tools on that build. Claude Code works after the one-time `.mcp.json`
approval; `vowp chat` is the tool-free fallback. Do not claim a tool call that did not happen.

## What a surface may write

| Surface | Read | Write | Lease |
|---|---|---|---|
| `vowp chat` | yes | turn input and slash commands | required |
| opencode app via MCP | yes | MCP write tools only | required; checked on every call |
| Claude app via MCP | yes | MCP write tools only | same |
| fleet, status, capacity and project CLI families | yes | writes go to the home | no session lease |
| vowp `session ingest` | yes | event append for one run | token in the grant |
| `vowp session tail --follow` | yes | no | not required |

A surface cannot close another vendor's window. The cut-off is enforced where the power is: the lease
check in every write path and in the ingest stream.

## Secrets, briefly

VOWP refers to a secret by **name** only. No command takes a value as an argument.

- **Vault-user mode** (when a separate `vowp-vault` user and the one sudoers line exist) stores files
  under `/home/vowp-vault/secrets/<NAME>` mode 0600.
- **Basic mode** (no sudo or no vault user) stores them under `~/.vowp/secrets/<NAME>` mode 0600.
  Setup reports the mode it chose (`basic mode (same user)`) and vowp `secret upgrade` reports the move to
  the vault user; `vowp doctor` and the board do not print the secret mode in this build.

vowp `secret set` reads from a hidden prompt; vowp `secret ls` shows names and the last four characters;
vowp `secret rm` deletes; vowp `secret upgrade` prints the one sudo command that moves basic-mode files to the
vault user; vowp `secret request` starts a one-time page on `127.0.0.1` with a 10-minute expiry and returns
only the name, URL and expiry. After the paste the result reports the last four characters; the value
is never returned. Through a tunnel this gives a desktop owner a safe place to paste a value without
putting it in a chat.

Injection is by name through the harness's `env_pass` and the configured deploy commands. Because VOWP
knows the exact values it injected, it replaces exact occurrences with `[REDACTED:<NAME>]` in events,
capsules, report artifacts, raw run mirrors and the snapshot before writing. Gate evidence and ledger
details are not redacted in this build, so a command gate that prints an injected secret can persist it
there; redaction at those sinks is a follow-up. The secret-scan gate remains a separate backstop.

The honest limit: any agent that can run arbitrary code in a process that holds a secret can print it.
This design keeps values out of files an agent can read, out of transcripts and out of git; it cannot
make a secret unreadable to code that is given it. Secrets are never included in a backup.

## Setup and a first day

The AI-first setup path is vowp `setup plan`, vowp `setup home --apply` and vowp
`setup control --apply`. The plan lists every phase, command and file before anything changes; a human
hand-off stops the run with exit 5 instead of working around it. A typical first day on a new home
looks like:

- vowp `setup plan --target <user@home>`
- vowp `setup home --target <user@home> --apply --yes`
- on the control machine: vowp `control init <user@home>`, vowp `project add /srv/app` and
  vowp `orchestrator new --project app --app opencode`
- vowp `fleet status`

The desktop joins through the `join.desktop` instructions printed by vowp `orchestrator new`; the
phone uses `join.phone`; `join.chat` is the fallback. Setup never asks for a secret value in the chat.

## Limits of v0.3

- **One home.** There is no replication, leader election or multi-home scheduling. A second home is a
  separate installation.
- **No public API or page.** The v0.3 daemon and window bind loopback and are reached through SSH;
  there is no public listener.
- **Apps keep their own limits.** VOWP can configure and host an app; it cannot close another vendor's
  window or force an app build to expose MCP tools.
- **SSH is the transport.** A control machine needs key-based, batch-mode SSH to the home. A phone needs
  an SSH app and the key.
- **A surface is not a window.** tmux windows are views; they never carry input to a session.

Next: [capacity](capacity.md), [data lifecycle](data-lifecycle.md), or
[sessions and switching](sessions-and-switching.md).
