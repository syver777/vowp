# Sessions and switching

A VOWP session is one conversation with a stable id that outlives the harness process running it. Claude
Code, opencode, a generic CLI tool or a future adapter can each take a turn; the canonical log records
what the user, the model and the tools did, and a capsule carries the context across the handoff.

> **Status: current.** The store, adapters, session core, leases, remote execution, app hosts and the
> `vowp session`/`vowp chat` commands are implemented on `main`.

## Why a universal session

Harnesses keep their own session ids and formats. If a Claude limit hits, or you want to move a job from
a laptop to a VPS, the native session is not portable. VOWP therefore keeps its own log and treats native
sessions as attachments:

| Layer | Authority |
|---|---|
| `~/.vowp/sessions/<id>/log.jsonl` | Canonical conversation content. Append-only, one event per line. |
| `~/.vowp/vowp.db` (`sessions`, `session_events`, `session_natives`, `capsules`) | Rebuildable index for session history and the board. |
| `~/.vowp/vowp.db` (`surfaces`, `leases`) | Live surface and lease state; not rebuilt from the logs. |
| `sessions/<id>/adapters.json` | Harness-side session ids per node, append-only. |
| `sessions/<id>/blobs/<sha256>` | Content-addressed tool output, diffs and attachments. |
| `sessions/<id>/capsules/*.json` | Handoff documents. |
| `~/.vowp/orchestrators/<name>/` | Project-local app config, lease token and host state for an orchestrator. |

Deleting `vowp.db` loses no conversation content; `vowp reindex` rebuilds the session index from the
logs. Live surface and lease rows are recreated as surfaces reconnect. Copying a session directory to
another node makes it readable there after a reindex. The origin session is never modified by an import.

## One log, many surfaces

In VOWP 0.3 the same session can be open in the opencode desktop app, a Claude app, a phone over SSH or
`vowp chat`. Only one of them may write at a time. A **surface** registers once and takes a **lease**;
it heartbeats every 5 seconds, and a lease without a heartbeat for 60 seconds is free. A surface without
the current token can read in **follow mode** and cannot write.

When a second surface wants the session, the interactive choice is a takeover:

1. the lease row is replaced atomically;
2. the home pushes a `LeaseRevoked` notice on the old surface's ingest stream, so it stops immediately;
3. a running turn is drained for up to five seconds, then interrupted; events already appended stay;
4. a capsule is written and, when the session already has an executor, a `switch` with reason `takeover`
   is recorded;
5. the takeover `note` is appended after the switch releases the log lock;
6. the old surface prints `Taken over by phone at 14:05. This surface is now read-only (follow mode).`
   and keeps reading new events.

A pre-attach session with no executor gets the note only. `vowp chat --follow` starts read-only;
`--takeover` takes the lease before the first turn. The same rules apply to an app's MCP server: every
write checks the lease and records the actor, and a moved session's old tools refuse while its reads
keep working.

