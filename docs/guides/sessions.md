# Sessions and surfaces

A **session** is one conversation with a stable VOWP id. It lives on the home: one append-only
`log.jsonl`, one canonical event sequence and one capsule lineage. A **surface** is where you are
talking to it right now: `vowp chat` in a terminal, the opencode desktop app, a Claude app, a phone
over SSH, or the MCP tools an app calls. One surface holds the session's **lease** at a time; every
other surface can read in follow mode.

> **Status: current.** The session store, adapters, `vowp session` commands, `vowp chat`, leases,
> takeover, the app hosts and the orchestrator are implemented. `session lease` and
> `session takeover` are hidden implementation commands; use `vowp chat` for the user path.

## One conversation, many surfaces

```text
home (the authority)
  sessions/<id>/log.jsonl        one writer, gapless seq
  global DB                      sessions, surfaces, leases, capsules, switches
  supervisor                     jobs and orchestrators
        ▲
        │  SSH / app host / MCP
  laptop chat ── phone chat ── opencode desktop ── Claude app
```

The session stays on the home when you change device, app or model. The harness that runs the next
turn can change; the conversation does not fork. A surface that loses the lease is cut off with a
clear notice and continues read-only, so two devices cannot silently interleave writes.

## List and inspect

```sh
# from the project checkout, or add --project <name> to each command
vowp session ls
vowp session show <id>
vowp session show <id> --events 20
vowp session show <id> --lineage
vowp session show <id> --capsule
vowp session tail <id> --pretty
vowp session tail <id> --follow --from 120
```

On a home with several projects and no selected checkout, these commands need `--project <name>`;
without it the command exits 2 and asks for a project. `session ls` prints
`ID TITLE EXECUTOR NODE STATE LAST UPDATED EVENTS`. `session show` prints metadata
and the executor history. `--events <N>` prints the last N events, `--lineage` includes the switch
lineage, and `--capsule` includes the last capsule. `session tail` prints JSONL by default or one
pretty line per event (`HH:MM:SS #seq type summary`); `--follow` polls the log every 200 ms. This is
what the `job-<id>` tmux window runs, read-only.

Session ids are ULIDs accepted by a unique prefix of at least six characters. An ambiguous prefix
exits 2 and lists the candidates. Jobs need the full slug.

A session that has been compacted by `vowp gc --apply` no longer has a full log. `session tail` and
`session show --events` exit 3 with `session compacted to capsule <id>; use 'session show
--capsule'`. `session show --capsule` and `session export` still work; export writes an empty event
tail and says so.

## Create and chat

```sh
vowp session new --title "Refactor the parser" --executor opencode:<provider>/<model>#max
vowp chat <id>
vowp session chat <id>
```

`session new` creates an interactive session with role `user` and prints its id. `vowp chat <id>` is
the guaranteed terminal surface for any machine: run it on the home, or on a control machine where
the CLI routes it to the home over SSH. On a phone, SSH to the home and run it there.

Useful options:

| Option | Effect |
|---|---|
| `--executor <executor>` | Switch to that executor before the first turn. |
| `--takeover` | Take the lease from a live surface before the first turn. |
| `--follow` | Follow the log read-only; never take the lease. |

In a chat, each line is a user turn. Assistant text streams, and tool calls are shown as
`· tool: <name>`. The slash commands are:

| Command | Effect |
|---|---|
| `/switch <executor>` | Run the switch protocol to another harness or model. |
| `/status` | Show the session, executor, node and lease state. |
| `/takeover` | Take the lease from another live surface in an interactive chat. After `/follow`, reopen with `vowp chat <id> --takeover`. |
| `/follow` | Release the lease and follow read-only. |
| `/capsule` | Write a capsule now and print its id. |
| `/help` or `/?` | Print the slash-command list. |
| `/quit` or `/q` | Leave the chat; the session stays readable. |

With non-TTY stdin, `chat` reads turns until EOF, which makes it scriptable. A provider limit that
cannot be switched automatically exits 6 with the hint to `/switch`.

