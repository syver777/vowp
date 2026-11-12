# crate `vowp-adapters`

Behaviour of each adapter is specified in `adapters.md`; this file is the **API, module layout and tests**. Depends only on `vowp-protocol`
(+ `serde`, `serde_json`, `thiserror`, `toml`, `regex`; dev: `tempfile`, `pretty_assertions`). No `unsafe`, no tokio, no network.

## 1. Modules

The shared modules (`classify`, `proc`, `stream`, `testing`) are the foundation for the four adapter modules.

```
crates/vowp-adapters/
  Cargo.toml
  src/lib.rs                      (final form below)
  src/classify/mod.rs             classify_text, parse_reset_time, redact, ...      (adapters.md §5)
  src/proc/mod.rs                 process-group spawn, reader threads, signals, RunHandle
  src/testing.rs                  parse_fixture, FixtureKind (cfg(test) or feature "testing")
  src/stream.rs                   StreamParser trait, ParsedRun
  src/opencode/mod.rs             (+ submodules src/opencode/*.rs)
  src/claude_code/mod.rs          (+ src/claude_code/*.rs)
  src/cli/mod.rs                  (+ src/cli/*.rs)
  src/acp/mod.rs                  ACP adapter, config and run (+ mapping.rs, run.rs)  (adapters.md §10)
  tests/fixtures/claude_*.jsonl           tests/fixtures/opencode_*.jsonl         tests/fixtures/cli_*
  tests/fakes/fake-*.sh           claude/opencode/cli, written once from adapters.md §9
  tests/fakes/fake_acp.py         ACP JSON-RPC fake                                  (adapters.md §10.6)
  tests/*.rs                      claude_code.rs, opencode.rs, cli.rs, acp.rs, classify.rs, proc.rs
```

Final `src/lib.rs`:

```rust
//! Harness adapters: claude-code, opencode, generic cli and ACP. Spec: docs/design/adapters.md
#![forbid(unsafe_code)]
#![warn(missing_docs)]
pub mod acp;
pub mod classify;
pub mod proc;
pub mod stream;
pub mod claude_code;
pub mod opencode;
pub mod cli;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use acp::{ACP_PROTOCOL_VERSION, AcpAdapter, AcpConfig};
pub use claude_code::{ClaudeCodeAdapter, ClaudeConfig};
pub use opencode::{OpencodeAdapter, OpencodeConfig};
pub use cli::{CliAdapter, Mapping};
```

Cargo feature `testing = []` in `Cargo.toml`.

## 2. `stream` — parser trait

```rust
/// Incremental, process-free parser of one harness's stdout. Adapters own one per run. Pure: no I/O, no clock except via the `now` given.
pub trait StreamParser: Send {
    /// Feed one stdout line (no trailing newline). Returns events in order; may return none.
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent>;
    /// Feed one stderr line (default: ignore; parsers keep a 4 KiB tail for classification).
    fn feed_stderr(&mut self, _line: &str) {}
    /// End of stream. `exit` is None when killed by a signal; `killed` is true if the caller requested stop/interrupt/timeout.
    /// Returns the trailing events (Error/Limit/Cost/TurnEnded) and the classified summary. Consumes the parser.
    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary);
}

/// Result of parsing a whole recorded run (tests, importer).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRun { pub events: Vec<HarnessEvent>, pub summary: RunSummary }
```

(`finish` returns the terminal events too; the shared `proc::RunHandle` appends them to the channel. This refines the one-line sketch in the brief: `finish(self, exit, stderr_tail) -> RunSummary`.)

## 3. `proc` — process plumbing

