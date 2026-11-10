# Sessions and switching

A VOWP session is one conversation with a stable id that outlives the harness process running it. Claude
Code, opencode, a generic CLI tool or a future adapter can each take a turn; the canonical log records
what the user, the model and the tools did, and a capsule carries the context across the handoff.

> **Status: current.** The store, adapters, session core and the `vowp session`/`vowp chat` commands
> are implemented.

## Why a universal session

Harnesses keep their own session ids and formats. If a Claude limit hits, or you want to move a job from
a laptop to a VPS, the native session is not portable. VOWP therefore keeps its own log and treats native
sessions as attachments:

| Layer | Authority |
|---|---|
| `~/.vowp/sessions/<id>/log.jsonl` | Canonical conversation content. Append-only, one event per line. |
| `~/.vowp/vowp.db` (`sessions`, `session_events`, `session_natives`, `capsules`) | Rebuildable index for queries and the board. |
| `sessions/<id>/adapters.json` | Harness-side session ids per node, append-only. |
| `sessions/<id>/blobs/<sha256>` | Content-addressed tool output, diffs and attachments. |
| `sessions/<id>/capsules/*.json` | Handoff documents. |

Deleting `vowp.db` loses no conversation content; `vowp reindex` rebuilds the index from the logs.
Copying a session directory to another node makes it readable there after a reindex. The origin session
is never modified by an import.

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

## Adapters in v0.1

| Adapter | How it drives the harness | Resume | Limits detected |
|---|---|---|---|
| **claude-code** | `claude -p --output-format stream-json --verbose [--resume <id>] --model … --effort … --permission-mode …` | `--resume <native id>` | usage/session/weekly limit text and reset time, 429/overloaded, auth; repeated unknown errors become "unavailable for 15 min" |
| **opencode** | `opencode run --format json [--session <id>] --model provider/model#variant --auto` | `--session <native id>` | 402/insufficient credit, 429, auth, context overflow |
| **cli** (generic) | Any command with JSON or plain output plus a mapping file | per tool | exit codes and patterns from the mapping file |
| **acp** (v0.2) | Agent Client Protocol | yes | yes |

Adapters spawn stock binaries exactly as a user would. They do not read login files, spoof clients or
forward tokens. The harness never sees tmux; the tmux window only follows the event log.

## Capsules

A capsule is a structured handoff: intent, constraints, decisions, state (done, in progress, blocked,
next steps), files, gotchas, open questions and telemetry. It is JSON validated against the protocol
schema, stored both as a file and in `vowp.db`.

- **When:** an explicit switch, a provider limit, context pressure, a manual request, a job end, or an
  export. Debounced to one capsule per session per 120 seconds, except for switch, manual, export and
  limit triggers. A turn end does not write a capsule by itself.
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
| 3. Probe | Run the target adapter's local `probe()`; failure aborts. v0.1 does not run the remote `vowp --node <n> doctor --json` probe for a switch to another node, so a remote target is attached without proving its harness is available. |
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

Native tool calls are never replayed. `can_inject_assistant_history` is false in v0.1, so the handoff is
deliberate and readable. The seed is appended as a `user.message` with `note: "handoff"`, so the log shows
exactly what the new executor saw.

Failure handling is explicit: a target probe failure leaves the session unchanged; a rejected seed is
retried once with the capsule only (no tail); a supervisor crash mid-switch is reconciled at startup —
completed if the target was attached, aborted otherwise, never both. Two simultaneous switches: the
second gets a conflict and retries on the next tick.

## Export and import

Cross-node movement in v0.1 is a file, not sync:

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

`vowp up` ensures one dedicated tmux session per project, `vowp-<project>`, with windows:

| Window | Contents |
|---|---|
| `board` | Live dashboard. |
| `supervisor` | `~/.vowp/logs/supervisor.log`. |
| `planner`, `keeper` | The planner/keeper session tails. |
| `job-<id>` | A pretty follower of the job's latest session (`vowp __window job <id>`). |
| `inbox` | Open inbox items. |

The windows are **views, never control channels**. VOWP tags its windows and only ever kills tagged
windows. It never sends keystrokes to a harness TUI, never respawns a pane and never reads screen text
for decisions — logs are authoritative. `vowp attach [job]` attaches to the tmux session; without tmux,
`vowp attach <job-id>` tails the job's latest session log, and `vowp session ls --job <job-id>` prints
the session id for `vowp session tail <session-id> --pretty --follow`.

## Watching an interactive Claude Code session

The optional `vowp session watch` importer (included in v0.1) records interactive Claude Code
transcripts into canonical sessions without running the harness. `vowp session watch --once` scans the
transcript JSONL under `$CLAUDE_CONFIG_DIR` or `~/.claude` with read-only opens, maps user/assistant
text and tool calls to events, and never writes to Claude's files. It stores the processed line count
in the node database under `watch:<native session id>`; a later scan imports only the new lines and
skips a transcript that is shorter or unchanged. Continuous following is not available in v0.1:
without `--once`, a terminal run exits 2 with a message to use `--once`.

Next: [providers and failover](providers-and-failover.md), or the [sessions guide](../guides/sessions.md).
