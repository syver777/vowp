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
  the target, and both sides record the node transition. In v0.3 the normal multi-device path is one
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

`import --same-id` keeps the bundle's session id instead of creating a child:

```sh
vowp session import planner.vowp-bundle.json --same-id
```

The import is refused with exit 4 when that id already exists locally (a session row or a session
directory). Nothing is overwritten or merged. A same-id import has no parent, writes a fresh
`session.created`, copies the rest of the bundle tail into the log (event ids and timestamps are
preserved; local sequence numbers are contiguous) and attaches the bundle capsule. A duplicate
event id is a conflict, not a warning. A compacted bundle keeps its capsule and an empty tail.

Common rejections:

| Error | Meaning |
|---|---|
| `unknown bundle format` | The `format` field is not `vowp-bundle-v1`. |
| `bundle too new` | `protocol_version` is newer than this binary. |
| `checksum mismatch` | The JSON was edited or truncated. Nothing was created. |

## Pull from another node

```sh
vowp session pull <node> <session>
vowp session pull vps 01KA1V0Z7A0000000000000000
vowp --json session pull vps 01KA1V0Z7A0000000000000000
vowp session pull --same-id vps 01KA1V0Z7A0000000000000000
vowp session pull --full vps 01KA1V0Z7A0000000000000000
vowp --json session pull --full vps 01KA1V0Z7A0000000000000000
```

`pull` exports the bundle from the named node over the existing SSH transport, stages it in a
private temporary directory, verifies the format, protocol version and sha256 checksum locally,
then imports it as a child session. It prints the source id and the new child id; `--json` prints
`{"node","source_session","child_session"}`. The child follows the same rules as `session import`:
a new id, `parent_session_id` set to the source, a title ending in `(imported)`, and a first turn
seeded from the bundle capsule and tail.

`pull --same-id` keeps the source id when it is absent locally, with no parent and the same log and
capsule rules as `session import --same-id`. It prints `pulled session <source> from <node> with
same id <id>`; `--json` prints `{"node","source_session","same_id_session"}`.

`pull --full` copies the whole `sessions/<id>/` directory instead of the bundle: the full event
log, every blob, the capsule files and `adapters.json`. It always keeps the source id (there is no
child and `--full` cannot be combined with `--same-id`). The files arrive as a
`vowp-session-dir-v1` manifest plus raw bytes, are written into a private staging directory and
checked against the manifest's byte sizes and sha256 digests before the directory is renamed into
`sessions/<id>/`. The resulting directory contains `manifest.json` for inspection. A failed copy,
digest mismatch, corrupt log or over-cap directory leaves no final `sessions/<id>/` and no staging
directory. `vowp reindex` then registers the session.

The full-pull limit is 1 GiB (1 073 741 824 bytes), checked from the remote file sizes before
anything is copied. There is no partial copy and no placeholder for missing blobs: an over-cap or
unreadable session is refused. A compacted source has no log to copy and is refused; use the bundle
pull for it.

If `--same-id` or `--full` finds the source id already present locally, the pull does not overwrite,
silently refuse or merge. It prints both heads and exits 4 (conflict):

```text
vowp: session 01KA… exists locally and on vps; refusing to overwrite or merge
local:  01KA… on laptop: seq 12, last user.message at 2026-11-12T09:14:02.118Z, state idle, executor opencode:…
remote: 01KA… on vps: seq 15, last assistant.message at 2026-11-12T09:20:41.006Z, state idle, executor claude-code:…
choose which side advances; no merge performed
hint: delete or export one side, then pull again
```

`--json` writes one document:
`{"error":{"kind":"conflict","code":"session_divergence","message","hint","node",
"source_session","local","remote","note"}}`. Each head carries `session`, `node`, `last_seq`,
`last_event`, `last_event_at`, `state`, `executor` and `updated_at`; unavailable fields are `null`.
The local head comes from the index row and `log.jsonl`. The remote head comes from the bundle tail;
for `--full` it comes from the source's read-only `session show --json --events 1` because the
directory manifest carries no log position. Neither form prints a token, cwd or home path.

The choice is manual and the pull itself writes nothing locally: delete or export one side, then
pull again. There is no force flag and no merge. Both pull modes are one-way snapshots and no live
lease is migrated. A bundle export writes its usual export capsule on the source; the directory
export and head read are read-only and take no lock. For `--full`, leave the source lease released
or quiescent while it runs and run the same build on both nodes. A failed ssh/remote read,
truncated stream or checksum mismatch leaves the local home unchanged. Push, bidirectional
sync/merge, live lease migration and an offline retry queue remain out of scope. For a local file
copy use `session export` and `session import`.

## Watch an interactive Claude Code session

```sh
vowp session watch --once
vowp session watch --follow
vowp session watch --once --claude-dir ~/.claude
```

`session watch` is an optional importer. It scans `*.jsonl` files under `--claude-dir`
(default `$CLAUDE_CONFIG_DIR`, else `~/.claude`) and records user text, assistant text and tool
calls; other line types are skipped. It reads transcripts read-only and never writes to Claude's
files. The importer stores a byte cursor (offset plus the last complete line's id) in the node
database under `watch:<native session id>`, so a later pass imports only newly completed lines. A
partial final line waits for its newline, and a transcript that shrank below the cursor restarts at
zero. One Claude native uuid maps to one canonical VOWP session (`role=user`, title from the first
user message); later passes append only new events and never create a second session.

`--once` makes one incremental pass. `--follow` polls every two seconds until Ctrl-C, importing
newly completed lines; a terminal run without `--once` or `--follow` follows the same way. A non-TTY
run without `--follow` makes one pass even when `--once` is omitted, so scripts do not hang. Run it
from cron with `--once` for repeated scans.

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