```rust
/// How to launch one child.
pub struct SpawnSpec {
    pub argv: Vec<String>,                 // argv[0] = resolved absolute path
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,        // ALREADY filtered: base env (PATH,HOME,...) + caller's env_pass; the child's env is cleared first
    pub stdin: Option<Vec<u8>>,            // written then closed; None = Stdio::null()
    pub tee_stdout: Option<std::path::PathBuf>,   // StartInput.event_log_hint: raw stdout lines appended + flushed per line
    pub tee_stderr: Option<std::path::PathBuf>,   // same path with ".err" appended
    pub timeout: Option<std::time::Duration>,
}

/// A running child with reader threads. Implements `AdapterRun` generically over a boxed `StreamParser`.
pub struct RunHandle { /* child, pgid, rx: Receiver<HarnessEvent>, waiter: JoinHandle<..>, state: Arc<Mutex<..>> */ }
impl RunHandle {
    pub fn spawn(spec: SpawnSpec, parser: Box<dyn StreamParser>) -> Result<RunHandle, AdapterError>;
}
impl vowp_protocol::AdapterRun for RunHandle {}   // pid/events/send(Unsupported)/interrupt/stop/wait as in adapters.md §1

/// Base environment for children: PATH, HOME, USER, LANG, LC_ALL, TERM=dumb, TMPDIR, XDG_RUNTIME_DIR, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_CACHE_HOME
/// (only those present in the current process), then `extra`.
pub fn child_env(extra: &[(String, String)]) -> Vec<(String, String)>;

/// Find `name` on PATH (or accept an absolute/relative path that exists and is executable). No `which` crate.
pub fn resolve_binary(name_or_path: &str) -> Option<std::path::PathBuf>;

/// Run a short command to completion (probe): returns (exit code, stdout, stderr), killing the process group after `timeout`.
pub fn run_capture(argv: &[String], timeout: std::time::Duration) -> Result<(Option<i32>, String, String), AdapterError>;

/// Signal a process group. Implemented with `Command::new("kill").arg("-TERM"|"-INT"|"-KILL").arg("--").arg(format!("-{pgid}"))`.
pub fn signal_group(pgid: u32, sig: Signal) -> std::io::Result<()>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Signal { Int, Term, Kill }
```

**No `unsafe`, no `libc`/`nix`.** Signals to a process group are sent by spawning `/bin/kill` with a negative pid. Tradeoffs (accepted for v0.1): one extra fork per signal;
`kill` must exist on `PATH` (always true on Linux/macOS); if the group already exited `kill` fails with "No such process", treated as success. `process_group(0)` makes the child its own group leader
(stable `std::os::unix::process::CommandExt`), so `pgid == child pid`. The waiter thread uses `Child::wait` (blocking). Signals are never sent to pid `0`/`1` or a pgid equal to our own group
(`signal_group` returns `InvalidInput` for pgid ≤ 1 or == own pgid).

Reader/waiter rules: stdout reader uses `BufRead::read_until(b'\n')` (lossy UTF-8; lines > 4 MiB are truncated to 4 MiB with a Note event) and sends events to the channel with `send` ignoring `SendError`
(receiver dropped ⇒ keep draining and tee). Order guarantee: all stdout events precede terminal events. `wait()` joins readers before calling `parser.finish`.

## 4. Public adapter API

### 4.1 claude-code

```rust
#[derive(Debug, Clone, Default)]
pub struct ClaudeConfig {
    pub binary: Option<String>,            // default: resolve_binary("claude")
    pub extra_args: Vec<String>,           // appended after VOWP's flags, before the prompt
    pub add_dirs: Vec<std::path::PathBuf>, // --add-dir
    pub permission_mode: Option<String>,   // default "auto" (StartInput.permission_mode overrides)
    pub prompt_via_arg: bool,              // default false = stdin
    pub tz_offset_minutes: i32,            // for clock-time reset texts (adapters.md §2.4)
}
pub struct ClaudeCodeAdapter { /* config */ }
impl ClaudeCodeAdapter { pub fn new(cfg: ClaudeConfig) -> Self; }
impl vowp_protocol::Adapter for ClaudeCodeAdapter {}

/// Pure helpers (public for tests):
pub fn build_argv(cfg: &ClaudeConfig, input: &StartInput, binary: &str) -> Vec<String>;        // adapters.md §2.1, without the prompt when via stdin
pub struct ClaudeParser { /* now: Timestamp, tz_offset_minutes, model, native id, last rate info, pending limit, usage ... */ }
impl ClaudeParser { pub fn new(now: Timestamp, tz_offset_minutes: i32, executor: Executor) -> Self; }
impl StreamParser for ClaudeParser {}
```

### 4.2 opencode

