//! Adapter for the opencode CLI harness (`opencode run --format json`).
//! Spec: `docs/design/adapters.md` §3 and `docs/design/crates/vowp-adapters.md` §4.2.
//!
//! One run is one prompt in one process. [`build_argv`] is pure and public for tests;
//! [`OpencodeAdapter::start`] resolves the binary, spills oversized prompts to
//! `.vowp/prompts/`, spawns the child through [`crate::proc::RunHandle`] and lets
//! [`OpencodeParser`] map its JSON lines to canonical events.

mod parser;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use regex::Regex;
use vowp_protocol::{
    Adapter, AdapterCapabilities, AdapterError, AdapterRun, HarnessKind, ProbeResult, StartInput, Timestamp,
};

use crate::proc::{RunHandle, SpawnSpec, child_env, resolve_binary, run_capture};
pub use parser::OpencodeParser;

/// Default prompt size above which the prompt is passed through a file.
const DEFAULT_MAX_PROMPT_BYTES: usize = 100_000;

/// Probe commands are cheap; they are killed after this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-wide counter for prompt file names.
static PROMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`OpencodeAdapter`].
#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    /// Binary to run; defaults to `opencode` on `PATH`, then `$HOME/.opencode/bin/opencode`.
    pub binary: Option<String>,
    /// Extra arguments appended after VOWP's flags, before the prompt.
    pub extra_args: Vec<String>,
    /// Optional `--title` value, usually `"vowp <role> <job_id>"`.
    pub title: Option<String>,
    /// Fixed offset used to interpret clock-time reset texts.
    pub tz_offset_minutes: i32,
    /// Prompts larger than this go through a file under `<cwd>/.vowp/prompts/`.
    pub max_arg_prompt_bytes: usize,
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            binary: None,
            extra_args: Vec::new(),
            title: None,
            tz_offset_minutes: 0,
            max_arg_prompt_bytes: DEFAULT_MAX_PROMPT_BYTES,
        }
    }
}

/// The opencode adapter.
pub struct OpencodeAdapter {
    config: OpencodeConfig,
}

impl OpencodeAdapter {
    /// Creates an adapter from `config`.
    pub fn new(config: OpencodeConfig) -> Self {
        Self { config }
    }
}

impl Adapter for OpencodeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        let Some(binary) = resolve_config_binary(&self.config) else {
            return ProbeResult {
                ok: false,
                version: None,
                path: None,
                detail: "binary not found on PATH".to_owned(),
            };
        };

        let argv = vec![binary.to_string_lossy().into_owned(), "--version".to_owned()];
        match run_capture(&argv, PROBE_TIMEOUT) {
            Ok((exit, stdout, stderr)) => {
                let version = version_regex()
                    .captures(&stdout)
                    .and_then(|captures| captures.get(1))
                    .map(|value| value.as_str().to_owned());
                let ok = exit == Some(0) && version.is_some();
                let detail = if ok {
                    format!("opencode {stdout}", stdout = stdout.trim())
                } else if !stderr.trim().is_empty() {
                    format!("version probe failed: {}", stderr.trim())
                } else {
                    format!("version probe exited with {exit:?}")
                };
                ProbeResult {
                    ok,
                    version,
                    path: Some(binary.to_string_lossy().into_owned()),
                    detail,
                }
            }
            Err(error) => ProbeResult {
                ok: false,
                version: None,
                path: Some(binary.to_string_lossy().into_owned()),
                detail: format!("version probe failed: {error}"),
            },
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: true,
            can_inject_assistant_history: false,
            can_switch_model_in_session: true,
            live_input: false,
            emits_reasoning: false,
            emits_cost_receipts: true,
            reports_resets_at: false,
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let Some(binary) = resolve_config_binary(&self.config) else {
            let detail = match &self.config.binary {
                Some(value) => format!("opencode binary not found: {value}"),
                None => "opencode binary not found on PATH".to_owned(),
            };
            return Err(AdapterError::Unavailable(detail));
        };

        let prompt_arg = prompt_argument(&self.config, &input)?;
        let argv = build_argv(&self.config, &input, binary.to_string_lossy().as_ref(), &prompt_arg);
        let spec = SpawnSpec {
            argv,
            cwd: input.cwd.clone(),
            env: child_env(&input.env),
            stdin: None,
            tee_stdout: input.event_log_hint.clone(),
            tee_stderr: input.event_log_hint,
            timeout: input.timeout,
        };
        let parser = OpencodeParser::new(Timestamp::now(), self.config.tz_offset_minutes, input.executor.clone());
        let run = RunHandle::spawn(spec, Box::new(parser))?;
        Ok(Box::new(run))
    }
}

/// Builds the argv for one opencode run (adapters.md §3.1).
///
/// The prompt is always the last argument. `--session` appears exactly when the input
/// resumes a native session, and `--title` exactly when the config sets one.
pub fn build_argv(cfg: &OpencodeConfig, input: &StartInput, binary: &str, prompt_arg: &str) -> Vec<String> {
    let mut argv = vec![
        binary.to_owned(),
        "run".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        "--auto".to_owned(),
        "--model".to_owned(),
        input.executor.model_arg(),
    ];

    if let Some(resume) = &input.resume {
        argv.push("--session".to_owned());
        argv.push(resume.clone());
    }
    if let Some(title) = &cfg.title {
        argv.push("--title".to_owned());
        argv.push(title.clone());
    }
    argv.extend(cfg.extra_args.iter().cloned());
    argv.push(prompt_arg.to_owned());
    argv
}

/// Resolves the configured binary, falling back to the user's opencode install.
fn resolve_config_binary(cfg: &OpencodeConfig) -> Option<PathBuf> {
    if let Some(binary) = &cfg.binary {
        return resolve_binary(binary);
    }
    if let Some(found) = resolve_binary("opencode") {
        return Some(found);
    }
    let home = std::env::var_os("HOME")?;
    let candidate = Path::new(&home).join(".opencode/bin/opencode");
    resolve_binary(candidate.to_str()?)
}

/// Returns the prompt argument, spilling oversized prompts to a file.
///
/// The file lives at `<cwd>/.vowp/prompts/<session-or-resume>-<n>.md`; the argument
/// then points at the relative path. The prompt itself is never logged.
fn prompt_argument(cfg: &OpencodeConfig, input: &StartInput) -> Result<String, AdapterError> {
    if input.prompt.len() <= cfg.max_arg_prompt_bytes {
        return Ok(input.prompt.clone());
    }

    let directory = input.cwd.join(".vowp/prompts");
    std::fs::create_dir_all(&directory)?;
    let base = input.resume.as_deref().unwrap_or(input.session_id.as_str());
    let base = sanitize_file_stem(base);
    let sequence = PROMPT_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    let name = format!("{base}-{sequence}.md");
    std::fs::write(directory.join(&name), input.prompt.as_bytes())?;
    Ok(format!(
        "Read the instructions in .vowp/prompts/{name} and follow them exactly."
    ))
}

/// Keeps native session ids safe for a file name.
fn sanitize_file_stem(value: &str) -> String {
    let stem: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() { "prompt".to_owned() } else { stem }
}

/// Matches the version in output such as `2.0.28`.
fn version_regex() -> &'static Regex {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    VERSION.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+)").expect("invariant: version regex literal is valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_limit_is_100_000_bytes() {
        assert_eq!(OpencodeConfig::default().max_arg_prompt_bytes, 100_000);
    }
}
