# adapters.md — driving harnesses headlessly

Used by the adapter implementations and their callers.
Types are in `types.md` §12 (trait, `StartInput`, `RunSummary`, `AdapterError`) and §5 (events). Crate API: `crates/vowp-adapters.md`.
Rules that never bend (design summary §1): stock binaries, official headless JSON modes, **no TUI scraping, no keystrokes, no credential handling**.

Observed facts below come from real harness runs (Claude Code 2.1.319 `stream-json`, opencode 2.0.28 `run --format json`).

---

## 1. Contract recap

- **One process per run = one prompt = one turn.** Continuing a conversation is a new run with `resume = Some(native id)`.
  v0.1 `live_input` is `false` for every adapter; `send()` returns `AdapterError::Unsupported("live_input")`.
- `Adapter::start` spawns the child **in its own process group** (`process_group(0)`), `cwd = input.cwd`, stdin/stdout/stderr piped,
  and returns immediately. Three threads: stdout reader (line by line → `StreamParser::feed_line` → `HarnessEvent`s into the
  `mpsc` channel), stderr reader (keeps a ring of the last 4 KiB and tees to `<event_log_hint>.err` when given), and a waiter that
  `wait()`s on the child, then joins readers, then calls `StreamParser::finish` and stores the `RunSummary`.
- **Terminal event on every exit path.** After the last stdout line (or on spawn failure after spawn succeeded, crash, kill, interrupt,
  timeout) the adapter emits exactly one `TurnEnded{stop_reason}` and then closes the channel. `stop_reason`: `EndTurn` (clean),
  `Limit` (classified limit), `Error`, `Interrupted` (stop/interrupt requested by caller), `Crash` (killed by signal / no result line and non-zero exit),
  `MaxTurns` (harness said so). A `LimitEvent`/`ErrorEvent` event precedes the `TurnEnded` when applicable; a `CostEvent` is emitted at end of run
  (claude) or per step (opencode) *before* `TurnEnded`.
- **Framing is core's, not the adapter's.** Adapters never emit `SessionExecutor` or `TurnStarted`. Core (`run_turn`, crates/vowp-core.md §3) appends, in this order: `session.executor{attach}` → `turn.started` → `user.message` → the adapter's events → (`cost`, `limit`|`error`, `turn.ended` as emitted by the adapter; core appends a missing `turn.ended` itself) → `session.executor{detach}` (carrying `RunSummary.native_session_id`). The adapter's terminal `TurnEnded` is still mandatory on every exit path.
- `stop()` is idempotent: SIGTERM to the process group, wait up to 3 s, SIGKILL; a second call is a no-op returning `Ok`.
  `interrupt()`: SIGINT to the group, then `stop()` if the child is still alive after 5 s. Both set `Interrupted` so `wait()` reports `outcome = Killed`.
- `wait()` blocks until exit and thread joins; calling it again returns a clone of the stored summary.
- **Raw tee:** every stdout line is appended verbatim to `event_log_hint` (if `Some`) *before* parsing (flush per line), so a parser bug never loses data.
  Lines that are not valid JSON are teed too and, for JSON adapters, produce no events but are kept in a 4 KiB `noise` buffer used by classification.
- **Environment:** the child inherits only: `PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TERM=dumb`, `TMPDIR`, `XDG_*`, plus exactly the variable
  **names** the caller put in `StartInput.env` (resolved by core from `[harnesses.*].env_pass`). The adapter never logs env keys' values, never prints argv
  with the prompt (argv is logged as `argv[0..n-1]` + `<prompt omitted>`).
- Adapters never sleep or time out on their own. Hung/silent runs are L1's job (stall rule: 8 min with no output at all, 20 min otherwise —
  loops.md). `StartInput.timeout`, when set, makes the waiter call `stop()` and finish with `outcome = Killed`, `stop_reason = Interrupted`.
- `HarnessEvent.raw`: see §7.

---

## 2. claude-code adapter (`ClaudeCodeAdapter`)

### 2.1 argv

```
claude -p --output-format stream-json --verbose
       --model <executor.model>
       [--effort <executor.effort>]
       --permission-mode <permission_mode | "auto">
       [--append-system-prompt <system_note>]
       [--add-dir <dir>]...                       # from ClaudeConfig.add_dirs (default none)
       ( --resume <resume>  |  --session-id <new_native_id> )   # exactly one when resume/new_native_id is Some; neither otherwise
       [<extra_args>...]                          # ClaudeConfig.extra_args, verbatim, last before the prompt
```

**Prompt goes on stdin**, not argv. Reasons: `argv` is visible in `ps`/`/proc` to every local user (prompts contain briefs and sometimes pasted
paths), and Linux caps a single argument at 128 KiB (`MAX_ARG_STRLEN`) which a capsule seed + brief can exceed. `claude -p` reads the prompt from
stdin when no positional prompt is given (documented headless behaviour). The adapter writes the prompt, closes stdin, then starts reading.
If `probe()` finds the stdin form unsupported — it never has — fall back to
the last-arg form: `ClaudeConfig.prompt_via_arg: bool`, default `false`.)

Rules:
- `--effort` omitted when `executor.effort` is `None`. `--model` always passed.
- New session: caller sets `new_native_id = Some(uuid-v4)`; the adapter passes `--session-id <uuid>`; the `system/init` `session_id` must equal it
  (if it differs, trust the init value and record it in `RunSummary.native_session_id`).
- Resume with `can_resume_native`: `--resume <id>`. If a resumed run fails immediately with text matching `No conversation found` the adapter
  returns `outcome = Error` with `detail` containing it; core then retries once as a fresh session seeded with a capsule (sessions.md §5).
- `ClaudeConfig.permission_mode` default is `"auto"`; VOWP never sends keystrokes, so a mode that prompts would hang — `probe()` does not check this; docs say so.
- Binary: `ClaudeConfig.binary` (default: first `claude` on `PATH`).

### 2.2 Stream-json message → canonical events

Every line is one JSON object with `type`. `session_id` appears on every message. Emitted events are `HarnessEvent`s; `actor` shown in the table.