```rust
#[derive(Debug, Clone, Default)]
pub struct OpencodeConfig {
    pub binary: Option<String>,            // default resolve_binary("opencode") then $HOME/.opencode/bin/opencode
    pub extra_args: Vec<String>,
    pub title: Option<String>,             // --title (core: "vowp <role> <job>")
    pub tz_offset_minutes: i32,
    pub max_arg_prompt_bytes: usize,       // default 100_000: larger prompts go through .vowp/prompts/<file>
}
pub struct OpencodeAdapter { /* config */ }
impl OpencodeAdapter { pub fn new(cfg: OpencodeConfig) -> Self; }
impl vowp_protocol::Adapter for OpencodeAdapter {}
pub fn build_argv(cfg: &OpencodeConfig, input: &StartInput, binary: &str, prompt_arg: &str) -> Vec<String>;   // adapters.md §3.1
pub struct OpencodeParser { /* now, tz, executor, session id, step count, cost sum, error lines, ... */ }
impl OpencodeParser { pub fn new(now: Timestamp, tz_offset_minutes: i32, executor: Executor) -> Self; }
impl StreamParser for OpencodeParser {}
```

### 4.3 generic cli

```rust
/// Deserialised mapping file (adapters.md §4.1). `deny_unknown_fields` everywhere.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct Mapping { pub name: String, #[serde(default)] pub provider: Option<String>, #[serde(default)] pub version_argv: Option<Vec<String>>,
    pub command: CommandSpec, #[serde(default)] pub output: OutputSpec, #[serde(default, rename = "map")] pub rules: Vec<MapRule>,
    #[serde(default)] pub limits: LimitSpec, #[serde(default)] pub cost: CostSpec, #[serde(default)] pub exit: ExitSpec }
/// `[command]` table. (Distinct from `vowp_core::CommandSpec`; lives in `vowp_adapters::cli`.)
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub argv: Vec<String>,                                   // required, non-empty
    #[serde(default)] pub prompt_via: PromptVia,             // default Arg
    #[serde(default)] pub resume_argv: Option<Vec<String>>,  // contains "{native_id}" exactly once
    #[serde(default)] pub model_flag: Option<String>,
    #[serde(default)] pub model_arg: Option<String>,
    #[serde(default)] pub env: Vec<String>,                  // env var NAMES
    #[serde(default)] pub cwd_flag: Option<String>,
}
/// TOML string form: "stdin" | "arg" | "flag:<--prompt>".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PromptVia { Stdin, #[default] Arg, Flag(String) }
impl PromptVia { pub fn parse(s: &str) -> Result<Self, AdapterError>; }   // + serde Deserialize via try_from = "String"

/// `[output]` table.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct OutputSpec {
    #[serde(default)] pub format: OutputFormat,                          // default Jsonl
    #[serde(default)] pub native_session_pointer: Option<String>,
    #[serde(default = "default_text_chunk_lines")] pub text_chunk_lines: u32,   // 20
}
impl Default for OutputSpec { /* Jsonl, None, 20 */ }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)] #[serde(rename_all = "snake_case")]
pub enum OutputFormat { #[default] Jsonl, Text }

/// One `[[map]]` rule. TOML key `match` is a Rust keyword, hence the rename.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct MapRule {
    #[serde(rename = "match")] pub match_: Match,
    pub emit: String,                                                    // an EventType string (allowed list: adapters.md §4.1)
    #[serde(default)] pub fields: std::collections::BTreeMap<String, String>,   // payload field -> JSON pointer, or "=literal"
}
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct Match { pub pointer: String, #[serde(default)] pub equals: Option<String>, #[serde(default)] pub regex: Option<String>, #[serde(default)] pub exists: Option<bool> }
// exactly one of equals / regex / exists must be set (validate()).

/// `[limits]` table: case-insensitive regex lists per kind.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct LimitSpec {
    #[serde(default)] pub credits: Vec<String>, #[serde(default)] pub auth: Vec<String>, #[serde(default)] pub quota: Vec<String>,
    #[serde(default)] pub rate: Vec<String>, #[serde(default)] pub context: Vec<String>,
    #[serde(default)] pub resets_at: Option<ResetSpec>,
    #[serde(default)] pub tz_offset_minutes: i32,
}
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct ResetSpec { pub regex: String /* first capture group = the time text */, pub kind: ResetKind }
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)] #[serde(rename_all = "snake_case")]
pub enum ResetKind { Unix, Clock }

/// `[cost]` table: JSON pointers evaluated on every line, last value wins; empty string = not set.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct CostSpec {
    #[serde(default)] pub usd_pointer: Option<String>, #[serde(default)] pub input_pointer: Option<String>,
    #[serde(default)] pub output_pointer: Option<String>, #[serde(default)] pub reasoning_pointer: Option<String>,
    #[serde(default)] pub cache_read_pointer: Option<String>, #[serde(default)] pub cache_write_pointer: Option<String>,
}
/// `[exit]` table.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)] #[serde(deny_unknown_fields)]
pub struct ExitSpec { #[serde(default = "default_ok_codes")] pub ok: Vec<i32> /* [0] */, #[serde(default)] pub limit: Vec<i32>,
                      #[serde(default = "default_true")] pub error_from_stderr: bool /* true */ }
impl Default for ExitSpec { /* ok [0], limit [], error_from_stderr true */ }
// An empty string for any Option<String> pointer is normalised to None by `Mapping::from_toml` before validation.
impl Mapping {
    pub fn from_toml(s: &str) -> Result<Mapping, AdapterError>;                      // parse + validate
    pub fn validate(&self, path_for_errors: &str) -> Result<(), AdapterError>;       // adapters.md §4.2
}
pub struct CliAdapter { /* mapping, binary resolved lazily, env */ }
impl CliAdapter {
    pub fn from_mapping_file(path: &std::path::Path) -> Result<CliAdapter, AdapterError>;
    pub fn from_mapping(m: Mapping) -> Result<CliAdapter, AdapterError>;
    pub fn mapping(&self) -> &Mapping;
}
impl vowp_protocol::Adapter for CliAdapter {}
pub struct CliParser { /* mapping clone, compiled regexes, lines buffered, native id, cost accumulators */ }
impl CliParser { pub fn new(mapping: Mapping, now: Timestamp) -> Result<Self, AdapterError>; }
impl StreamParser for CliParser {}
/// Evaluate a JSON pointer ("/a/0/b"; "" = whole value) on a value. Also used by tests.
pub fn pointer_get<'a>(v: &'a serde_json::Value, ptr: &str) -> Option<&'a serde_json::Value>;
```

