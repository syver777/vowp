//! Mapping-file schema and validation for the generic CLI adapter.
//! Spec: `docs/design/adapters.md` §4.1–§4.2 and `docs/design/crates/vowp-adapters.md` §4.3.
//!
//! A mapping describes one harness: how to build its argv, how to turn its JSON lines (or
//! plain text) into canonical events, how to recognise limit texts and where cost lives.
//! Parsing and validation are separate: [`Mapping::from_toml`] does both, while
//! [`Mapping::validate`] can be called on a hand-built value.

use std::collections::BTreeMap;

use regex::RegexBuilder;
use serde::Deserialize;
use vowp_protocol::{AdapterError, EventType};

/// Deserialised mapping file (`adapters.md` §4.1). Unknown keys are rejected everywhere.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// Harness entry name, used as the `cli:<name>` executor model.
    pub name: String,
    /// Limits key; defaults to [`Mapping::name`].
    #[serde(default)]
    pub provider: Option<String>,
    /// Probe argv; defaults to `[command.argv[0], "--version"]`.
    #[serde(default)]
    pub version_argv: Option<Vec<String>>,
    /// How to launch the harness.
    pub command: CommandSpec,
    /// How to read its output.
    #[serde(default)]
    pub output: OutputSpec,
    /// Event mapping rules, evaluated top to bottom for every JSON line.
    #[serde(default, rename = "map")]
    pub rules: Vec<MapRule>,
    /// Limit text patterns.
    #[serde(default)]
    pub limits: LimitSpec,
    /// JSON pointers carrying cost and token counts.
    #[serde(default)]
    pub cost: CostSpec,
    /// Exit-code rules.
    #[serde(default)]
    pub exit: ExitSpec,
}

/// `[command]` table. Distinct from `vowp_core::CommandSpec`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    /// Required, non-empty argv; `argv[0]` is a name on `PATH` or an absolute path.
    pub argv: Vec<String>,
    /// How the prompt is delivered; defaults to [`PromptVia::Arg`].
    #[serde(default)]
    pub prompt_via: PromptVia,
    /// Resume argv; must contain `{native_id}` exactly once.
    #[serde(default)]
    pub resume_argv: Option<Vec<String>>,
    /// Optional flag for a model value.
    #[serde(default)]
    pub model_flag: Option<String>,
    /// Optional value passed after [`CommandSpec::model_flag`].
    #[serde(default)]
    pub model_arg: Option<String>,
    /// Environment variable names to pass through from the caller's env map.
    #[serde(default)]
    pub env: Vec<String>,
    /// Optional flag that receives the working directory.
    #[serde(default)]
    pub cwd_flag: Option<String>,
}

/// TOML string form: `"stdin"`, `"arg"` or `"flag:<--prompt>"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum PromptVia {
    /// Write the prompt to the child's stdin and close it.
    Stdin,
    /// Append the prompt as the last argument (the default).
    #[default]
    Arg,
    /// Append a flag followed by the prompt.
    Flag(String),
}

impl PromptVia {
    /// Parses the TOML string form.
    pub fn parse(s: &str) -> Result<Self, AdapterError> {
        match s {
            "stdin" => Ok(Self::Stdin),
            "arg" => Ok(Self::Arg),
            other => match other.strip_prefix("flag:") {
                Some(flag) if !flag.is_empty() => Ok(Self::Flag(flag.to_owned())),
                _ => Err(mapping_error(
                    "<mapping>",
                    format!("prompt_via {other:?} must be \"stdin\", \"arg\" or \"flag:<non-empty>\""),
                )),
            },
        }
    }
}

impl TryFrom<String> for PromptVia {
    type Error = AdapterError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// `[output]` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// Output shape; defaults to JSON lines.
    #[serde(default)]
    pub format: OutputFormat,
    /// JSON pointer to the harness-side session id (first match wins).
    #[serde(default)]
    pub native_session_pointer: Option<String>,
    /// `format = "text"`: flush an assistant message every N lines.
    #[serde(default = "default_text_chunk_lines")]
    pub text_chunk_lines: u32,
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            format: OutputFormat::default(),
            native_session_pointer: None,
            text_chunk_lines: default_text_chunk_lines(),
        }
    }
}

/// The default number of text lines in one [`vowp_protocol::AssistantMessage`].
fn default_text_chunk_lines() -> u32 {
    20
}

/// How the harness prints its output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// One JSON object per line.
    #[default]
    Jsonl,
    /// Plain text, chunked into assistant messages.
    Text,
}