| stream-json | Condition | Emits |
|---|---|---|
| `{"type":"system","subtype":"init",…}` | first line | **no event** (framing — `session.executor` attach/detach and `turn.started` — is owned by core, see below). Records `session_id` as the native id (exposed as `RunSummary.native_session_id` and in every later event's `source.native_session_id`). Also records `model` from `init.model` (used for later events' `source.model`). Other `system` subtypes (`hook_*`, `status`, …) → `Note{text: "system:<subtype>"}` only when `subtype` is not `init`; ignore if noisy (`hook_started`, `hook_response`). |
| `{"type":"assistant","message":{content:[…]}}` | each content block, in order | `text` → `AssistantMessage{text}` (skip empty); `thinking` → `AssistantReasoning{text: thinking}` **only if `thinking` is non-empty** (the sample shows `"thinking":""` with an opaque `signature`; the signature is never stored) ; `tool_use{id,name,input}` → `ToolCall{call_id:id, name, input}` |
| `{"type":"user","message":{content:[{type:"tool_result",tool_use_id,content,is_error?}]}}` | each tool_result block | `ToolResult{call_id: tool_use_id, ok: !is_error.unwrap_or(false), output / output_ref}` — `content` is a **string or an array of `{type:"text",text}` blocks** (both seen): join text blocks with `\n`. Adapters always emit `output: Some(full text)` and never `output_ref`: the session layer (`vowp-store` `SessionLog::append`) spills outputs > 8192 bytes to `blobs/<sha256>` and rewrites the payload. |
| `{"type":"user",…}` with `text` blocks (replayed user text) | | ignored |
| `{"type":"rate_limit_event","rate_limit_info":{status,resetsAt,rateLimitType,unifiedWindows,…}}` | `status == "rejected"` | `LimitEvent{kind: Quota, provider: "claude", resets_at: <§2.4>, message: "rate_limit_event rejected: <rateLimitType>"}`; remembered as `pending_limit` so `finish()` can set `RunSummary.limit` |
| same | `status == "allowed_warning"` | `Note{text: "claude usage warning: <rateLimitType> utilization <u>"}` |
| same | `status == "allowed"` | no event; the parser keeps the latest `rate_limit_info` (`last_rate`) for diagnostics; not persisted in v0.1 |
| `{"type":"result",…}` | final line | `CostEvent{cost}` (§6) then, in `finish()`, `TurnEnded`. Reads `subtype`, `is_error`, `api_error_status`, `result`, `stop_reason`, `terminal_reason`, `num_turns`, `duration_ms`. |
| `{"type":"stream_event",…}` (partial deltas, only with `--include-partial-messages`, which VOWP does not pass) | | ignored |
| any other `type` | | ignored; teed raw |

`result` handling (observed values: `subtype:"success"`, `is_error:false`, `stop_reason:"end_turn"`, `terminal_reason:"completed"`, `api_error_status:null`):

| `result` fields | `StopReason` | `RunOutcome` |
|---|---|---|
| `is_error:false`, `subtype:"success"` | `EndTurn` | `Ok` |
| `subtype:"error_max_turns"` | `MaxTurns` | `Ok` (not an error: L2 treats a missing report like any other) |
| `subtype:"error_during_execution"` or `is_error:true` with no classification | `Error` | `Error` |
| `is_error:true` and `api_error_status` ∈ {401,403} or auth text | `Error` | `Auth` |
| `api_error_status == 402` | `Error` | `Credits` |
| `api_error_status` ∈ {429, 529} or transient text | `Limit` | `Rate` |
| quota text or rejected `rate_limit_event` | `Limit` | `Limit` (`limit_kind = Quota`) |
| context text | `Error` | `Context` |

If `is_error:true`, the `result` string (`result` field) is the diagnostic text fed to `classify_text` together with the stderr tail and the `noise` buffer.
`api_error_status` (number or null) is passed as `http_status`. Also emit `ErrorEvent{kind, message (redacted ≤300), retryable}` before `TurnEnded` whenever the outcome is not `Ok`.

No `result` line and exit 0: `outcome = Error`, `detail = "no result line"`, `stop_reason = Crash`. No `result` and non-zero exit: classify stderr/noise;
if nothing matches → `Crash`.

### 2.3 Limit texts (claude)

Texts appear in `result.result`, in stderr, or as plain non-JSON stdout lines. Examples the parser must handle (also fixtures):

| Text | kind | reset |
|---|---|---|
| `Claude AI usage limit reached\|1794164400` | Quota | unix `1794164400` = `2026-11-08T19:00:00Z` |
| `You've hit your limit · resets 11pm (Europe/Oslo)` | Quota | next 23:00 in the node's tz (see §2.4) |
| `5-hour limit reached ∙ resets 3pm` | Quota | next 15:00 local |
| `You've hit your weekly limit · resets Nov 12, 9am` | Quota | that date 09:00 local (weekly) |
| `Session limit reached ∙ resets 4:30 am` | Quota | 04:30 |
| `API Error: 429 {"type":"error","error":{"type":"rate_limit_error",…}}` | Rate | now + 10 min |
| `API Error: 529 Overloaded` / `overloaded_error` | Rate | now + 10 min |
| `Invalid API key · Please run /login` / `OAuth token has expired` / `Not logged in` | Auth | none |
| `Prompt is too long` / `prompt is too long: 210000 tokens > 200000 maximum` | Context | none |
| `Your credit balance is too low to access the Anthropic API` | Credits | none |

Regexes: see §5.3 (shared with every adapter).

### 2.4 Reset time

**Margin rule (single place):** adapters report the **exact** reset time the harness gave (no margin). The +2 min safety margin is added **once**, by core's L6 when it writes `limits.resets_at` (loops.md §8.1). Nothing else adds it.

Preference order for `LimitEvent.resets_at` / `RunSummary.limit.resets_at`:

