//! Adapter for the Claude Code CLI harness (`claude -p --output-format stream-json`).
//! Spec: `docs/design/adapters.md` §2 and `docs/design/crates/vowp-adapters.md` §4.1.
//!
//! One process per run: [`ClaudeCodeAdapter::start`] spawns the harness in its own
//! process group with the prompt on stdin and pipes stdout through [`ClaudeParser`].
//! This module owns argv construction, the cheap `--version` probe and capabilities.

mod mapping;
mod parser;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use vowp_protocol::{
    Adapter, AdapterCapabilities, AdapterError, AdapterRun, HarnessKind, ProbeResult, StartInput, Timestamp,
};

use crate::proc::{RunHandle, SpawnSpec, child_env, resolve_binary, run_capture};

pub use parser::ClaudeParser;

/// Maximum time `probe()` allows `claude --version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for [`ClaudeCodeAdapter`].
#[derive(Debug, Clone, Default)]
pub struct ClaudeConfig {
    /// Binary to run; defaults to the first `claude` on `PATH`.
    pub binary: Option<String>,
    /// Extra arguments appended after VOWP's flags and before the prompt.
    pub extra_args: Vec<String>,
    /// Directories passed through `--add-dir`.
    pub add_dirs: Vec<PathBuf>,
    /// Permission mode; defaults to `"auto"`.
    pub permission_mode: Option<String>,
    /// Pass the prompt as the last argument instead of stdin.
    pub prompt_via_arg: bool,
    /// Fixed offset used to interpret clock-time reset texts.
    pub tz_offset_minutes: i32,
}

/// The Claude Code adapter.
pub struct ClaudeCodeAdapter {
    config: ClaudeConfig,
}

impl ClaudeCodeAdapter {
    /// Creates an adapter from `config`.
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// Resolves the configured binary, or the first `claude` on `PATH`.
    fn binary(&self) -> Option<PathBuf> {
        match &self.config.binary {
            Some(name) => resolve_binary(name),
            None => resolve_binary("claude"),
        }
    }
}

impl Adapter for ClaudeCodeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    fn probe(&self) -> ProbeResult {
        let Some(binary) = self.binary() else {
            return ProbeResult {
                ok: false,
                version: None,
                path: None,
                detail: "binary not found on PATH".to_owned(),
            };
        };
        let path = Some(binary.to_string_lossy().into_owned());
        let argv = vec![binary.to_string_lossy().into_owned(), "--version".to_owned()];
        match run_capture(&argv, PROBE_TIMEOUT) {
            Ok((exit, stdout, _stderr)) => {
                let version = version_regex()
                    .captures(&stdout)
                    .and_then(|captures| captures.get(1))
                    .map(|matched| matched.as_str().to_owned());
                let ok = exit == Some(0) && version.is_some();
                let detail = if ok {
                    "claude --version succeeded".to_owned()
                } else if exit == Some(0) {
                    "claude --version printed no version".to_owned()
                } else {
                    format!("claude --version exited with {exit:?}")
                };
                ProbeResult {
                    ok,
                    version,
                    path,
                    detail,
                }
            }
            Err(error) => ProbeResult {
                ok: false,
                version: None,
                path,
                detail: error.to_string(),
            },
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: true,
            can_inject_assistant_history: false,
            can_switch_model_in_session: true,
            live_input: false,
            emits_reasoning: true,
            emits_cost_receipts: true,
            reports_resets_at: true,
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let binary = self.binary().ok_or_else(|| {
            let requested = self.config.binary.as_deref().unwrap_or("claude");
            AdapterError::Unavailable(format!("claude binary not found: {requested}"))
        })?;
        let binary = binary.to_string_lossy().into_owned();
        let argv = build_argv(&self.config, &input, &binary);
        let stdin = if self.config.prompt_via_arg {
            None
        } else {
            Some(input.prompt.as_bytes().to_vec())
        };

        let spec = SpawnSpec {
            argv,
            cwd: input.cwd.clone(),
            env: child_env(&input.env),
            stdin,
            tee_stdout: input.event_log_hint.clone(),
            tee_stderr: input.event_log_hint.clone(),
            timeout: input.timeout,
        };
        let parser = Box::new(ClaudeParser::new(
            Timestamp::now(),
            self.config.tz_offset_minutes,
            input.executor.clone(),
        ));
        let run = RunHandle::spawn(spec, parser)?;
        Ok(Box::new(run))
    }
}

/// Builds the Claude Code argv from `cfg` and `input`.
///
/// The prompt is appended last only when [`ClaudeConfig::prompt_via_arg`] is set; by
/// default it travels on stdin. `binary` is `argv[0]` (already resolved by `start`).
pub fn build_argv(cfg: &ClaudeConfig, input: &StartInput, binary: &str) -> Vec<String> {
    let mut argv = vec![
        binary.to_owned(),
        "-p".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--model".to_owned(),
        input.executor.model.clone(),
    ];

    if let Some(effort) = &input.executor.effort {
        argv.push("--effort".to_owned());
        argv.push(effort.clone());
    }

    let permission_mode = input
        .permission_mode
        .clone()
        .or_else(|| cfg.permission_mode.clone())
        .unwrap_or_else(|| "auto".to_owned());
    argv.push("--permission-mode".to_owned());
    argv.push(permission_mode);

    if let Some(system_note) = &input.system_note {
        argv.push("--append-system-prompt".to_owned());
        argv.push(system_note.clone());
    }

    for directory in &cfg.add_dirs {
        argv.push("--add-dir".to_owned());
        argv.push(directory.to_string_lossy().into_owned());
    }

    if let Some(resume) = &input.resume {
        argv.push("--resume".to_owned());
        argv.push(resume.clone());
    } else if let Some(new_native_id) = &input.new_native_id {
        argv.push("--session-id".to_owned());
        argv.push(new_native_id.clone());
    }

    argv.extend(cfg.extra_args.iter().cloned());

    if cfg.prompt_via_arg {
        argv.push(input.prompt.clone());
    }
    argv
}

/// Returns the compiled `x.y.z` version regex.
fn version_regex() -> &'static Regex {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    VERSION.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+)").expect("invariant: version regex literal is valid"))
}