/// One `[[map]]` rule. TOML key `match` is a Rust keyword, hence the rename.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapRule {
    /// Condition that selects this rule.
    #[serde(rename = "match")]
    pub match_: Match,
    /// An allowed [`EventType`] string.
    pub emit: String,
    /// Payload field -> JSON pointer, or `=literal`.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// One rule condition. Exactly one of the three forms must be set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// JSON pointer to inspect; empty means the whole line.
    pub pointer: String,
    /// Match when the pointed value equals this string (or bool/number text).
    #[serde(default)]
    pub equals: Option<String>,
    /// Match when the pointed value is a string matching this regex.
    #[serde(default)]
    pub regex: Option<String>,
    /// Match when the pointer exists (`true`) or is absent (`false`).
    #[serde(default)]
    pub exists: Option<bool>,
}

/// `[limits]` table: case-insensitive regex lists per kind.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitSpec {
    /// Credits patterns.
    #[serde(default)]
    pub credits: Vec<String>,
    /// Authentication patterns.
    #[serde(default)]
    pub auth: Vec<String>,
    /// Quota patterns.
    #[serde(default)]
    pub quota: Vec<String>,
    /// Rate-limit patterns.
    #[serde(default)]
    pub rate: Vec<String>,
    /// Context-window patterns.
    #[serde(default)]
    pub context: Vec<String>,
    /// Optional reset-time extraction.
    #[serde(default)]
    pub resets_at: Option<ResetSpec>,
    /// Fixed offset used to interpret clock-time resets.
    #[serde(default)]
    pub tz_offset_minutes: i32,
}

/// How to pull a reset time out of limit text.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetSpec {
    /// First capture group is the time text.
    pub regex: String,
    /// Whether the captured text is Unix seconds or a clock time.
    pub kind: ResetKind,
}

/// Interpretation of a captured reset time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    /// Unix seconds.
    Unix,
    /// Clock time parsed by `vowp_adapters::classify::parse_reset_time`.
    Clock,
}

/// `[cost]` table: JSON pointers evaluated on every line, last value wins.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSpec {
    /// USD amount.
    #[serde(default)]
    pub usd_pointer: Option<String>,
    /// Input tokens.
    #[serde(default)]
    pub input_pointer: Option<String>,
    /// Output tokens.
    #[serde(default)]
    pub output_pointer: Option<String>,
    /// Reasoning tokens.
    #[serde(default)]
    pub reasoning_pointer: Option<String>,
    /// Cache-read tokens.
    #[serde(default)]
    pub cache_read_pointer: Option<String>,
    /// Cache-write tokens.
    #[serde(default)]
    pub cache_write_pointer: Option<String>,
}

/// `[exit]` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitSpec {
    /// Exit codes that mean success.
    #[serde(default = "default_ok_codes")]
    pub ok: Vec<i32>,
    /// Exit codes that mean "limit" even without matching text.
    #[serde(default)]
    pub limit: Vec<i32>,
    /// Whether stderr may signal a limit on an otherwise successful exit.
    #[serde(default = "default_true")]
    pub error_from_stderr: bool,
}

impl Default for ExitSpec {
    fn default() -> Self {
        Self {
            ok: default_ok_codes(),
            limit: Vec::new(),
            error_from_stderr: true,
        }
    }
}

/// Default success exit code: zero.
fn default_ok_codes() -> Vec<i32> {
    vec![0]
}

/// Default for [`ExitSpec::error_from_stderr`].
fn default_true() -> bool {
    true
}

impl Mapping {
    /// Parses and validates a mapping from TOML.
    pub fn from_toml(s: &str) -> Result<Mapping, AdapterError> {
        parse_toml(s, "<inline>")
    }

