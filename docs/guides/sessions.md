# Sessions

A session is a conversation with a stable VOWP id. The harness underneath can change: you can start on
Claude Code, hit a limit, switch to another model on a VPS, and come back when the quota resets. The log
and the lineage stay one session.

> **Status: current.** The session store, adapters, `vowp session` commands and the `vowp chat` alias
> are implemented.

## List and inspect

```sh
vowp session ls
vowp session show <id>
vowp session show <id> --events 20
vowp session show <id> --lineage
vowp session show <id> --capsule
vowp session tail <id> --pretty
vowp session tail <id> --follow --from 120
```

`session ls` prints `ID TITLE EXECUTOR NODE STATE LAST UPDATED EVENTS`. `session show` prints metadata
and the executor history. `--events <N>` prints the last N events, `--lineage` includes the switch
lineage, and `--capsule` includes the last capsule. `session tail` prints JSONL by default or one pretty
line per event (`HH:MM:SS #seq type summary`). `--follow` polls the log every 200 ms. This is what the
`job-<id>` tmux window runs, read-only.

Session ids are ULIDs accepted by unique prefix of at least six characters. An ambiguous prefix exits 2
and lists the candidates. Jobs need the full slug.

## Create and chat

```sh
vowp session new --title "Refactor the parser" --executor opencode:provider/model#max
vowp chat <id>
vowp session chat <id>
```

`session new` creates an interactive session with role `user` and prints its id. `vowp chat <id>` is an
alias for `vowp session chat <id>`.

In a chat, each line is a user turn. Assistant text streams, and tool calls are shown as
`· tool: <name>`. The slash commands are:

| Command | Effect |
|---|---|
| `/switch <harness:model[@effort][#variant]> [--node <n>]` | Run the switch protocol to another executor. |
| `/capsule` | Write a capsule now and print its id. |
| `/export <file>` | Export a `vowp-bundle-v1` bundle. |
| `/events [n]` | Show the last n events. |
| `/quit` | Leave the chat; the session stays readable. |

With non-TTY stdin, `chat` reads turns until EOF, which makes it scriptable. A provider limit that cannot
be switched automatically exits 6 with the hint to `/switch`.

## Switch

```sh
vowp session switch <id> --to opencode:provider/model#max
vowp session switch <id> --to claude-code:model@medium --reason user
vowp session switch <id> --to <executor> --node vps
```

The switch protocol freezes the session, probes the target, drains the current turn for up to five
seconds, writes a capsule, detaches the source, attaches the target and records the `switch` event and
`switches` row. The target is seeded with the capsule and the last two turns as plain text; native tool
calls are never replayed.

Useful flags:

- `--reason user|limit|crash|failover|context|probe_back` records why the switch happened. The default
  is `user`.
- `--node <n>` switches to an executor on another node. The session is exported and imported on the
  target, and both sides record the node transition.
- `--force` allows a switch while a turn is running. Without it, an active turn is a conflict (exit 4).
  The drain phase is used by automatic switches (failover and context pressure) and by a forced manual
  switch.

Exit 6 means the target provider is limited. Exit 4 means the session is already switching or a turn is
running without `--force`. On a target probe failure the session stays on the source and remains
readable.

## Capsules

```sh
vowp session capsule <id>
vowp session capsule <id> --print
```

A capsule is written automatically on switch, limit, context pressure, job end and export. It is
debounced to one per session per 120 seconds, except for the switch, manual, export and limit triggers,
which never debounce. The manual command uses the manual trigger and bypasses the debounce. `--print`
prints the capsule Markdown instead of just the id.

A capsule carries intent, constraints, decisions, state, files, gotchas, open questions and telemetry.
It is validated against the protocol schema and repaired once if invalid. If it still fails, the previous
capsule is kept and an `error` event is appended; the session does not silently lose its handoff.

## Export and import

```sh
vowp session export <id>
vowp session export <id> --out planner.vowp-bundle.json --tail 50
vowp session import planner.vowp-bundle.json
vowp session import planner.vowp-bundle.json --node vps --cwd /srv/project
```

`export` writes one JSON file (`format: "vowp-bundle-v1"`, default `./<id>.vowp-bundle.json`) with a
fresh capsule, the last 20 replayable events (up to 500 with `--tail`), the switch lineage, small blobs
inlined and a sha256 checksum.

`import` validates the format, protocol version and checksum before changing anything, then creates a
**child** session on the importing node with `parent_session_id` set to the origin. The first turn on the
child is seeded with the bundle capsule and tail. Importing the same bundle twice creates two independent
children; the origin is never modified.

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

`session watch` is an optional v0.1 importer. `--once` scans `*.jsonl` files under `--claude-dir`
(default `$CLAUDE_CONFIG_DIR`, else `~/.claude`). For a transcript with new lines it creates a canonical
session and records user text, assistant text and tool calls; other line types are skipped. It reads
transcripts read-only and never writes to Claude's files. The processed line count is stored in the node
database under `watch:<native session id>`; a later scan imports only lines after that count, and a
transcript that is shorter or unchanged is skipped rather than restarted. Continuous following is not
available in v0.1: in a terminal, `vowp session watch` without `--once` exits 2 with a message to use
`--once`. Run it from cron (or another non-TTY runner) for repeated scans.

## When to switch

| Situation | What happens |
|---|---|
| Claude quota reached | L6 switches to the next ladder rung with a capsule, then probes Claude after the reset. |
| Credits exhausted | Jobs on that provider park; a human tops up and answers the inbox item. No silent switch. |
| Auth expired | Jobs park; log in with the harness's own login, then answer the inbox item. |
| Context overflow | A capsule is written and a fresh native session starts on the same harness. |
| Moving machines | `export` on the source, `import --node` on the target, or `switch --node`. |
| Trying a different model | `session switch --to … --reason user` at any time. |
| The owner wants to drive | `vowp chat <id>` into the same session; the next turn sees the log and capsule. |

What survives a switch: the canonical log, the capsule, the last turns as plain text, and the lineage.
What does not: the harness's native tool-call history and reasoning. That is deliberate — replaying a
native session into a different harness is not reliable, and VOWP never fakes it.

Next: [multiple nodes](multi-node.md) for the SSH side of switching.