1. **Structured:** the *rejected* `rate_limit_event.rate_limit_info`: `max(resetsAt of every window in unifiedWindows whose status == "rejected", and the top-level resetsAt if the top-level status == "rejected")`, unix seconds (`1794164400` = 2026-11-08T19:00:00Z).
2. `|<10 digits>` suffix in the text → unix seconds.
3. Clock-time text (`resets 11pm`, `resets 3pm`, `resets 4:30 am`, `resets Nov 12, 9am`) parsed by `parse_reset_time(text, now, tz_offset_minutes)` (§5.2). The text names a zone like `(Europe/Oslo)`; v0.1 cannot resolve IANA names (decisions.md open question 2), so: use `StartInput`-independent config `ClaudeConfig.tz_offset_minutes` (default `0`; core fills it from `[node] tz_offset_minutes`, which `vowp init` writes from the machine's current offset) and ignore the zone name. Result is the next occurrence strictly after `now` (no margin).
4. Fallback `now + 60 min` (quota) — still a `Limit`.

`Rate` kind: `resets_at = now + 10 min` (`TRANSIENT_BACKOFF_MIN` constant `10`, in `classify`).

### 2.5 `capabilities()`

`can_resume_native: true, can_inject_assistant_history: false, can_switch_model_in_session: true` (new `--model` on `--resume` is accepted),
`live_input: false, emits_reasoning: true (when non-empty), emits_cost_receipts: true, reports_resets_at: true`.

### 2.6 Importer (nice-to-have, v0.1.x)

`vowp session watch` reads `~/.claude/projects/**/<uuid>.jsonl` transcripts. Not part of the adapter run path; the
`claude_code::import_transcript(path, tz_offset_minutes) -> Result<Vec<HarnessEvent>, AdapterError>` parser reusing the same message mapping is specified in `crates/vowp-adapters.md` §5 as **optional**. The core importer (`crates/vowp-core.md` §3, `ClaudeTranscriptImporter::scan_once`) owns directory scanning and byte offsets and calls this parser per file.

---

## 3. opencode adapter (`OpencodeAdapter`)

### 3.1 argv

```
opencode run --format json --auto
         --model <executor.model_arg()>            # "provider/model#variant", '#variant' only if Some
         [--session <resume>]                      # when resume is Some
         [--title "vowp <role> <job_id>"]          # optional, from OpencodeConfig.title (core fills it); shows in the opencode app
         [<extra_args>...]
         <prompt>                                  # LAST positional argument
```

The adapter closes stdin immediately (`Stdio::null()`).
Prompt size: Linux's per-argument cap is 128 KiB. If `prompt.len() > 100_000` bytes the adapter writes the prompt to
`<cwd>/.vowp/prompts/<run-or-session>-<n>.md` (creating the directory; `.vowp/` is git-ignored by `vowp init`) and passes
`Read the instructions in .vowp/prompts/<file> and follow them exactly.` instead. The prompt also shows up in `ps`; briefs are not secret (secrets are never put in prompts: AGENTS.md).

- `--auto` is mandatory: opencode must never open a permission dialog (design summary principle 1). No keystrokes, ever.
- New session: no `--session`. The id is learned from the first event's top-level `sessionID` (e.g. `ses_e439617b6ffepWG4UNK521b6ly`) and reported as `RunSummary.native_session_id`
  and in `RunSummary.native_session_id` (core writes it into the detach event and `adapters.json`); the adapter emits **no** framing event.
- `--model` variant sigil: the Executor stores `variant = Some("max")`; `model_arg()` renders `provider/model#max`.
- Binary: `OpencodeConfig.binary` (default first `opencode` on `PATH`, falling back to `$HOME/.opencode/bin/opencode` — checked with `Path::exists`, only for the user's own HOME read from `StartInput.env`'s `HOME`… the adapter reads `HOME` from the process env for this single lookup).

### 3.2 Event mapping (real data, opencode 2.0.28)

Every stdout line is a JSON object with `type`, `timestamp` (unix **milliseconds**), top-level `sessionID`, and `part`. Observed distinct shapes: `step_start`, `tool_use`
(status `completed` or `error`), `text`, `step_finish`. (No `reasoning` line was observed in the recorded runs, but see the reasoning row.) Errors are `type:"error"`.

| opencode line | Emits (in order) |
|---|---|
| first line of the run (any type) | no event: records `sessionID` as the native id (`source.native_session_id` on later events, `RunSummary.native_session_id`) |
| `step_start` (`part.type="step-start"`, `part.messageID`) | **no event** (core emits `turn.started`; steps are tool-loop iterations of one turn) |
| `tool_use`, `part.type="tool"`, `part.state.status="completed"` | `ToolCall{call_id: part.id, name: part.tool, input: part.state.input}` then `ToolResult{call_id: part.id, ok: true, output: part.state.output (string; if not a string, its JSON text), exit_code: JSON pointer `/state/metadata/metadata/exit` (the real nested shape: `state.metadata = {"metadata":{"status":"completed","truncated":false,"exit":0},"content":[…]}`), falling back to `/state/metadata/exit`, else None}`; both events carry `ts` from `part.state.time.start` / `.end` (ms) when present, else `timestamp`. **One line → two events.** For tool names `edit`, `write`, `patch`, `multiedit` (if `state.input.filePath` or `path` exists) additionally emit `FileChange{path, op: Modify (Create for `write` when output says created, else Modify)}` — best-effort; core's owns gate uses git, not these events. |
| `tool_use` with `state.status="error"` | `ToolCall{…}` + `ToolResult{ok: false, output: part.state.error}` (observed: `{"status":"error","input":{…},"error":"Web search cancelled","time":{start,end}}`). **Not** a run error: the harness continues. |
| `tool_use` with `state.status` ∈ {`pending`,`running`} | ignored (the final line carries the result; v0.1 only sees completed/error) |
| `text` (`part.type="text"`, `part.text`) | `AssistantMessage{text: part.text}` |
| `reasoning` (`part.type="reasoning"`, `part.text`) | `AssistantReasoning{text}` (replayable=false). Unverified shape: tolerate absence. |
| `step_finish` (`part.cost` USD, `part.tokens.{input,output,reasoning,cache.{read,write}}`, `part.reason`) | `CostEvent{cost: Cost::receipt(part.cost, Tokens{input, output, reasoning, cache_read: cache.read, cache_write: cache.write})}` per step (accumulated into `RunSummary.cost` by `Cost::add`). Then by `part.reason`: `"stop"` (and any value other than `tool-calls`) → `TurnEnded{turn:1, stop_reason: EndTurn}` is **deferred to `finish()`** so exactly one TurnEnded is emitted; `"tool-calls"` → nothing (loop continues); `"length"` → remember `max_tokens=true` → `StopReason::MaxTurns`; `"error"` → remembered as failed step. |
| `error` line (`type:"error"`, payload under `error`/`part`/top-level `message`) | `ErrorEvent{kind, message (redacted), retryable}` where kind comes from `classify_text` (§5): credits/auth/rate/context/quota → `ErrorKind::Limit`/`Auth`; else `AdapterFailed`. Also stored for `finish()`. |
| anything else | ignored; teed raw |

`finish(exit, stderr_tail)` determines the outcome (precedence in §5.4):
1. Any `type:"error"` line text, plus the stderr tail and non-JSON noise, is classified; if classification hits → `Limit/Credits/Auth/Rate/Context` outcome and a `LimitEvent` (not for `Context`: emit `ErrorEvent` and `LimitEvent{kind: Context}` both).
2. Else exit 0 → `Ok`/`EndTurn` (even if no `step_finish` was seen when text was produced; with no output at all → `Error`, `detail="no output"`).
3. Else non-zero exit → `Crash` if killed by signal (exit `None`), else `Error`.
4. `Killed` if `stop()`/`interrupt()`/timeout fired.

**Provider text for `provider` in `LimitEvent`:** `executor.provider()` (the text before `/` in the model).

### 3.3 Provider error classification (opencode)

opencode forwards provider HTTP errors inside the `error` line or on stderr. Matching is on text and an extracted status (`"statusCode":402`, `"status":429`, `HTTP 401`):

| Signal | kind |
|---|---|
| `402`, `Payment Required`, `insufficient balance`, `Insufficient Balance`, `insufficient credits`, `credit balance` | Credits |
| `401`, `403` with `unauthorized`/`invalid api key`/`authentication`/`API key` ... | Auth |
| `429`, `rate limit`, `too many requests`, `529`, `overloaded` | Rate (10 min backoff) |
| `context length`, `context_length_exceeded`, `maximum context`, `prompt is too long`, `input is too long`, `context window … exceeded` | Context |
| `usage limit`, `quota`, `limit reached` (opencode-zen / subscription providers) | Quota |

### 3.4 Observed behaviours the adapter must respect

- opencode sometimes prints **nothing at all** for minutes after start. The adapter does not time out; L1 kills runs silent for 8 min and continues them.
- A `tool_use` can fail (`state.status = error`) while the run still exits 0.
- Sessions created by `run` appear in the opencode app; `--title` helps recognising them.
- `sessionID` is present on every line including `error`; use the first seen.
- Cost `part.cost` is USD for the step as computed by opencode from the provider's price table — treated as a **receipt**.

### 3.5 `capabilities()`

`can_resume_native: true, can_inject_assistant_history: false, can_switch_model_in_session: true` (`--model` per run),
`live_input: false, emits_reasoning: false` (true if a `reasoning` part is ever seen; report `true` only via `probe`-independent static value `false`),
`emits_cost_receipts: true, reports_resets_at: false`.

---

## 4. generic `cli` adapter (`CliAdapter`)

Drives any command that prints JSON lines or plain text, using a **mapping file** (TOML), referenced from `vowp.toml`:
`[harnesses.codex] mapping = ".vowp/adapters/codex.toml"` and used as executor `cli:codex`. Illustrative, **untested against the real tools**; the
fixtures `cli_codex_like.jsonl` / `cli_gemini_like.txt` are hand-built.

### 4.1 Schema (all keys, with defaults)

```toml
name = "codex"                       # required; equals the executor model/name `cli:<name>`; [a-z0-9-]+
provider = "openai"                  # limits key; default = name
version_argv = ["codex", "--version"]# for probe(); default = [command.argv[0], "--version"]

[command]
argv = ["codex", "exec", "--json"]   # required, non-empty; argv[0] resolved on PATH or absolute
prompt_via = "arg"                   # "stdin" | "arg" (appended last) | "flag:<--prompt>" (flag then prompt) ; default "arg"
resume_argv = ["codex", "exec", "resume", "{native_id}", "--json"]  # optional; {native_id} substituted; presence => can_resume_native
model_flag = "--model"               # optional: "<flag> <executor.model>" is not used (the cli executor's model IS the name); use `model_arg` below
model_arg  = ""                      # optional: if the Executor had `@effort`/`#variant`, passed as: <model_flag> <effort|variant>  (default none)
env = ["OPENAI_API_KEY"]             # NAMES of env vars to pass through (values read from the caller's env map at start)
cwd_flag = ""                        # optional: flag to pass cwd; default none (process cwd is set instead)

[output]
format = "jsonl"                     # "jsonl" | "text"
native_session_pointer = "/thread_id"# JSON pointer in any line giving the native session id (first match wins); optional
text_chunk_lines = 20                # format="text": flush an AssistantMessage every N lines or at EOF; default 20

[[map]]                              # evaluated top-to-bottom for every JSON line; ALL matching rules fire
match = { pointer = "/type", equals = "agent_message" }     # or { pointer = "/msg/kind", regex = "^tool_.*" } or { pointer = "/x", exists = true }
emit  = "assistant.message"          # an EventType string (types.md §5.1): assistant.message | assistant.reasoning | tool.call | tool.result | file.change | note | error | plan
fields = { text = "/message" }       # payload field -> JSON pointer (or "=literal" for a constant, e.g. "=true")
# Required payload fields per emit: assistant.message{text}, assistant.reasoning{text}, tool.call{call_id,name,input}, tool.result{call_id,ok,output},
# file.change{path,op}, note{text}, error{message}. Missing optional pointer => field omitted; missing required => rule skipped + Note("mapping: <rule index> missing <field>").

[limits]                             # regex lists (case-insensitive) matched against error text; first non-empty list in order credits, auth, quota, rate, context wins
credits = ["insufficient (funds|balance)"]
auth    = ["invalid api key", "not logged in"]
quota   = ["usage limit", "quota"]
rate    = ["429", "rate limit"]
context = ["context length"]
resets_at = { regex = "resets? at (\\d{10})", kind = "unix" }   # kind: "unix" | "clock" (parse_reset_time) ; optional
tz_offset_minutes = 0

[cost]
usd_pointer = "/usage/cost_usd"      # on any JSON line; last value wins; optional
input_pointer = "/usage/input_tokens"
output_pointer = "/usage/output_tokens"
reasoning_pointer = ""
cache_read_pointer = ""
cache_write_pointer = ""             # no usd_pointer => Cost::unknown() (price-table fallback is core's job)

[exit]
ok = [0]                             # exit codes meaning success; default [0]
limit = []                           # exit codes that mean "limit" (kind from [limits] text, else quota); default none
error_from_stderr = true             # classify stderr tail even on exit 0 if a [limits] regex matches AND a JSON/stderr error was seen
```

### 4.2 Validation (`Mapping::validate`, also run by `from_mapping_file`; errors are `AdapterError::Mapping{path,message}`)

1. `name` matches `^[a-z0-9][a-z0-9-]*$`; `command.argv` non-empty, no empty strings.
2. `prompt_via` ∈ {`stdin`,`arg`, `flag:<non-empty>`}; `resume_argv` (if set) contains `{native_id}` exactly once.
3. `output.format` ∈ {`jsonl`,`text`}; `[[map]]` rules and pointers only valid with `jsonl`; JSON pointers start with `/` (or are empty = whole value); `emit` is a known EventType string from the allowed list; every required field of the emit type is present in `fields` (pointer or `=literal`).
4. All regexes compile (`regex` crate, case-insensitive flag applied); each list ≤ 32 patterns, each ≤ 200 chars.
5. `[exit]` codes in 0..=255. `env` names match `^[A-Z_][A-Z0-9_]*$` and are **names only**: the file may never contain values; a validator rejects values that look like secrets (`sk-…`, `ghp_…`).
6. Unknown keys are rejected (`deny_unknown_fields`).

### 4.3 Behaviour

- `jsonl`: each line parsed as JSON (non-JSON lines → `noise`); each `[[map]]` rule evaluated; events created via `EventBody::from_parts`. If no rule matches a line it is ignored.
  Cost: pointers evaluated on every line; at `finish()` build `Cost` (`source: Receipt` if `usd_pointer` produced a number, else `Unknown`).
- `text`: stdout split into lines; every `text_chunk_lines` lines (or at EOF, or after 2 s of silence is **not** implemented — no timers) flush
  `AssistantMessage{text: chunk}`. stderr is classified. No tool events.
- No framing events: the first event is the first content event; the native id (if the mapping defines a pointer) is reported in `RunSummary.native_session_id`.
- Terminal `TurnEnded` rules: exit ∈ `exit.ok` → `EndTurn`; limit classification → `Limit`; else `Error`/`Crash`.
- `capabilities()`: `can_resume_native = resume_argv.is_some()`, `can_inject_assistant_history: false`, `can_switch_model_in_session: false`, `live_input: false`, `emits_reasoning: any rule emits assistant.reasoning`, `emits_cost_receipts: usd_pointer set`, `reports_resets_at: resets_at set`.

### 4.4 Example A — "codex exec --json"-style (illustrative)

```toml
name = "codex"
provider = "openai"
[command]
argv = ["codex", "exec", "--json"]
prompt_via = "arg"
resume_argv = ["codex", "exec", "resume", "{native_id}", "--json"]
env = ["OPENAI_API_KEY"]
[output]
format = "jsonl"
native_session_pointer = "/session_id"
[[map]]
match = { pointer = "/type", equals = "agent_message" }
emit = "assistant.message"
fields = { text = "/text" }
[[map]]
match = { pointer = "/type", equals = "command_execution" }
emit = "tool.call"
fields = { call_id = "/id", name = "=shell", input = "/command" }
[[map]]
match = { pointer = "/type", equals = "command_result" }
emit = "tool.result"
fields = { call_id = "/id", ok = "/ok", output = "/output" }
[limits]
credits = ["insufficient_quota", "billing"]
rate = ["429", "rate limit"]
auth = ["invalid api key", "401"]
[cost]
input_pointer = "/usage/input_tokens"
output_pointer = "/usage/output_tokens"
```

### 4.5 Example B — "gemini -p"-style plain text (illustrative)

```toml
name = "gemini"
provider = "google"
[command]
argv = ["gemini", "-p"]
prompt_via = "arg"
env = ["GEMINI_API_KEY"]
[output]
format = "text"
text_chunk_lines = 40
[limits]
quota = ["quota exceeded", "RESOURCE_EXHAUSTED"]
rate = ["429"]
auth = ["API key not valid"]
[exit]
ok = [0]
```

---

## 5. Classification (`vowp_adapters::classify`)

Used by all adapters and by core (gate output, probe results).

### 5.1 Signatures

```rust
/// What a piece of error text means.
#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub kind: LimitKind,                 // Quota | Credits | Rate | Context | Auth   (types.md §4)
    pub resets_at: Option<Timestamp>,    // Quota: parsed reset; Rate: now+10min; others None
    pub message: String,                 // redacted excerpt, <= 300 chars
    pub http_status: Option<u16>,
}
pub const TRANSIENT_BACKOFF_MIN: u32 = 10;
pub const QUOTA_FALLBACK_MIN: u32 = 60;