    /// Validates the mapping, naming `path_for_errors` in every error.
    ///
    /// Rules 1–6 of `adapters.md` §4.2 are checked in order; each error names the
    /// offending value, rule or field.
    pub fn validate(&self, path_for_errors: &str) -> Result<(), AdapterError> {
        let fail = |message: String| mapping_error(path_for_errors, message);

        // 1. name and argv
        if !valid_name(&self.name) {
            return Err(fail(format!("name {:?} must match ^[a-z0-9][a-z0-9-]*$", self.name)));
        }
        if self.command.argv.is_empty() {
            return Err(fail("command.argv must not be empty".to_owned()));
        }
        for (index, arg) in self.command.argv.iter().enumerate() {
            if arg.is_empty() {
                return Err(fail(format!("command.argv[{index}] must not be empty")));
            }
        }
        if let Some(argv) = &self.version_argv {
            if argv.is_empty() {
                return Err(fail("version_argv must not be empty when set".to_owned()));
            }
            for (index, arg) in argv.iter().enumerate() {
                if arg.is_empty() {
                    return Err(fail(format!("version_argv[{index}] must not be empty")));
                }
            }
        }

        // 2. prompt delivery and resume placeholder
        if let PromptVia::Flag(flag) = &self.command.prompt_via {
            if flag.is_empty() {
                return Err(fail("prompt_via flag must not be empty".to_owned()));
            }
        }
        if let Some(argv) = &self.command.resume_argv {
            let count: usize = argv.iter().map(|part| part.matches("{native_id}").count()).sum();
            if count != 1 {
                return Err(fail(format!(
                    "command.resume_argv must contain {{native_id}} exactly once (found {count})"
                )));
            }
            if let Some((index, _)) = argv.iter().enumerate().find(|(_, arg)| arg.is_empty()) {
                return Err(fail(format!("command.resume_argv[{index}] must not be empty")));
            }
        }

        // 3. output shape, pointers, map rules
        if self.output.format == OutputFormat::Text {
            if !self.rules.is_empty() {
                return Err(fail(format!(
                    "[[map]] rules require output.format = \"jsonl\" (found \"text\" with {} rules)",
                    self.rules.len()
                )));
            }
            for (label, pointer) in self.pointers() {
                if let Some(pointer) = pointer {
                    return Err(fail(format!(
                        "{label} {pointer:?} is only valid with output.format = \"jsonl\""
                    )));
                }
            }
        }
        for (label, pointer) in self.pointers() {
            if let Some(pointer) = pointer {
                if !valid_pointer(pointer) {
                    return Err(fail(format!(
                        "{label} {pointer:?} must be a JSON pointer starting with \"/\" or empty"
                    )));
                }
            }
        }
        if self.output.text_chunk_lines == 0 {
            return Err(fail("output.text_chunk_lines must be at least 1".to_owned()));
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let number = index + 1;
            if !valid_pointer(&rule.match_.pointer) {
                return Err(fail(format!(
                    "map rule {number} match.pointer {:?} must be a JSON pointer starting with \"/\" or empty",
                    rule.match_.pointer
                )));
            }
            let conditions = usize::from(rule.match_.equals.is_some())
                + usize::from(rule.match_.regex.is_some())
                + usize::from(rule.match_.exists.is_some());
            if conditions != 1 {
                return Err(fail(format!(
                    "map rule {number} match must set exactly one of equals, regex, exists (found {conditions})"
                )));
            }
            if let Some(pattern) = &rule.match_.regex {
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|error| {
                        fail(format!(
                            "map rule {number} match.regex {pattern:?} does not compile: {error}"
                        ))
                    })?;
            }
            let Some(event_type) = allowed_emit(&rule.emit) else {
                return Err(fail(format!(
                    "map rule {number} emit {:?} is not allowed (allowed: assistant.message, assistant.reasoning, \
                     tool.call, tool.result, file.change, note, error, plan)",
                    rule.emit
                )));
            };
            for (field, spec) in &rule.fields {
                if spec.starts_with('=') {
                    continue;
                }
                if !valid_pointer(spec) {
                    return Err(fail(format!(
                        "map rule {number} field {field:?} pointer {spec:?} must start with \"/\" or be empty"
                    )));
                }
            }
            for field in required_fields(event_type) {
                if !rule.fields.contains_key(*field) {
                    return Err(fail(format!(
                        "map rule {number} emit {:?} requires field {field:?}",
                        rule.emit
                    )));
                }
            }
        }