The full surface model, app hosts and join instructions are in
[home and surfaces](home-and-surfaces.md#sessions-surfaces-and-leases).

## The event log

Every line is an `EventEnvelope`:

```json
{
  "id": "01KA1V0Z7A0000000000000000",
  "session_id": "01KA1V0Z7A0000000000000000",
  "seq": 42,
  "ts": "2024-01-01T00:00:00.000Z",
  "type": "tool.call",
  "actor": "assistant",
  "source": {"harness": "opencode", "model": "provider/model", "node": "vps", "native_session_id": "ses_…"},
  "payload": {"name": "shell", "input": {"command": "cargo test"}},
  "raw": {"…": "original harness event, display only"},
  "replayable": true
}
```

Rules that consumers rely on:

- `seq` starts at 1 and increments by exactly 1. A gap or duplicate is rejected.
- Events are immutable; a correction is a new event.
- The log is flushed per append and fsynced at turn end, switch, capsule, session creation and fatal
  errors. A crash loses at most the in-flight turn's tail; a torn final line is truncated on the next
  open.
- Tool output larger than 8192 bytes is replaced by `output_ref` pointing at a sha256 blob, with a short
  preview.
- `raw` keeps the harness's original JSON for display only. It is never replayed.
- `assistant.reasoning` is stored with `replayable: false`; it is never fed into a capsule prompt or a
  seed.

### Event types

| Group | Types |
|---|---|
| Lifecycle | `session.created`, `session.executor` (attach/detach), `turn.started`, `turn.ended` |
| Conversation | `user.message`, `assistant.message`, `assistant.reasoning` |
| Tools and files | `tool.call`, `tool.result`, `file.change`, `plan` |
| Provider and cost | `limit` (`quota`, `credits`, `rate`, `context`, `auth`), `cost` (`receipt`, `price-table`, `unknown`) |
| Handoff | `switch`, `capsule`, `note`, `error` |

A turn is framed as `turn.started` → `user.message` → reasoning/tool/file/plan/assistant messages →
`cost` → optional `limit` or `error` → `turn.ended`. The framing events belong to the session layer;
adapters produce the harness events between them.

## Session states

| State | Meaning |
|---|---|
| `idle` | No run attached; readable and ready for a turn. |
| `running` | A run is attached. |
| `blocked` | The last run ended in a limit, auth or credits outcome. Readable; waiting for a switch or reset. |
| `switching` | The switch protocol is running; new input is rejected. |
| `closed` | The job finished; no further turns. |

The valid transitions are `idle → running → idle|blocked`, any readable state → `switching → idle` (with
the new executor) or back to the previous state on abort, and any state → `closed` when the job ends.
State and executor are updated in the same database transaction as the event index row.

## Executors and profiles

An executor is one concrete way to run a model:

```
harness ":" model [ "@" effort ] [ "#" variant ]
```

| Example | Harness | Model | Effort / variant |
|---|---|---|---|
| `claude-code:model@medium` | Claude Code | `model` | effort `medium` |
| `opencode:provider/model#max` | opencode | `provider/model` | variant `max` |
| `cli:codex` | generic CLI adapter | mapping entry `codex` | — |

`@` is valid only for Claude Code and `#` only for opencode. The provider key used by limits and cost is
`claude` for Claude Code, the text before the first `/` for opencode (`provider`), and the mapping entry
name for a generic CLI. A profile is an ordered ladder of executors; rung 0 is preferred. See
[providers and failover](providers-and-failover.md).

## Adapters and app hosts

| Adapter / host | How it drives the harness | Resume | Limits detected |
|---|---|---|---|
| **claude-code** | `claude -p --output-format stream-json --verbose [--resume <id>] --model … --effort … --permission-mode …` | `--resume <native id>` | usage/session/weekly limit text and reset time, 429/overloaded, auth; repeated unknown errors become "unavailable for 15 min" |
| **opencode** | `opencode run --format json [--session <id>] --model provider/model#variant --auto` | `--session <native id>` | 402/insufficient credit, 429, auth, context overflow |
| **cli** (generic) | Any command with JSON or plain output plus a mapping file | per tool | exit codes and patterns from the mapping file |
| **opencode host** | An opencode server on the home, joined through an SSH tunnel and `opencode pair` | the app's own session | transcript mirrored at host create/resume and supervisor startup; limits are not parsed from the host yet |
| **Claude Remote Control** | Claude Code on the home, driven by the Claude apps where a claude.ai login exists | the app's own session | transcript mirrored at host create/resume and supervisor startup; limits are not parsed from the host yet |

Adapters spawn stock binaries exactly as a user would. They do not read login files, spoof clients or
forward tokens. The harness never sees tmux; the tmux window only follows the event log. The ACP
adapter ships in v0.3.

The transcript mirror runs when the orchestrator is created, resumed, or resumed by the supervisor at
startup: it appends the app's new entries to the VOWP log once and writes the `mirror.json` cursor. It
is not a continuous watcher, so the next create/resume/startup pass catches up. The MCP
`session_resume` tool reads the session log and capsule directly. See
[home and surfaces](home-and-surfaces.md#app-surfaces-and-orchestrators).

## Capsules

A capsule is a structured handoff: intent, constraints, decisions, state (done, in progress, blocked,
next steps), files, gotchas, open questions and telemetry. It is JSON validated against the protocol
schema, stored both as a file and in the global database.

- **When:** an explicit switch, a takeover, a provider limit, context pressure, a manual request, a job
  end, or an export. Debounced to one capsule per session per 120 seconds, except for switch, takeover,
  manual, export and limit triggers. A turn end does not write a capsule by itself.
- **Who writes it:** the cheapest capable executor on the node, by default the last rung of the `keeper`
  profile. The capsule run gets its own throwaway session so it never pollutes the log it summarises.
- **Input:** the previous capsule plus replayable events after its `at_seq`. Tool results are dropped
  first, then long assistant messages are truncated, then the oldest events are dropped, always keeping
  the first user message and the last 20 events.
- **Failure:** one repair attempt. If the second answer is invalid, the previous capsule is kept and an
  `error` event is appended; the switch may still use a previous capsule, marked stale.

Capsules never contain secrets. The prompt requires references by name only.

## Switching

`vowp session switch <id> --to <executor> [--node <n>] [--reason user|limit|…]` moves a session to
another executor. The same protocol runs automatically for failover and context pressure.

| Step | What happens |
|---|---|
| 1. Freeze | Refuse if already switching; reject new input. |
| 2. Plan | Insert a `switches` row (`started`) with source, target, reason and last seq. |
| 3. Probe | Run the target adapter's local `probe()`. A remote target node is probed over SSH; a failed or timed-out probe aborts before the source is touched. |
| 4. Drain | Wait up to 5 seconds for the current turn. On timeout it interrupts and stops the run, then continues the switch. The later waits for the runner to finish and release the log are also bounded; if one of those expires, the switch aborts and restores the source. |
| 5. Capsule | Write a switch capsule (never debounced). Use the previous one if repair fails. |
| 6. Detach | Stop the source and append `session.executor{detach}`. |
| 7. Start | Attach the target; deliver the seed with the next turn or start it idle. |
| 8. Record | Append `switch`, sync, mark the row completed. |
| 9. Unfreeze | Return to `idle`, or `running` if a turn started on the target. |

A CLI switch refuses an active turn without `--force` (exit 4); the drain step is used by automatic
switches (failover and context pressure) and by a forced switch. The target is seeded with the capsule and
the last two complete turns as **plain text**:

```
[VOWP handoff] You are continuing session <id>, started on <executor>.
...capsule markdown...

## Last turns (plain text)
user: …
assistant: …

## Next message
…
```

Native tool calls are never replayed. `can_inject_assistant_history` is false, so the handoff is
deliberate and readable. The seed is appended as a `user.message` with `note: "handoff"`, so the log shows
exactly what the new executor saw.

Failure handling is explicit: a target probe failure leaves the session unchanged; a rejected seed is
retried once with the capsule only (no tail); a supervisor crash mid-switch is reconciled at startup —
completed if the target was attached, aborted otherwise, never both. Two simultaneous switches: the
second gets a conflict and retries on the next tick.

## Remote execution

The executor does not have to be the home. A laptop surface can run the harness against the same
session while the home remains the only writer:

1. the surface asks the home to begin a turn; the lease token is in a request document, not on the
   command line;
2. the home writes the framing events and a `runs` row with the executor node, then returns a grant;
3. the client runs vowp `__exec --grant <file>` on its own machine; the grant file is mode 0600 and the
   token is never in argv or `ps`;
4. the adapter writes every event to a local spool **before** sending it, then streams JSON lines to
   vowp `session ingest` over SSH;
5. ingest assigns the sequence, appends with the supplied event id, indexes it, heartbeats the lease and
   acknowledges the event. A duplicate id is acknowledged, not appended; the same id with different
   content is a conflict;
6. the client keeps a bounded in-flight window and replays from the first unacked event after an SSH
   drop. The spool survives a killed client, not only a killed SSH connection.

The executor node owns its harness binary and any secret it injects; the home sends environment-variable
names, never values. A laptop that dials the home is registered once with vowp `node add <name> -`; the home
never dials it.

## Export and import

Sessions stay on the home in v0.3, and a switch to another executor node no longer copies the session
directory. Export/import remains an explicit file copy for a backup, a test fixture or a deliberate
sibling session:

```sh
vowp session export <id> --out planner.vowp-bundle.json
vowp session import planner.vowp-bundle.json --node vps
```

A bundle is one JSON document (`format: "vowp-bundle-v1"`) containing the session metadata, a fresh
capsule, the last 20 replayable events (configurable up to 500), the switch lineage, small blobs inlined
as base64 (≤ 64 KiB each) and a sha256 checksum. Import validates the format, protocol version and
checksum before changing anything, then creates a **child** session with `parent_session_id` pointing at
the origin. Tail events are not copied into the child log; they are rendered into the first seed. The
origin session is never modified, and importing the same bundle twice creates two independent children.

## Windows

`vowp up` ensures one `vowp-home` session for the node and one `vowp-<project>` session per active
project. All windows are views:

| Session | Window | Contents |
|---|---|---|
| `vowp-home` | `board` | The node board with every project. |
| `vowp-home` | `capacity` | The capacity report, redrawn on a TTY. |
| `vowp-<project>` | `board` | The project dashboard. |
| `vowp-<project>` | `supervisor` | `~/.vowp/logs/supervisor.log`. |
| `vowp-<project>` | `planner`, `keeper` | The planner/keeper session tails. |
| `vowp-<project>` | `job-<id>` | A follower of the job's latest session (`vowp __window job <id>`). |
| `vowp-<project>` | `inbox` | Open inbox items. |

The windows are **views, never control channels**. VOWP tags its windows and only ever kills tagged
windows. It never sends keystrokes to a harness TUI, never respawns a pane and never reads screen text
for decisions — logs are authoritative. `vowp attach [job]` attaches to the tmux session; without tmux,
`vowp attach <job-id>` tails the job's latest session log, and `vowp session ls --job <job-id>` prints
the session id for `vowp session tail <session-id> --pretty --follow`.

## Watching an interactive Claude Code session

The optional `vowp session watch` importer records interactive Claude Code transcripts into canonical
sessions without running the harness. `vowp session watch --once` scans the transcript JSONL under
`$CLAUDE_CONFIG_DIR` or `~/.claude` with read-only opens, maps user/assistant text and tool calls to
events, and never writes to Claude's files. It stores the processed line count in the node database
under `watch:<native session id>`; a later scan imports only the new lines and skips a transcript that
is shorter or unchanged. Continuous following is not available: without `--once`, a terminal run exits
2 with a message to use `--once`.

Next: [providers and failover](providers-and-failover.md), [home and surfaces](home-and-surfaces.md),
or the [sessions guide](../guides/sessions.md).