`AdapterRegistry` (name → `Arc<dyn Adapter>` from `VowpConfig`) is **not** here: it lives in `vowp-core::deps`, which constructs adapters from `[harnesses.*]`.

### 4.4 ACP

```rust
/// ACP protocol revision 1, sent in `initialize`.
pub const ACP_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct AcpConfig {
    pub binary: Option<String>,     // default resolve_binary("claude-code-acp")
    pub extra_args: Vec<String>,
    pub tz_offset_minutes: i32,
}
pub struct AcpAdapter { /* config */ }
impl AcpAdapter { pub fn new(cfg: AcpConfig) -> Self; }
impl vowp_protocol::Adapter for AcpAdapter {}
```

ACP is JSON-RPC 2.0 on the child's stdin/stdout, so the adapter owns a small driver rather than a
`StreamParser`: `src/acp/run.rs` performs `initialize`, `session/new` and `session/prompt`, keeps the
process-group, stop/interrupt/timeout and 4 MiB/4 KiB reader rules of `proc::RunHandle`, and
`src/acp/mapping.rs` is the pure event mapper. The pinned subset, permission rule and capabilities
are in adapters.md §10.

## 5. Optional importer

`pub fn import_transcript(path: &Path, tz_offset_minutes: i32) -> Result<Vec<HarnessEvent>, AdapterError>` is re-exported as `claude_code::import_transcript` (implementation in `claude_code::import`). It reads a `~/.claude/projects/…/<uuid>.jsonl`
transcript with the same block mapping as §2.2 of adapters.md (no result/cost lines). Used later by `vowp session watch` (cli.md). Not required for v0.1.

## 6. Errors

`vowp_protocol::AdapterError` (types.md §12) is the only error type exported; `classify` functions are infallible. Mapping errors carry `path` and a human message (rule index for map rules).

