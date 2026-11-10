# v0.2 — ACP adapter

Status: design only. v0.1 ships the `claude-code`, `opencode` and generic `cli` adapters. ACP is a
v0.2 adapter in `plan.md` §4.2 and is out of scope in
`docs/design/crates/vowp-protocol.md` §7.

## Goal

Drive an **Agent Client Protocol (ACP)** agent server as one more stock harness behind the existing
`Adapter` trait. ACP is a JSON-RPC 2.0 protocol over the child process's stdin/stdout; an ACP server
such as `claude-code-acp` exposes a session API and streams updates and tool calls. The adapter should:

- start one ACP server per run, as the other adapters start one harness process;
- run one prompt turn and translate ACP updates into canonical `EventBody` values;
- report the native session id, token/cost usage where the server provides it, and a classified stop
  reason;
- expose capabilities honestly: `can_resume_native`, `live_input`, `emits_reasoning` and
  `emits_cost_receipts` depend on what the server supports.

A permission request must **not** become a keystroke or an automatic yes. In v0.1 the rule is that
VOWP never answers a harness dialog by sending keys; for ACP, a permission request should either be
mapped to an existing inbox approval or be refused with a recorded `error`/`note` until a permission
event type exists.

## Non-goals

- Replacing the Claude Code or opencode adapters.
- A browser permission UI (that belongs to the web window, if it is added at all).
- Reading, storing or forwarding ACP authentication tokens. The server owns its own login.
- A second event log or a second session model.

## Existing crates it builds on

- `vowp-protocol::Adapter`, `AdapterRun`, `AdapterCapabilities`, `StartInput`, `RunSummary`,
  `AdapterError`, `HarnessEvent` and every `EventBody` payload.
- `vowp-adapters::proc` and `stream` — child-process spawning, bounded readers and process-group
  signals.
- `vowp-adapters::classify` — limit/auth/credit/rate/context classification and redaction.
- `vowp-core::session::service::run_turn` — the caller; it already treats an adapter as a stream of
  `HarnessEvent`s and does not care which harness produced them.
- `vowp-protocol::HarnessKind` — currently `claude-code`, `opencode`, `cli`, `vowp`. ACP needs either
  a new `acp` variant or a `cli:<name>` mapping; the first is cleaner but is an additive wire change.

## Mapping sketch

The exact ACP schema version must be pinned before implementation. The intended mapping is:

| ACP signal | Canonical event |
|---|---|
| `session/new` result | `SessionExecutor { attach }` with the native id |
| `session/prompt` start | `turn.started`; the prompt content becomes `user.message` |
| agent message chunk | accumulate; emit one `assistant.message` at turn end |
| tool call (start/update) | `tool.call`, then `tool.result` or `file.change` on completion |
| plan update | `plan` |
| usage/cost update | `cost` with `CostSource::Receipt` when real, otherwise `Unknown` |
| permission request | a `note` + an inbox approval, or an `error` with `retryable: false`; no fake auto-approval |
| `session/prompt` result | `turn.ended` with the mapped stop reason |

Do not replay ACP tool calls into a new native session; the v0.1 rule is that a switch carries plain
text, not native tool history.

## Open questions

1. **Protocol version.** Which ACP revision and method names are stable enough to support? Pin one and
   record it in `docs/design/adapters.md`.
2. **Permission flow.** The protocol has permission requests but `EventType` has no permission event.
   Options: (a) add additive `permission.request`/`permission.resolve` events in v0.2, or (b) map a
   request to an inbox `approval` and a `note`, with no live pause. Option (b) is smaller but cannot
   resume the same turn automatically.
3. **Streaming text.** v0.1 emits one `assistant.message` per turn. Keep that for the first slice and
   add a chunk event later, so the log stays small and the web timeline stays simple.
4. **Resume.** Does the server support `session/load`/resume across processes? If not,
   `can_resume_native = false` and the normal capsule seed is used.
5. **Cost.** ACP usage may be tokens only; report `CostSource::Unknown` rather than inventing a price.

## First-issue-sized slice

Add an ACP adapter module that starts a fake ACP server script (JSON-RPC over stdio), performs
`initialize`, `session/new` and one `session/prompt`, and maps the streamed updates to
`user.message`, `assistant.message`, `tool.call`, `tool.result` and `turn.ended`. Run the existing
adapter-contract tests against it, including a clean stop and a server that exits without a result. No
permission UI and no real ACP server in the test suite.

The test must fail if the adapter drops or reorders the events; a fixture-only replay is not enough.