        // 4. every regex compiles and list bounds hold
        let lists = [
            ("credits", &self.limits.credits),
            ("auth", &self.limits.auth),
            ("quota", &self.limits.quota),
            ("rate", &self.limits.rate),
            ("context", &self.limits.context),
        ];
        for (label, patterns) in lists {
            if patterns.len() > 32 {
                return Err(fail(format!("limits.{label} has {} patterns (max 32)", patterns.len())));
            }
            for (index, pattern) in patterns.iter().enumerate() {
                if pattern.chars().count() > 200 {
                    return Err(fail(format!("limits.{label}[{index}] is longer than 200 characters")));
                }
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|error| fail(format!("limits.{label}[{index}] {pattern:?} does not compile: {error}")))?;
            }
        }
        if let Some(reset) = &self.limits.resets_at {
            RegexBuilder::new(&reset.regex)
                .case_insensitive(true)
                .build()
                .map_err(|error| {
                    fail(format!(
                        "limits.resets_at.regex {:?} does not compile: {error}",
                        reset.regex
                    ))
                })?;
        }

        // 5. exit codes and env variable names (never values)
        for code in self.exit.ok.iter().chain(self.exit.limit.iter()) {
            if !(0..=255).contains(code) {
                return Err(fail(format!("exit codes must be in 0..=255 (found {code})")));
            }
        }
        for (index, name) in self.command.env.iter().enumerate() {
            if looks_like_secret(name) {
                return Err(fail(format!(
                    "command.env[{index}] looks like a secret value; list variable names only"
                )));
            }
            if !valid_env_name(name) {
                return Err(fail(format!(
                    "command.env[{index}] {name:?} must match ^[A-Z_][A-Z0-9_]*$"
                )));
            }
        }

        Ok(())
    }

    /// Every optional JSON pointer with its config path, for shared validation.
    fn pointers(&self) -> Vec<(&'static str, &Option<String>)> {
        vec![
            ("output.native_session_pointer", &self.output.native_session_pointer),
            ("cost.usd_pointer", &self.cost.usd_pointer),
            ("cost.input_pointer", &self.cost.input_pointer),
            ("cost.output_pointer", &self.cost.output_pointer),
            ("cost.reasoning_pointer", &self.cost.reasoning_pointer),
            ("cost.cache_read_pointer", &self.cost.cache_read_pointer),
            ("cost.cache_write_pointer", &self.cost.cache_write_pointer),
        ]
    }

    /// Turns empty optional strings into `None` before validation.
    fn normalise(&mut self) {
        fn none_if_empty(value: &mut Option<String>) {
            if value.as_deref() == Some("") {
                *value = None;
            }
        }

        none_if_empty(&mut self.provider);
        none_if_empty(&mut self.command.model_flag);
        none_if_empty(&mut self.command.model_arg);
        none_if_empty(&mut self.command.cwd_flag);
        none_if_empty(&mut self.output.native_session_pointer);
        none_if_empty(&mut self.cost.usd_pointer);
        none_if_empty(&mut self.cost.input_pointer);
        none_if_empty(&mut self.cost.output_pointer);
        none_if_empty(&mut self.cost.reasoning_pointer);
        none_if_empty(&mut self.cost.cache_read_pointer);
        none_if_empty(&mut self.cost.cache_write_pointer);
    }
}

/// Parses and validates `text`, naming `path` in parse and validation errors.
pub(super) fn parse_toml(text: &str, path: &str) -> Result<Mapping, AdapterError> {
    let mut mapping: Mapping = toml::from_str(text).map_err(|error| mapping_error(path, error.to_string()))?;
    mapping.normalise();
    mapping.validate(path)?;
    Ok(mapping)
}

/// Builds a mapping error.
fn mapping_error(path: &str, message: impl Into<String>) -> AdapterError {
    AdapterError::Mapping {
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Returns the event type for an allowed `emit` value.
pub(super) fn allowed_emit(name: &str) -> Option<EventType> {
    Some(match name {
        "assistant.message" => EventType::AssistantMessage,
        "assistant.reasoning" => EventType::AssistantReasoning,
        "tool.call" => EventType::ToolCall,
        "tool.result" => EventType::ToolResult,
        "file.change" => EventType::FileChange,
        "note" => EventType::Note,
        "error" => EventType::Error,
        "plan" => EventType::Plan,
        _ => return None,
    })
}

/// Payload fields that must be present for each allowed event type.
pub(super) fn required_fields(event_type: EventType) -> &'static [&'static str] {
    match event_type {
        EventType::AssistantMessage | EventType::AssistantReasoning | EventType::Note => &["text"],
        EventType::ToolCall => &["call_id", "name", "input"],
        EventType::ToolResult => &["call_id", "ok", "output"],
        EventType::FileChange => &["path", "op"],
        // `kind` is not listed here: parser::build_event defaults it to `adapter_failed`.
        EventType::Error => &["message"],
        EventType::Plan => &["items"],
        _ => &[],
    }
}

/// Returns true for `^[a-z0-9][a-z0-9-]*$` without compiling a regex.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Returns true for `^[A-Z_][A-Z0-9_]*$`.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Returns true for `""` or an RFC 6901 pointer starting with `/`.
fn valid_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

/// Returns true when an env entry looks like a secret value rather than a variable name.
fn looks_like_secret(value: &str) -> bool {
    ["sk-", "ghp_", "github_pat_", "AKIA", "xox", "-----BEGIN", "Bearer "]
        .iter()
        .any(|marker| value.contains(marker))
}