## 7. Named tests

All hermetic: temp dirs, the fake scripts, recorded fixtures. Names are exact.

### classify / proc (`tests/classify.rs`, `tests/proc.rs`, unit tests inside the modules)

| Test | Proves |
|---|---|
| `classify_claude_usage_limit_with_unix_suffix` | `Claude AI usage limit reached\|1794164400` → Quota, `resets_at = 2026-11-08T19:00:00Z` (exact, no margin) |
| `classify_claude_hit_limit_clock_time_with_tz_offset` | `You've hit your limit · resets 11pm (Europe/Oslo)`, now 18:10Z, offset 60 → `22:00Z` same day (exact, no margin) |
| `classify_clock_time_rolls_to_next_day` | `resets 3pm` after 3pm → tomorrow |
| `classify_five_hour_and_weekly_and_minutes` | `5-hour limit reached ∙ resets 3pm`, `weekly limit · resets Nov 12, 9am`, `resets 4:30 am` all parsed |
| `classify_rate_529_429_overloaded` | Rate with `resets_at = now + 10 min` |
| `classify_credits_402_insufficient_balance` | Credits, no `resets_at`; beats Rate/Quota when both match |
| `classify_auth_texts` | `Invalid API key · Please run /login`, `Not logged in`, 401/403 status |
| `classify_context_texts` | `prompt is too long: 210000 tokens > 200000 maximum` → Context |
| `classify_precedence_credits_auth_quota_rate_context` | table-driven: strings matching several classes pick the documented winner |
| `classify_unrelated_text_is_none` | `"all tests passed"` → None; JSON key `rate_limit_event` in a message is not fed (documented) |
| `reset_from_rate_limit_info_picks_max_rejected_window` | sample object with five_hour rejected & seven_day allowed → five_hour resetsAt exactly; both rejected → max |
| `redact_removes_secret_shapes_and_truncates` | each pattern in adapters.md §5.4 → `<redacted>`; output ≤ 300 chars; char-boundary safe on multibyte input |
| `extract_http_status_variants` | `"statusCode":402`, `HTTP 401`, `API Error: 529` |
| `proc_spawn_collects_events_and_exit` | fake script prints 3 lines, parser stub emits events, channel order, `wait()` summary, channel closes |
| `proc_tees_raw_stdout_before_parsing` | tee file equals script stdout byte for byte, including a non-JSON line |
| `proc_stop_is_idempotent_and_kills_group` | `FAKE_HANG=1` + `FAKE_SPAWN_CHILD=1`: after `stop()` both the child and the grandchild pid are dead; second `stop()` is `Ok`; outcome `Killed` |
| `proc_stop_escalates_to_sigkill` | `FAKE_HANG=ignoreterm`: dies within 5 s |
| `proc_interrupt_then_stop` | SIGINT, then forced stop after 5 s if still alive (timing scaled by an injectable `grace` field in `SpawnSpec` test builder) |
| `proc_timeout_kills_and_marks_killed` | `timeout = 1s`, hang → `Killed` |
| `proc_env_is_filtered` | child printing `env` shows only base + passed names; a canary var set in the test process is absent |
| `proc_spawn_failure_is_typed_error` | missing binary → `AdapterError::Spawn`/`Unavailable`, no panic |
| `resolve_binary_finds_on_path_and_rejects_non_executable` | temp dir PATH |
| `signal_group_refuses_pid_0_1_and_own_group` | returns `InvalidInput` |

### claude-code (`tests/claude_code.rs`)

