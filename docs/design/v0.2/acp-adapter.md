# v0.2 — ACP adapter

Status: current. The first slice is implemented (v0.2); v0.1 ships the
`claude-code`, `opencode` and generic `cli` adapters. The ACP adapter, its registry entry, fake-server
tests and pinned subset are in
`docs/design/adapters.md` §10. Real ACP servers, native resume, a permission UI, live pause and
streaming chunk events remain **planned**.

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
- `vowp-protocol::HarnessKind` — now `claude-code`, `opencode`, `cli`, `acp`, `vowp`. The additive
  `acp` variant is implemented; the executor string is `acp:<model>`.

## Mapping sketch

ACP revision 1 is pinned. The adapter's mapping is:

| ACP signal | Adapter event |
|---|---|
| agent message chunk | accumulate; emit one `assistant.message` at turn end |
| tool call (start/update) | `tool.call`, then `tool.result` and optionally `file.change` on completion |
| plan update | `plan` |
| usage on the prompt result | `cost` with tokens and `CostSource::Unknown` (no price is invented) |
| permission request | `note` + `error{retryable: false}` and a `cancelled` answer; no fake auto-approval and no inbox approval yet |
| `session/prompt` result | `turn.ended` with the mapped stop reason |

Core still appends the framing: `session.executor{attach}` with the native id, `turn.started`,
`user.message` (the prompt), and `session.executor{detach}`. The adapter never emits those.

Do not replay ACP tool calls into a new native session; the v0.1 rule is that a switch carries plain
text, not native tool history.

## Decisions for the first slice

1. **Protocol version.** ACP revision **1** is pinned in `ACP_PROTOCOL_VERSION`; the supported
   methods are exactly `initialize`, `session/new` and `session/prompt`, with the update kinds in
   `docs/design/adapters.md` §10.2.
2. **Permission flow.** Option (b) without the inbox step: a request becomes a `note` plus a
   non-retryable `error{refused}` and the client answers `cancelled`; the turn ends. The inbox
   approval variant is **planned** and needs a way for a worker run to pause and resume, which
   this slice does not have.
3. **Streaming text.** One `assistant.message` per turn, built from accumulated
   `agent_message_chunk` text; a chunk event is **planned**.
4. **Resume.** `session/load`/resume is not implemented; `can_resume_native = false`, so core uses
   the normal capsule seed.
5. **Cost.** Usage is tokens only with `CostSource::Unknown` and `usd: None`; the price table is
   core's fallback, never the adapter's.

## First slice — implemented

`crates/vowp-adapters/src/acp/` drives a fake ACP server (JSON-RPC 2.0 over stdio) through
`initialize`, `session/new` and one `session/prompt`, mapping the streamed updates to
`tool.call`, `tool.result`, `file.change`, `plan`, one `assistant.message`, `cost` (tokens only)
and `turn.ended`. Core still appends `session.executor`, `turn.started` and `user.message`.

The tests cover the existing adapter contract (a clean turn and a refused permission), an
exited-without-result server, malformed JSON, an unknown method, a JSON-RPC quota error and
`stop()` killing the process group. `HarnessKind::Acp` is registered in `vowp-core` and probed by
`vowp doctor` when `[harnesses.acp]` or an ACP profile rung is configured.

**Still planned:** a real ACP server, authentication, native resume, a permission UI or live
pause, streaming chunk events, the inbox-approval fallback, the process-usage sidecar and the
ACP cost price table/receipts.

## Slice history

The first-issue-sized slice was: add an ACP adapter module that starts a fake ACP server script
(JSON-RPC over stdio), performs `initialize`, `session/new` and one `session/prompt`, and maps the
streamed updates to canonical content events plus `turn.ended`. That slice is implemented; the
event-order tests fail if the adapter drops or reorders events, and a fixture-only replay is not
used. The prompt's `user.message` is the core-owned framing event, not an adapter event.