## Leases and takeover

One surface holds the lease. The rules are deliberately simple:

- Opening an unheld session takes the lease and prints `attached session <id> as <holder>`.
- A lease with no heartbeat for 60 seconds is stale and can be taken. The heartbeat is every 5
  seconds.
- Opening a session that another live surface holds asks:

  ```text
  Active on laptop since 13:02. Take over? [y/N]
  ```

  Answer `y` once, or pass `--takeover` (non-TTY defaults to no and exits 4).
- The old surface is cut off immediately with:

  ```text
  Taken over by phone at 14:05. This surface is now read-only (follow mode).
  ```

  It keeps showing new events, but every write is refused. A turn running on the old surface is
  drained for up to five seconds, then interrupted; the events already written stay in the log and a
  capsule is written. The new surface continues from that capsule.
- `vowp chat --follow` and the `/follow` command release the lease and only read. An interactive
  chat then follows until Ctrl-C and accepts no slash commands; a scripted (non-TTY) chat accepts
  `/quit`, `/status` and `/follow`. To take the lease back, reopen with
  `vowp chat <id> --takeover`.
- A surface that exits normally releases the lease.

VOWP cannot close another vendor's window. The cut-off is enforced where the power is: the VOWP MCP
tools and the chat surface check the lease before every write. Inside one app's own multi-device
session (for example opencode desktop and opencode on the phone) there is one conversation, so
nothing needs cutting off.

## Surfaces: apps, MCP and chat

The product surfaces are the apps people already use:

| Surface | How it joins | What it can write |
|---|---|---|
| `vowp chat` | A terminal on the home, a control machine or a phone over SSH. | Turns, slash commands and capsules through the lease. |
| opencode | An opencode server on the home; the desktop joins through an SSH tunnel and `opencode pair`, the phone runs `opencode --server ...` over SSH. | Its own session; VOWP MCP tools where the app exposes them. |
| Claude app | Claude Code Remote Control on the home, where a claude.ai login exists; it connects outbound to claude.ai and the desktop or phone joins there. | The same session; VOWP MCP tools after the project-local `.mcp.json` approval. |
| `vowp mcp` | A stdio MCP server started by the app. | The write tools, each checked against the current lease. |