| Test | Proves |
|---|---|
| `claude_probe_parses_version` | fake `--version` prints `2.1.319 (Claude Code)` → `ok`, version `2.1.319`; missing binary → `ok:false` |
| `claude_argv_new_session` | recorded argv equals §2.1 with `--session-id <uuid>`, no `--resume`, no prompt in argv |
| `claude_argv_resume_effort_system_note` | `--resume <id>`, `--effort medium`, `--append-system-prompt`, `--permission-mode auto` default, extra args order |
| `claude_prompt_goes_to_stdin` | `FAKE_STDIN_OUT` file equals the prompt exactly (incl. multibyte, > 200 KB prompt works) |
| `claude_prompt_via_arg_fallback` | `prompt_via_arg=true` → last argv element |
| `claude_ok_fixture_events` | `claude_ok.jsonl` → `AssistantMessage("ok")`, `CostEvent{usd 0.0349033, tokens input 2/output 4/cache_read 11873/cache_write 8418}`, `TurnEnded(EndTurn)`; no limit event for `allowed` rate event; no framing events; summary outcome Ok and `summary.native_session_id == Some("607b6f5f-88b0-4133-8716-9270d9d52c99")` |
| `claude_tool_use_fixture_events` | `ToolCall` + `ToolResult` for both string and array-of-blocks content; empty thinking produces no `AssistantReasoning`; signatures absent from all events and raw |
| `claude_rate_limit_rejected_fixture` | `LimitEvent{Quota, provider claude, resets_at = resetsAt exactly}`, `RunSummary.outcome = Limit`, `limit.resets_at` equal, stop_reason `Limit`, even with exit 0 |
| `claude_usage_limit_text_fixture` | is_error result text → Limit with unix reset |
| `claude_weekly_limit_fixture` | date form with tz offset |
| `claude_429_fixture` | Rate, `resets_at = now+10min` using the injected `now` |
| `claude_auth_fixture` | `RunOutcome::Auth`, `LimitKind::Auth`, no `resets_at` |
| `claude_limit_words_in_assistant_text_are_not_a_limit` | assistant text "you may hit your usage limit" + exit 0 + `is_error:false` → Ok |
| `claude_no_result_line_is_crash_or_error` | truncated fixture, exit 137 → `Crash`; exit 0 → `Error("no result line")` |
| `claude_subtype_error_max_turns` | `MaxTurns`, outcome Ok |
| `claude_unknown_message_types_ignored_but_teed` | unknown `type` yields no events; tee file contains it |
| `claude_run_end_to_end_with_fake_script` | `ClaudeCodeAdapter::start` with `binary = fake-claude.sh`; events from channel; `wait()` summary; `native_session_id` captured |
| `claude_stop_kills_hung_run` | `FAKE_HANG=1`: `stop()` → terminal `TurnEnded(Interrupted)` emitted, outcome Killed |
| `claude_cost_thinking_tokens_split` | `output = output_tokens − thinking`, `reasoning = thinking`, total equals harness |
| `claude_raw_cap_and_redaction` | 40 KB tool result → raw truncated marker; a fake `sk-…` in tool output absent from `raw` |

### opencode (`tests/opencode.rs`)

| Test | Proves |
|---|---|
| `opencode_probe_parses_version` | like claude (`2.0.28`) |
| `opencode_argv_new_and_resume` | `run --format json --auto --model <executor.model_arg()> [--session ses_…] [--title …] <prompt last>`; stdin is null |
| `opencode_tool_exit_code_from_nested_metadata` | fixture `opencode_tool_exit.jsonl` (real nested shape `state.metadata.metadata.exit`): `ToolResult.exit_code == Some(0)` and `Some(2)` for the second line |
| `opencode_variant_sigil_only_when_present` | `#max` absent when `variant: None` |
| `opencode_long_prompt_goes_through_file` | > 100 000 bytes → file under `<cwd>/.vowp/prompts/`, argv prompt is the pointer text |
| `opencode_ok_fixture_events` | real fixture: no `TurnStarted`/`SessionExecutor`, `ToolCall`+`ToolResult` per `tool_use`, `AssistantMessage`, one `CostEvent` per `step_finish`, exactly one `TurnEnded(EndTurn)`; `summary.native_session_id` = the fixture's `ses_…` id |
| `opencode_tool_use_line_becomes_two_events_in_order` | call before result, same `call_id`, ts from `state.time` |
| `opencode_tool_error_is_not_run_error` | `state.status:"error"` → `ToolResult{ok:false, output "Web search cancelled"}`, outcome Ok |
| `opencode_cost_sums_over_steps` | `RunSummary.cost.usd` = sum of `part.cost`, tokens summed incl. cache read, `source: Receipt` |
| `opencode_no_receipt_is_unknown_cost` | run with no `step_finish` → `Cost::unknown()` (usd None) |
| `opencode_402_fixture_is_credits` | outcome Credits, `LimitEvent{Credits, provider <executor provider>}`, exit non-zero |
| `opencode_429_fixture_is_rate` | Rate + 10 min |
| `opencode_auth_fixture` | Auth |
| `opencode_context_fixture` | `LimitEvent{Context}` + `ErrorEvent` |
| `opencode_error_text_in_assistant_message_is_not_a_limit` | exit 0 → Ok |
| `opencode_silent_run_then_stop` | `FAKE_HANG=1` with no output: adapter does not time out by itself; `stop()` → Killed |
| `opencode_end_to_end_with_fake_script` | start/wait with `fake-opencode.sh` and `opencode_ok.jsonl` |

