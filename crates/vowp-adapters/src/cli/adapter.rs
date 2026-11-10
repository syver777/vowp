//! The generic CLI adapter: probe, argv construction and process start.
//! Spec: `docs/design/adapters.md` §4.3 and §8, `docs/design/crates/vowp-adapters.md` §4.3.

use std::path::Path;
use std::time::Duration;

use vowp_protocol::{
    Adapter, AdapterCapabilities, AdapterError, AdapterRun, HarnessKind, ProbeResult, StartInput, Timestamp,
};

use crate::proc::{RunHandle, SpawnSpec, child_env, resolve_binary, run_capture};
use crate::stream::StreamParser;

use super::mapping::{Mapping, PromptVia, parse_toml};
use super::parser::{CliParser, version_regex};

/// How long a probe may run before its process group is killed.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The generic CLI adapter, driven entirely by a [`Mapping`].
pub struct CliAdapter {
    mapping: Mapping,
}

impl CliAdapter {
    /// Loads, validates and stores a mapping from a TOML file.
    pub fn from_mapping_file(path: &Path) -> Result<CliAdapter, AdapterError> {
        let text = std::fs::read_to_string(path)?;
        let mapping = parse_toml(&text, &path.display().to_string())?;
        Ok(Self { mapping })
    }

    /// Stores a mapping after validating it.
    pub fn from_mapping(mapping: Mapping) -> Result<CliAdapter, AdapterError> {
        mapping.validate("<mapping>")?;
        Ok(Self { mapping })
    }

    /// Returns the mapping this adapter uses.
    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    /// Builds the child argv, including model, cwd and prompt arguments.
    fn build_argv(&self, input: &StartInput) -> Vec<String> {
        let mut argv = match (&input.resume, &self.mapping.command.resume_argv) {
            (Some(native_id), Some(template)) => template
                .iter()
                .map(|part| part.replace("{native_id}", native_id))
                .collect::<Vec<_>>(),
            _ => self.mapping.command.argv.clone(),
        };

        if let Some(flag) = &self.mapping.command.model_flag {
            if let Some(value) = &self.mapping.command.model_arg {
                argv.push(flag.clone());
                argv.push(value.clone());
            } else if let Some(modifier) = input.executor.effort.as_ref().or(input.executor.variant.as_ref()) {
                argv.push(flag.clone());
                argv.push(modifier.clone());
            }
        } else if let Some(value) = &self.mapping.command.model_arg {
            argv.push(value.clone());
        }

        if let Some(flag) = &self.mapping.command.cwd_flag {
            argv.push(flag.clone());
            argv.push(input.cwd.to_string_lossy().into_owned());
        }

        match &self.mapping.command.prompt_via {
            PromptVia::Stdin => {}
            PromptVia::Arg => argv.push(input.prompt.clone()),
            PromptVia::Flag(flag) => {
                argv.push(flag.clone());
                argv.push(input.prompt.clone());
            }
        }
        argv
    }

    /// Resolves `argv[0]` or returns the typed unavailable error.
    fn resolve_argv(&self, argv: &[String]) -> Result<String, AdapterError> {
        let name = argv.first().map_or("", String::as_str);
        resolve_binary(name)
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| AdapterError::Unavailable(format!("harness binary not found on PATH: {name}")))
    }

    /// Selects the mapping's env names from the caller's env map.
    fn selected_env(&self, input: &StartInput) -> Vec<(String, String)> {
        self.mapping
            .command
            .env
            .iter()
            .filter_map(|name| input.env.iter().find(|(candidate, _)| candidate == name).cloned())
            .collect()
    }
}

impl Adapter for CliAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Cli
    }

    fn probe(&self) -> ProbeResult {
        let template = self.mapping.version_argv.clone().unwrap_or_else(|| {
            vec![
                self.mapping.command.argv.first().cloned().unwrap_or_default(),
                "--version".to_owned(),
            ]
        });
        let name = template.first().map_or("", String::as_str);
        let Some(binary) = resolve_binary(name) else {
            return ProbeResult {
                ok: false,
                version: None,
                path: None,
                detail: "binary not found on PATH".to_owned(),
            };
        };

        let mut argv = template;
        if let Some(first) = argv.first_mut() {
            *first = binary.to_string_lossy().into_owned();
        }
        match run_capture(&argv, PROBE_TIMEOUT) {
            Ok((exit, stdout, _stderr)) => {
                let version = version_regex()
                    .captures(&stdout)
                    .and_then(|captures| captures.get(1))
                    .map(|matched| matched.as_str().to_owned())
                    .or_else(|| {
                        stdout
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(|line| line.chars().take(80).collect())
                    });
                ProbeResult {
                    ok: exit == Some(0),
                    version,
                    path: Some(binary.to_string_lossy().into_owned()),
                    detail: match exit {
                        Some(code) => format!("exit code {code}"),
                        None => "probe timed out or was killed".to_owned(),
                    },
                }
            }
            Err(error) => ProbeResult {
                ok: false,
                version: None,
                path: Some(binary.to_string_lossy().into_owned()),
                detail: error.to_string(),
            },
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: self.mapping.command.resume_argv.is_some(),
            can_inject_assistant_history: false,
            can_switch_model_in_session: false,
            live_input: false,
            emits_reasoning: self.mapping.rules.iter().any(|rule| rule.emit == "assistant.reasoning"),
            emits_cost_receipts: self.mapping.cost.usd_pointer.is_some(),
            reports_resets_at: self.mapping.limits.resets_at.is_some(),
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let mut argv = self.build_argv(&input);
        let binary = self.resolve_argv(&argv)?;
        if let Some(first) = argv.first_mut() {
            *first = binary;
        }

        let stdin = match self.mapping.command.prompt_via {
            PromptVia::Stdin => Some(input.prompt.clone().into_bytes()),
            PromptVia::Arg | PromptVia::Flag(_) => None,
        };
        let spec = SpawnSpec {
            argv,
            cwd: input.cwd.clone(),
            env: child_env(&self.selected_env(&input)),
            stdin,
            tee_stdout: input.event_log_hint.clone(),
            tee_stderr: input.event_log_hint,
            timeout: input.timeout,
        };

        let parser = CliParser::new(self.mapping.clone(), Timestamp::now())?;
        let run = RunHandle::spawn(spec, Box::new(parser) as Box<dyn StreamParser>)?;
        Ok(Box::new(run))
    }
}