`vowp orchestrator new` creates the project-local app config, starts the host and prints the exact
join instructions for the desktop, the phone and `vowp chat`. It never edits your global app config.
The opencode app host binds loopback on the home; the control machine reaches it through
`vowp tunnel open` and no public port is opened. Claude Remote Control opens no local port: it
connects outbound to claude.ai from the home, and the desktop or phone joins the Remote Control
session there. The instructions and the app-host details are in
[Operating a home](operating.md#orchestrators) and the
[v0.2 specification](../design/v0.2/home-and-surfaces.md#75-app-surfaces-opencode-claude-mirror-and-resume).

`vowp mcp` exposes read tools (fleet status, queue, inbox, capacity, session tail) and write tools
(inbox answer, job add/pause/retry, capacity set, tunnel open/close, secret request, session
handoff/resume). Every write checks the lease and records the actor. A moved session's old app gets
the read-only notice instead of a write. The opencode build may load an MCP server and report it
connected without exposing its tools to the model; in that case `vowp chat` is the tool path, and
Claude Code is the other proven MCP path.

## Switch

```sh
vowp session switch <id> --to opencode:<provider>/<model>#max
vowp session switch <id> --to claude-code:<model>@medium --reason user
vowp session switch <id> --to <executor> --node <node-name>
```

The switch protocol freezes the session, probes the target, drains the current turn for up to five
seconds, writes a capsule, detaches the source, attaches the target and records the `switch` event
and `switches` row. The target is seeded with the capsule and the last two turns as plain text;
native tool calls are never replayed.

Useful flags:

- `--reason user|limit|crash|failover|context|probe_back` records why the switch happened. The
  default is `user`.
- `--node <name>` switches to an executor on another node. The session is exported and imported on
  the target, and both sides record the node transition. In v0.2 the normal multi-device path is one
  home with several surfaces; the bundle path is for a copy or archive.
- `--force` allows a switch while a turn is running. Without it, an active turn is a conflict (exit
  4). The drain phase is used by automatic switches (failover and context pressure) and by a forced
  manual switch.

Exit 6 means the target provider is limited. Exit 4 means the session is already switching or a turn
is running without `--force`. On a target probe failure the session stays on the source and remains
readable.

## Capsules

```sh
vowp session capsule <id>
vowp session capsule <id> --print
```

A capsule is written automatically on switch, limit, context pressure, job end, export and takeover.
It is debounced to one per session per 120 seconds, except for the switch, manual, export and limit
triggers, which never debounce. The manual command uses the manual trigger and bypasses the debounce.
`--print` prints the capsule Markdown instead of just the id.

A capsule carries intent, constraints, decisions, state, files, gotchas, open questions and
telemetry. It is validated against the protocol schema and repaired once if invalid. If it still
fails, the previous capsule is kept and an `error` event is appended; the session does not silently
lose its handoff.

## Export and import

```sh
vowp session export <id>
vowp session export <id> --out planner.vowp-bundle.json --tail 50
vowp session import planner.vowp-bundle.json
vowp session import planner.vowp-bundle.json --node <node-name> --cwd /srv/project
```

`export` writes one JSON file (`format: "vowp-bundle-v1"`, default `./<id>.vowp-bundle.json`) with a
fresh capsule, the last 20 replayable events (up to 500 with `--tail`), the switch lineage, small
blobs inlined and a sha256 checksum.

`import` validates the format, protocol version and checksum before changing anything, then creates a
**child** session on the importing node with `parent_session_id` set to the origin. The first turn on
the child is seeded with the bundle capsule and tail. Importing the same bundle twice creates two
independent children; the origin is never modified. A compacted origin exports its capsule with an
empty event tail and says so.

Common rejections:

| Error | Meaning |
|---|---|
| `unknown bundle format` | The `format` field is not `vowp-bundle-v1`. |
| `bundle too new` | `protocol_version` is newer than this binary. |
| `checksum mismatch` | The JSON was edited or truncated. Nothing was created. |

## Watch an interactive Claude Code session

```sh
vowp session watch --once
vowp session watch --once --claude-dir ~/.claude
```

`session watch` is an optional importer. `--once` scans `*.jsonl` files under `--claude-dir`
(default `$CLAUDE_CONFIG_DIR`, else `~/.claude`). For a transcript with new lines it creates a
canonical session and records user text, assistant text and tool calls; other line types are skipped.
It reads transcripts read-only and never writes to Claude's files. The processed line count is stored
in the node database under `watch:<native session id>`; a later scan imports only lines after that
count, and a transcript that is shorter or unchanged is skipped rather than restarted. Continuous
following is not available: in a terminal, `session watch` without `--once` exits 2 with a message to
use `--once`. Run it from cron for repeated scans.

## When to switch

| Situation | What happens |
|---|---|
| Claude quota reached | L6 switches to the next ladder rung with a capsule, then probes Claude after the reset. |
| Credits exhausted | Jobs on that provider park; a human tops up and answers the inbox item. No silent switch. |
| Auth expired | Jobs park; log in with the harness's own login, then answer the inbox item. |
| Context overflow | A capsule is written and a fresh native session starts on the same harness. |
| Moving between devices | Open the same session on the other surface and answer the takeover prompt. |
| Trying a different model | `session switch --to ... --reason user`, or `/switch` in chat. |
| The owner wants to drive | `vowp chat <id>` into the same session; the next turn sees the log and capsule. |

What survives a switch: the canonical log, the capsule, the last turns as plain text, and the
lineage. What does not: the harness's native tool-call history and reasoning. That is deliberate;
replaying a native session into a different harness is not reliable, and VOWP never fakes it.

Next: [Using your phone](phone.md) for the SSH surface, or
[Control machines and extra nodes](multi-node.md) for the machine-to-machine side.