### cli (`tests/cli.rs`)

| Test | Proves |
|---|---|
| `mapping_valid_examples_load` | both examples from adapters.md §4.4–4.5 parse and validate |
| `mapping_rejects_each_invalid_rule` | table-driven, one case per rule 1–6 of §4.2 with the message naming the offender |
| `mapping_rejects_unknown_keys_and_secret_looking_env_values` | `deny_unknown_fields`; `sk-…` rejected |
| `cli_jsonl_codex_like_events` | `cli_codex_like.jsonl` → assistant message, tool call/result, native session id via pointer, `Cost` Receipt only if usd pointer set |
| `cli_missing_required_field_skips_rule_with_note` | `Note("mapping: rule 1 missing call_id")` emitted, other rules still fire |
| `cli_literal_field_values` | `"=shell"` / `"=true"` literals typed correctly |
| `cli_text_format_chunks_every_n_lines_and_at_eof` | 45 lines, chunk 20 → 3 messages |
| `cli_limit_regexes_by_precedence` | credits beats rate; resets_at unix extraction |
| `cli_exit_codes_ok_and_limit` | exit rules respected |
| `cli_resume_argv_substitution` | `{native_id}` replaced; `can_resume_native` true only when `resume_argv` set |
| `cli_prompt_via_stdin_arg_flag` | each mode recorded by the fake |
| `cli_end_to_end_with_fake_script` | start/wait with `fake-cli.sh` |
| `cli_pointer_get_cases` | `/a/0/b`, `""`, missing, escaped `~0 ~1` |

### acp (`tests/acp.rs`)

| Test | Proves |
|---|---|
| `acp_probe_reports_the_binary_without_a_version_call` | found executable → `ok`, `version == None`; missing configured path → `ok == false` |
| `acp_capabilities_are_honest` | resume/live-input/reasoning/receipts are all false |
| `acp_clean_turn_emits_events_in_order` | `tool.call`, `tool.result`, `tool.call`, `tool.result`, `file.change`, `plan`, one accumulated `assistant.message`, tokens-only `cost`, one `turn.ended`; requests are `initialize`/`session/new`/`session/prompt` with version 1, cwd and prompt text |
| `acp_permission_request_is_never_approved` | `note` + `Error{Refused}`, the JSON-RPC answer is `cancelled` with no option id, outcome `Error` |
| `acp_server_exiting_without_a_result_fails_cleanly` | one `Error`, stop reason `Crash`, no hang |
| `acp_malformed_json_fails_cleanly` | one `Error` naming malformed JSON |
| `acp_unknown_method_fails_cleanly_and_answers_the_request` | one `Error`, the agent request gets JSON-RPC `-32601` |
| `acp_rpc_error_classifies_a_quota_limit` | `LimitEvent{Quota, resets_at 1794164400}`, outcome `Limit`, stop reason `Limit` |
| `acp_stop_kills_the_process_group` | `stop()` returns, the spawned child is dead, one `Interrupted` terminal event |
| `acp_extra_args_follow_the_binary` | configured `extra_args` reach argv |

### Cross-adapter (in `vowp-core` tests, listed here so the contract is covered)

`adapters_emit_terminal_turn_ended_on_every_exit_path` (claude, opencode, cli and acp × {ok, error exit, killed, spawn fail after start}) — a parametrised test in `crates/vowp-core/tests/adapter_contract.rs`; the ACP fake selects its scenario through `FAKE_ACP_MODE`.