/// Classify error text. `provider_hint` is informational ("claude", "provider", "" ) and selects nothing in v0.1 except the
/// message prefix; `http_status` (if the harness gave a number) takes part: 402 -> Credits, 401/403 -> Auth, 429/529 -> Rate.
/// Returns None if nothing matches. NOTE: callers apply the exit-code gate (§5.5); this function is pure.
pub fn classify_text(provider_hint: &str, text: &str, http_status: Option<u16>, now: Timestamp, tz_offset_minutes: i32) -> Option<Classified>;

/// Parse a reset time out of limit text, per §2.4 rules 2-3. `None` if none found.
pub fn parse_reset_time(text: &str, now: Timestamp, tz_offset_minutes: i32) -> Option<Timestamp>;

/// Claude `rate_limit_info` object -> reset time (§2.4 rule 1). No margin added.
pub fn reset_from_rate_limit_info(info: &serde_json::Value) -> Option<Timestamp>;

/// Strip secret-looking substrings, collapse whitespace, truncate to `max_chars` (on a char boundary) with a trailing "…".
pub fn redact(text: &str, max_chars: usize) -> String;

/// Extract an HTTP status from text such as `"statusCode":402`, `"status": 429`, `HTTP 401`, `API Error: 529`.
pub fn extract_http_status(text: &str) -> Option<u16>;
```

### 5.2 `parse_reset_time` algorithm

1. `\|(\d{10})\b` → `Timestamp::from_unix_secs` (exact).
2. Else regex `(?i)resets?\s+(?:at\s+)?(?:(?P<mon>jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(?P<day>\d{1,2}),?\s+)?(?P<h>\d{1,2})(?::(?P<m>\d{2}))?\s*(?P<ap>am|pm)?`:
   - `h` 12-hour with `ap`: `pm && h<12 → h+12`; `am && h==12 → 0`; without `ap`, `h` is 24-hour; reject `h>23` or `m>59`.
   - Work in local time = UTC + `tz_offset_minutes`. If `mon/day` present: that date in the current local year (if already past by > 1 day, next year); else today at `h:m`, and if that is `<= now` add one day.
   - Convert back to UTC (no margin).
3. Else `None`.

Examples (`now = 2026-11-08T18:10:00Z`, `tz_offset_minutes = 60`, i.e. local 19:10):
`"resets 11pm"` → `2026-11-08T22:00:00Z`; `"resets 3pm"` → next day `2026-11-09T14:00:00Z`; `"resets 4:30 am"` → `2026-11-09T03:30:00Z`;
`"resets Nov 12, 9am"` → `2026-11-12T08:00:00Z`; `"…|1794164400"` → `2026-11-08T19:00:00Z`.

### 5.3 Rules (precedence **credits > auth > quota > rate > context**; first matching class wins; matching is case-insensitive)

| Kind | Regex (alternation) | Status shortcut |
|---|---|---|
| Credits | `insufficient[ _](?:balance\|credits?\|funds)\|payment required\|credit balance (?:is )?too low\|\b402\b\|out of credits` | 402 |
| Auth | `invalid api key\|incorrect api key\|unauthorized\|\b401\b\|please run /login\|not logged in\|oauth token (?:has )?(?:expired\|revoked)\|authentication[_ ](?:error\|failed)\|permission_denied` | 401, 403 |
| Quota | `usage limit\|hit your (?:\w+[ -])*limit\|limit (?:reached\|exceeded)\|out of (?:extra )?usage\|quota\|limit will reset\|resets? (?:at )?\d\|usage_limit\|weekly limit\|session limit\|5-hour limit` | — |
| Rate | `rate[_ ]limit\|\b429\b\|\b529\b\|overloaded\|too many requests\|\bbusy\b` | 429, 529 |
| Context | `prompt is too long\|context (?:window\|length) (?:exceeded\|overflow)\|maximum context length\|context_length_exceeded\|input is too long\|exceeds the context` | — |

Note the quota class contains `resets? \d` (very broad). A text that matches *both* Quota and Rate (`rate_limit_error … resets 5pm`)
is Quota. Plain `429` without quota words is Rate. `"rate_limit_event"` as a JSON key name must **not** match: callers feed only message text (`result` string, error message fields, stderr, noise), never whole JSON lines.

### 5.4 Redaction

`redact` removes: `sk-[A-Za-z0-9_-]{20,}`, `sb_secret_[A-Za-z0-9_-]{10,}`, `ghp_[A-Za-z0-9]{20,}`, `github_pat_[A-Za-z0-9_]{20,}`, `AKIA[0-9A-Z]{16}`,
`-----BEGIN [A-Z ]*PRIVATE KEY-----`, `xox[abp]-[A-Za-z0-9-]{10,}`, `Bearer [A-Za-z0-9._~+/-]{20,}`, and `(?i)(api[_-]?key|token|secret|password)["']?\s*[:=]\s*["']?[^\s"']{8,}` (replaced by `<redacted>`).
It never fails and is applied to every `message` in `LimitEvent`/`ErrorEvent`/`RunSummary.limit` and to `Run.detail`.

### 5.5 Exit-code gating

A limit regex hit counts only if **(exit code != 0) OR (an error was seen: `result.is_error`, an opencode `type:"error"` line, `rate_limit_event` rejected, or non-empty stderr tail that itself matched)**.
Exit 0 with no error signal and a model that merely *talks about* "usage limit" in its text is `Ok`. The adapter builds the classifier input from error sources only
(never from `AssistantMessage` text). A rejected `rate_limit_event` counts as an error signal by itself (`rejected` → `Limit` even if exit 0).
Final precedence for the run outcome: `Killed` (caller stop) > `Credits` > `Auth` > `Limit` (Quota) > `Rate` > `Context` > `Crash`/`Error` > `Ok`.

### 5.6 Unknown errors

Non-zero exit (or `is_error`) with no classification → `Error` (or `Crash` when killed by signal / no output at all). Core, not the adapter, counts **3 consecutive Error/Crash runs
for one provider ⇒ limits row `unavailable` for 15 min** (decisions.md D15).

---

## 6. Cost receipts

| Adapter | Source | Mapping |
|---|---|---|
| claude-code | `result.total_cost_usd` + `result.usage` | `Cost::receipt(total_cost_usd, Tokens{input: usage.input_tokens, output: usage.output_tokens − thinking_tokens (clamped ≥ 0), reasoning: usage.output_tokens_details.thinking_tokens, cache_read: usage.cache_read_input_tokens, cache_write: usage.cache_creation_input_tokens})` (`output_tokens` already includes thinking tokens, so `Tokens::total()` equals the harness's own count). The figure is a **list-price equivalent**: on a subscription nothing is billed (`modelUsage.<m>.costBasis: "list"`). The `CostEvent` is still `source: Receipt`, and the ledger row's `detail_json` gets `{"billed": false, "basis": "list"}` (core sets it for `claude-code` runs; `RunSummary` has no field for it). |
| opencode | each `step_finish.part.cost`, `.tokens` | one `CostEvent` per step; sums via `Cost::add`; `source: Receipt`. `reasoning` is separate from `output` in opencode tokens (some providers bill reasoning as output; core's totals count both). |
| cli | `[cost]` pointers | `Receipt` if `usd_pointer` yields a number, else `Unknown` (usd `None`). |
| acp | `session/prompt` result `usage` | one `CostEvent` at turn end with `Cost { usd: None, tokens, source: Unknown }`; the server reports tokens, not a price. `Tokens` reads `inputTokens`, `outputTokens`, `thoughtTokens`/`reasoningTokens`, `cachedReadTokens` and `cachedWriteTokens`. A result without usage stays `Cost::unknown()`. |
| none seen | — | `Cost::unknown()` (never zero). A run that ended without any receipt (crash before `result`/`step_finish`) is `Unknown`. |

**Price-table fallback is not computed in adapters.** Core (`ledger`) turns `Unknown` into `PriceTable` when a `[providers.<p>.prices.<model>]` entry exists and token counts are non-zero.

---

## 7. Raw handling

- `HarnessEvent.raw` is set only for: `ToolCall`/`ToolResult` (the original JSON line or the relevant block), `ErrorEvent`, and `LimitEvent`. Never for `AssistantMessage`, `Cost`, `Note`.
- `raw` is serialized, capped at **16 KiB** (if larger: replaced by `{"truncated":true,"bytes":<n>,"head":"<first 2 KiB>"}`), and passed through `redact`-style secret stripping on its string leaves.
- Thinking signatures and any opaque provider-signed blobs are **never** stored (not even in `raw`); `AssistantReasoning` events are `replayable=false` (set by `EventEnvelope::new`).
- `raw` is display-only and never replayed into a harness.

## 8. `probe()`

Cheap, never calls a model, ≤ 5 s (`wait` with a poll loop; kill after 5 s).

| Adapter | Command | Parse |
|---|---|---|
| claude-code | `<binary> --version` | stdout like `2.1.319 (Claude Code)` → regex `(\d+\.\d+\.\d+)`; `ok = exit==0 && version.is_some()`; `path` = resolved binary |
| opencode | `<binary> --version` | stdout like `2.0.28` → same regex |
| cli | `mapping.version_argv` | `ok = exit == 0`; `version` = first regex match or first line (≤ 80 chars) |
| acp | no process probe: resolve `[harnesses.acp].binary`, else `claude-code-acp` on `PATH` | `ok = found and executable`; `version = None` (a `--version` call would wait on JSON-RPC stdin) |

Missing binary → `ProbeResult{ok:false, version:None, path:None, detail:"binary not found on PATH"}`. `probe()` never returns `Err`.

## 9. Fixtures and fake harnesses

Location: `crates/vowp-adapters/tests/fixtures/` and `tests/fakes/`. All fixtures are sanitised (no secrets; cwd `/home/user/project`; ids shortened where needed).

| Fixture | Origin | Content |
|---|---|---|
| `claude_ok.jsonl` | **real**: copy of `/home/user/vowp/ref/claude-stream-json-sample.jsonl` (4 lines: init, assistant "ok", rate_limit_event allowed, result) | happy path |
| `claude_tool_use.jsonl` | **real**-derived: a sanitised excerpt from a real planner run (init, assistant{thinking(empty)+tool_use}, user{tool_result str}, user{tool_result as array of text blocks}, assistant{text}, result with `num_turns: 4`) with long outputs truncated | tool call/result, both content shapes |
| `claude_rate_limit_rejected.jsonl` | hand-built from the documented shape (copy of the real `rate_limit_event`, `status:"rejected"`, five_hour window rejected, `resetsAt` 1794164400, exit 1 recorded in a sibling `.meta`) | structured limit |
| `claude_usage_limit_text.jsonl` | hand-built: a `result` with `is_error:true`, `result:"Claude AI usage limit reached\|1794164400"` | text limit with unix |
| `claude_weekly_limit.jsonl` | hand-built: `result` text `You've hit your weekly limit · resets Nov 12, 9am (Europe/Oslo)` | clock-time with date |
| `claude_429.jsonl` | hand-built: `is_error:true`, `api_error_status:429`, `result:"API Error: 429 … rate_limit_error"` | Rate |
| `claude_auth.jsonl` | hand-built: `is_error:true`, `api_error_status:401`, `result:"Invalid API key · Please run /login"` | Auth |
| `opencode_ok.jsonl` | **real**: sanitised copy of one short real run from `/home/user/vowp/runs/*.jsonl` (step_start, tool_use completed, step_finish reason tool-calls, text, step_finish reason stop) | happy path + cost |
| `opencode_tools.jsonl` | **real**-derived: includes one `tool_use` with `state.status:"error"` (`Web search cancelled`) | tool error ≠ run error |
| `opencode_tool_exit.jsonl` | **real**-derived (sanitised from a `shell` `tool_use` in `/home/user/vowp/runs/*.jsonl`): `state.metadata.metadata.exit:0` and a second line with a non-zero exit | exercises the nested exit pointer; test `opencode_tool_exit_code_from_nested_metadata` asserts `exit_code == Some(0)` / `Some(2)` |
| `opencode_402.jsonl` | hand-built: `{"type":"error","sessionID":"ses_x","error":{"name":"APIError","statusCode":402,"message":"Insufficient Balance"}}` + non-zero exit | Credits |
| `opencode_429.jsonl` | hand-built: `error` with `statusCode:429` | Rate |
| `opencode_auth.jsonl` | hand-built: `error` with `statusCode:401`, `Authentication Fails` | Auth |
| `opencode_context.jsonl` | hand-built: `error` text `maximum context length exceeded` | Context |
| `cli_codex_like.jsonl` | hand-built for §4.4 | cli jsonl + cost pointers |
| `cli_gemini_like.txt` | hand-built for §4.5 (45 text lines, then `quota exceeded` on stderr in a variant `.err` sidecar) | text format |

Each fixture may have a sidecar `<name>.meta.json`: `{"exit": 1, "stderr": "…"}` used by `parse_fixture`.
Helper: `crate::testing::parse_fixture(kind: FixtureKind, path) -> ParsedRun { events: Vec<HarnessEvent>, summary: RunSummary }` (available under `cfg(test)` **and** the `testing` cargo feature so core tests can use it).

Fake harness scripts (POSIX `sh`, executable, in `tests/fakes/`): `fake-claude.sh`, `fake-opencode.sh`, `fake-cli.sh`. Env contract (all read by the script):

| Env | Effect |
|---|---|
| `FAKE_FIXTURE` | path of the file to print to stdout line by line |
| `FAKE_EXIT` | exit code (default 0) |
| `FAKE_SLEEP` | seconds to `sleep` after each line (default 0; fractional allowed if `sleep` supports it) |
| `FAKE_ARGV_OUT` | file to which the script writes its argv, one per line (`printf '%s\n' "$@"`) |
| `FAKE_STDIN_OUT` | file to which the script copies stdin (`cat > "$FAKE_STDIN_OUT"`), done **before** printing; if unset stdin is ignored |
| `FAKE_STDERR` | text printed to stderr before exit |
| `FAKE_HANG=1` | after printing, `trap '' TERM`-free hang: `sleep 300` in a loop (default signal handling, so SIGTERM kills it); `FAKE_HANG=ignoreterm` traps TERM to test the SIGKILL escalation |
| `FAKE_SPAWN_CHILD=1` | starts `sleep 300 &` and records its pid in `FAKE_CHILD_PID_OUT` (tests process-group kill) |

Tests set `ClaudeConfig.binary` / `OpencodeConfig.binary` / the mapping `argv[0]` to the fake script. They never reach the real `claude`/`opencode`.

The ACP fake is `tests/fakes/fake_acp.py` (Python 3, executable, no network). It speaks the
pinned subset from §10 and reads:
`FAKE_ACP_MODE` ∈ {`ok`, `empty`, `no-result`, `malformed`, `unknown-method`, `permission`, `quota`,
`hang`}, `FAKE_ACP_LOG` (JSONL of every client message and its argv) and `FAKE_ACP_CHILD_PID`
(hang mode writes a spawned child pid here, for the process-group test). It never calls a model,
a real ACP server or the network.

### 9.1 Test index (names are defined in `crates/vowp-adapters.md` §9; summary)

- classify: every row of §2.3 and §3.3 table, `parse_reset_time` examples of §5.2, redaction, precedence, status shortcuts, exit gating (talks-about-limit with exit 0 is Ok).
- claude: fixtures → expected event sequences; argv recorded by the fake equals §2.1 exactly (prompt absent from argv, present on stdin); `--resume`/`--session-id`; `rate_limit_event` rejected → Limit with `resets_at`; stop/interrupt kills the process group (child pid dead).
- opencode: fixtures → events (one `tool_use` → 2 events), cost sums over steps, error classes, argv equals §3.1 (prompt last), session id captured; silent hang then `stop()`.
- cli: mapping validation errors (each rule in §4.2), both examples run through fakes, resume substitution, text chunking.
- acp: the fake performs `initialize`/`session/new`/`session/prompt`; event order is `tool.call` → `tool.result` → `tool.call` → `tool.result` → `file.change` → `plan` → one accumulated `assistant.message` → `cost` (tokens, `Unknown`) → one `turn.ended`; the request log asserts the handshake and prompt; a permission request is answered `cancelled` (no option id) and maps to `note` + `Error{Refused}`; an exit without a result, malformed JSON, an unknown method and a JSON-RPC quota error all finish with a classified summary; `stop()` kills the process group.

---

## 10. ACP adapter (`AcpAdapter`, v0.2 first slice)

Status: **first slice implemented**. It drives a fake ACP server through one prompt and the
adapter-contract tests above; real ACP servers, native resume, a permission UI, live pause and
streaming chunk events are **not** implemented. The design note is
[`v0.2/acp-adapter.md`](v0.2/acp-adapter.md).

### 10.1 Protocol pin and argv

Agent Client Protocol revision **1**, JSON-RPC 2.0 over the child's stdin/stdout. One process per
run = one prompt = one turn.

```
<binary> [<extra_args>...]
```

- Binary: `[harnesses.acp].binary`, default `claude-code-acp` on `PATH`. `probe()` only resolves
  the executable; it does not run a `--version` command because an ACP server waits on JSON-RPC
  stdin.
- The adapter sends `initialize` (id 1) with `protocolVersion: 1`, empty
  `clientCapabilities` (no `fs`, no terminal — VOWP never reads or writes the server's files) and
  `clientInfo {name: "vowp", version: <crate version>}`.
- Then `session/new` (id 2) with the run `cwd` and `mcpServers: []`, recording the result's
  `sessionId` as `RunSummary.native_session_id`.
- Then `session/prompt` (id 3) with one `{type:"text", text}` content block. ACP returns the
  agent's streamed `session/update` notifications on the same stdout before the prompt response.
- `resume` is ignored: `can_resume_native` is false, so core never passes one. The prompt already
  appears in the log as the core-owned `user.message`; adapters never emit framing or user
  events.

### 10.2 Event mapping (pinned subset)

| ACP signal | Emits |
|---|---|
| `session/update` `agent_message_chunk` with a `text` content block | no event; chunks accumulate and become **one** `assistant.message` at turn end |
| `session/update` `tool_call` | `tool.call{call_id: toolCallId, name: kind (else title, else "tool"), input: rawInput}` on first sight |
| `session/update` `tool_call_update` with `status: completed\|failed` | `tool.result{call_id, ok, output: rawOutput (else joined content/newText blocks), exit_code: rawOutput.exitCode}`; then `file.change{path, op}` for `kind` `edit` (Modify), `delete` (Delete) or `move` (Rename) when `locations[0].path` or `rawInput.path/filePath/file_path` is present. A `tool_call` that already carries a completion status emits the call, result and change in that order. |
| `session/update` `plan` | `plan{items: [{text: content, status: pending\|in_progress\|completed → Pending\|InProgress\|Done}]}`; `priority` is ignored |
| `session/update` `agent_thought_chunk`, `available_commands_update`, `current_mode_update`, `usage_update`, … | ignored; the first slice reports no reasoning and does not invent a price from context-window use |
| `session/prompt` result `usage` | one `cost` event at turn end: `Cost {usd: None, tokens, source: Unknown}` (§6) |
| `session/prompt` result `stopReason: end_turn` | `turn.ended{EndTurn}`, outcome `Ok` |
| `… max_tokens` / `… max_turn_requests` | `turn.ended{MaxTurns}`, outcome `Ok` (the harness said so; L2 handles a missing report) |
| `… refusal` | `turn.ended{EndTurn}`, outcome `Ok` (the model completed the turn by refusing) |
| `… cancelled` (not caused by VOWP) | `turn.ended{Interrupted}`, outcome `Killed` |
| `session/request_permission` | §10.3 |

The adapter emits exactly one terminal `turn.ended` on every exit path. It never emits
`session.executor`, `turn.started` or `user.message`; core owns that framing. `raw` follows §7 for
tool calls/results, errors and limits.

### 10.3 Permission rule

A `session/request_permission` request is **never auto-approved**. The adapter:

1. emits `note` ("ACP permission requested for <title>; VOWP does not answer permission dialogs")
   and `error{kind: refused, retryable: false}`;
2. answers the JSON-RPC request with `{"outcome":{"outcome":"cancelled"}}` — no `optionId` is
   selected, and no keystroke is sent;
3. finishes the turn as `Error`/`StopReason::Error` with the one `turn.ended`, without waiting
   for a prompt result the server may never send. The pending permission cannot resume the same
   turn; the inbox-approval fallback from the design note is **planned**, not implemented.

### 10.4 Failure handling and process

- A server that closes stdout without a prompt result, sends malformed JSON, an unknown method
  (notification or request; a request also gets JSON-RPC `-32601`) or a JSON-RPC error response
  produces a classified failed summary — never a hang. The error event precedes `turn.ended`.
  Stderr (4 KiB tail) and non-JSON stdout are passed to `classify_text`; a classified class
  yields the matching `LimitEvent`/outcome, otherwise the failure is `Error` with stop reason
  `Crash` for a missing result or `Error` for a JSON-RPC error.
- The child runs in its own process group with stdin/stdout/stderr piped. Stdout lines are teed
  raw to `event_log_hint` before parsing and capped at 4 MiB; stderr is teed to
  `<event_log_hint>.err`. `stop()` sends SIGTERM to the group, waits 3 s, then SIGKILL;
  `interrupt()` sends SIGINT and escalates after 5 s; `StartInput.timeout` does the same through
  a watchdog. After a prompt result the driver closes stdin, waits 2 s, then TERM/KILL, so a
  server that keeps waiting on stdin does not keep the run alive.
- The first slice does **not** write the `<run-id>.usage.json` process-measurement sidecar that
  `proc::RunHandle` writes; capacity sampling therefore has no usage row for an ACP run until
  that is added.

### 10.5 Capabilities

```rust
AdapterCapabilities {
    can_resume_native: false,
    can_inject_assistant_history: false,
    can_switch_model_in_session: false,
    live_input: false,
    emits_reasoning: false,
    emits_cost_receipts: false,
    reports_resets_at: false,
}
```

### 10.6 Tests

`crates/vowp-adapters/tests/acp.rs` against `tests/fakes/fake_acp.py`: clean event order and
request log, accumulated message, tool call/result/file change, plan, tokens-only cost, clean
stop reason, permission refusal with no approval option, exit-without-result, malformed JSON,
unknown method (+ `-32601`), a JSON-RPC quota error, `stop()` killing the process group, extra
args, probe and capabilities.
