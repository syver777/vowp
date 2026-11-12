//! Adapter for an Agent Client Protocol (ACP) server.
//! Spec: `docs/design/adapters.md` §10 and `docs/design/v0.2/acp-adapter.md`.
//!
//! One run starts one ACP server (JSON-RPC 2.0 over stdio), performs `initialize`,
//! `session/new` and one `session/prompt`, and maps the streamed updates to canonical events.
//! The first slice covers agent message chunks, tool call start/update/completion, plan updates,
//! token usage and the prompt result. It does not resume natively, stream chunk events, approve
//! permission requests or render a permission UI.

mod mapping;
mod run;

use std::path::PathBuf;

use vowp_protocol::{Adapter, AdapterCapabilities, AdapterError, AdapterRun, HarnessKind, ProbeResult, StartInput};

use crate::proc::{SpawnSpec, child_env, resolve_binary};
use run::RunOptions;

/// ACP protocol version sent in `initialize`; the first slice pins revision 1.
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// Default binary looked up on `PATH` when the config names none.
const DEFAULT_BINARY: &str = "claude-code-acp";

/// Configuration for [`AcpAdapter`].
#[derive(Debug, Clone, Default)]
pub struct AcpConfig {
    /// Server to run; defaults to `claude-code-acp` on `PATH`.
    pub binary: Option<String>,
    /// Extra arguments appended after the binary.
    pub extra_args: Vec<String>,
    /// Fixed offset used to interpret clock-time reset texts.
    pub tz_offset_minutes: i32,
}

/// The ACP adapter.
pub struct AcpAdapter {
    config: AcpConfig,
}

impl AcpAdapter {
    /// Creates an adapter from `config`.
    pub fn new(config: AcpConfig) -> Self {
        Self { config }
    }
}

impl Adapter for AcpAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Acp
    }

    fn probe(&self) -> ProbeResult {
        let Some(binary) = resolve_config_binary(&self.config) else {
            let detail = match &self.config.binary {
                Some(binary) => format!("ACP server binary not found: {binary}"),
                None => format!("ACP server binary not found on PATH: {DEFAULT_BINARY}"),
            };
            return ProbeResult {
                ok: false,
                version: None,
                path: None,
                detail,
            };
        };

        // ACP servers speak JSON-RPC on stdin, so a `--version` probe would wait for input.
        // Finding an executable binary is the cheap, honest check; the version stays unknown.
        ProbeResult {
            ok: true,
            version: None,
            path: Some(binary.to_string_lossy().into_owned()),
            detail: format!("ACP server binary found at {} (version not probed)", binary.display()),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: false,
            can_inject_assistant_history: false,
            can_switch_model_in_session: false,
            live_input: false,
            emits_reasoning: false,
            emits_cost_receipts: false,
            reports_resets_at: false,
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let Some(binary) = resolve_config_binary(&self.config) else {
            let detail = match &self.config.binary {
                Some(value) => format!("ACP server binary not found: {value}"),
                None => format!("ACP server binary not found on PATH: {DEFAULT_BINARY}"),
            };
            return Err(AdapterError::Unavailable(detail));
        };

        let mut argv = vec![binary.to_string_lossy().into_owned()];
        argv.extend(self.config.extra_args.iter().cloned());
        let spec = SpawnSpec {
            argv,
            cwd: input.cwd.clone(),
            env: child_env(&input.env),
            // ACP keeps stdin open for the JSON-RPC conversation; the driver owns it.
            stdin: None,
            tee_stdout: input.event_log_hint.clone(),
            tee_stderr: input.event_log_hint,
            timeout: input.timeout,
        };
        let options = RunOptions {
            prompt: input.prompt,
            cwd: input.cwd,
            executor: input.executor,
            tz_offset_minutes: self.config.tz_offset_minutes,
            protocol_version: ACP_PROTOCOL_VERSION,
        };
        let run = run::AcpRun::start(spec, options)?;
        Ok(Box::new(run))
    }
}

/// Resolves the configured binary, falling back to the documented default name.
fn resolve_config_binary(config: &AcpConfig) -> Option<PathBuf> {
    match &config.binary {
        Some(binary) => resolve_binary(binary),
        None => resolve_binary(DEFAULT_BINARY),
    }
}
