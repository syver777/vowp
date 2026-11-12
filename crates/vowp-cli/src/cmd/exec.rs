//! Hidden `vowp __exec` — the remote executor client.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.2 and §12.5. The command starts the
//! harness on this machine, spools every event before sending it, and owns the SSH child
//! that runs `vowp session ingest` on the home. `RemoteLine` frames go to that child's
//! stdin; this command's own stdout is a single status document (see [`print_status`]), not
//! a stream of `RemoteLine` objects. Re-running it over an existing spool resumes the turn
//! after a killed client instead of starting a second harness process. A spool with a
//! malformed complete line is refused rather than mistaken for an empty spool.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use serde::Deserialize;
use vowp_core::surface::takeover::notice as takeover_notice;
use vowp_protocol::{
    Cost, Executor, ExecutorRef, HarnessEvent, IngestAck, NodeName, RemoteEvent, RemoteLine, RunOutcome, RunRole,
    StartInput, StopReason, Timestamp, TurnGrant, TurnRequest,
};

use crate::ssh_stream::{INGEST_ACK_WINDOW, IngestClient, SshTarget};
use crate::{CliError, Ctx, IoStreams};

/// How long `__exec` waits for the next adapter event before sending a heartbeat.
const EVENT_POLL: Duration = Duration::from_secs(1);

/// Arguments for the hidden `vowp __exec`.
///
/// The command streams `RemoteLine` frames to the home through its own ingest child and
/// prints one status document (`{run_id, session_id, done, resumed, ack_window}`) when it
/// is done; it does not emit the frames on its own stdout.
#[derive(Debug, Clone, Args)]
pub struct ExecArgs {
    /// Grant file written by `session turn-begin` (mode 0600).
    #[arg(long, value_name = "FILE")]
    pub grant: PathBuf,
    /// Directory for the executor-side spool.
    #[arg(long, value_name = "DIR")]
    pub spool_dir: Option<PathBuf>,
}

/// One `session turn-begin` request document.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnBeginRequest {
    /// Lease token acquired by the surface.
    pub lease: String,
    /// Prompt for this turn.
    pub prompt: String,
    /// Executor string; must match the command argument when present.
    #[serde(default)]
    pub executor: Option<String>,
    /// Purpose; must match the command argument when present.
    #[serde(default)]
    pub purpose: Option<String>,
    /// Node; must match the command argument when present.
    #[serde(default)]
    pub node: Option<String>,
    /// Absolute checkout path on the surface, when it differs from the home's session cwd.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Runs the hidden `vowp __exec`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &ExecArgs) -> Result<i32, CliError> {
    let grant = read_grant(&args.grant)?;
    let executor = Executor::parse(&grant.executor)?;
    let spool = args.spool_dir.clone().unwrap_or_else(|| {
        ctx.paths()
            .root()
            .join("spool")
            .join(grant.session_id.as_str())
            .join(grant.run_id.as_str())
    });
    let target = home_target(ctx)?;
    let mut client = IngestClient::new(
        target,
        grant.session_id.clone(),
        grant.run_id.clone(),
        grant.lease_token.clone(),
        spool,
    )
    .with_project(Some(ctx.project().to_string()))
    .with_executor(grant.executor.clone(), grant.node.clone());

    // A spool that survived a killed client is replayed and finished without starting the
    // harness again. A spool with events but no summary cannot be completed: the harness
    // process is gone and no terminal frame exists.
    if client.has_spool()? {
        if client.summary_acked() {
            if !client.all_acked()? {
                return Err(CliError::Conflict(
                    "the spool records a summary ack but some events are unacknowledged".to_owned(),
                ));
            }
            print_status(ctx, io, &grant, true)?;
            return Ok(0);
        }
        if !client.summary_present()? {
            return Err(CliError::Conflict(
                "the spool has unacknowledged events but no summary; begin a new turn".to_owned(),
            ));
        }
        // A killed client may have stopped before its artifact redaction; run it again
        // before the replayed summary leaves this machine. Redaction is best-effort on a
        // surface that has no local store.
        if let Ok(store) = ctx.store() {
            let _ = vowp_core::vault::redact_job_artifacts(ctx.deps(), &store, None);
        }
        client.replay()?;
        if report_revoked(io, client.revoked(), ctx.tz_offset_minutes())? {
            return Ok(75);
        }
        client.finish()?;
        if report_revoked(io, client.revoked(), ctx.tz_offset_minutes())? {
            return Ok(75);
        }
        print_status(ctx, io, &grant, false)?;
        return Ok(0);
    }

    // The grant names the surface's checkout, not the home's. Validate it on the machine
    // that will run the harness; the home deliberately tolerates a path it cannot see.
    let cwd = PathBuf::from(&grant.cwd);
    if !cwd.is_dir() {
        return Err(fail_turn(
            &mut client,
            io,
            CliError::Invalid(format!(
                "working directory {} does not exist on this surface",
                cwd.display()
            )),
        ));
    }
    let turn = TurnRequest {
        session_id: grant.session_id.clone(),
        executor: ExecutorRef {
            executor: executor.clone(),
            node: Some(grant.node.clone()),
        },
        purpose: grant.purpose,
        role: RunRole::Worker,
        job_id: None,
        prompt: grant.prompt.clone(),
        profile: None,
        rung: 0,
        window: None,
        timeout: None,
    };
    let vault_env = match vowp_core::vault::env_for(ctx.deps(), &turn) {
        Ok(env) => env,
        Err(error) => return Err(fail_turn(&mut client, io, CliError::Core(error))),
    };
    let env = match resolve_env(&grant.env_names, &vault_env) {
        Ok(env) => env,
        Err(error) => return Err(fail_turn(&mut client, io, error)),
    };
    let adapter = match ctx.deps().adapters.get(&executor) {
        Some(adapter) => adapter,
        None => {
            return Err(fail_turn(
                &mut client,
                io,
                CliError::Invalid(format!("harness for {executor} is not installed on this machine")),
            ));
        }
    };
    let start = StartInput {
        session_id: grant.session_id.clone(),
        executor: executor.clone(),
        cwd,
        prompt: grant.prompt.clone(),
        resume: grant.resume_native.clone(),
        new_native_id: grant.native_hint.clone(),
        env,
        permission_mode: harness_config(ctx, &executor).and_then(|config| config.permission_mode.clone()),
        system_note: None,
        event_log_hint: Some(ctx.paths().run_raw_path(&grant.run_id)),
        timeout: None,
    };
    let mut adapter_run = match adapter.start(start) {
        Ok(run) => run,
        Err(error) => {
            return Err(fail_turn(
                &mut client,
                io,
                CliError::Core(vowp_core::CoreError::Adapter(error)),
            ));
        }
    };
    loop {
        match adapter_run.events().recv_timeout(EVENT_POLL) {
            Ok(event) => {
                // The summary owns the canonical `turn.ended`; forwarding the adapter's
                // terminal frame as an event would append it twice.
                if matches!(event.body, vowp_protocol::EventBody::TurnEnded(_)) {
                    break;
                }
                let line = RemoteLine::Event {
                    event: remote_event(ctx, &event, &grant, &executor),
                };
                if let Some(ack) = client.send(&line)? {
                    match ack {
                        IngestAck::LeaseRevoked { by, at, .. } => {
                            let _ = adapter_run.stop();
                            print_revoked(io, &by, at, ctx.tz_offset_minutes())?;
                            return Ok(75);
                        }
                        IngestAck::Error { message, .. } => {
                            let _ = adapter_run.stop();
                            return Err(CliError::Remote(message));
                        }
                        _ => {}
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let heartbeat = RemoteLine::Heartbeat { at: ctx.clock().now() };
                if let Some(IngestAck::LeaseRevoked { by, at, .. }) = client.send(&heartbeat)? {
                    let _ = adapter_run.stop();
                    print_revoked(io, &by, at, ctx.tz_offset_minutes())?;
                    return Ok(75);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let summary = match adapter_run.wait() {
        Ok(summary) => summary,
        Err(error) => {
            return Err(fail_turn(
                &mut client,
                io,
                CliError::Core(vowp_core::CoreError::Adapter(error)),
            ));
        }
    };
    // Redact the executor-side report and raw mirror before the summary leaves this machine.
    // The grant carries no job id, so this redacts node artifacts.
    if let Ok(store) = ctx.store() {
        let _ = vowp_core::vault::redact_job_artifacts(ctx.deps(), &store, None);
    }
    let line = RemoteLine::Summary {
        exit_code: summary.exit_code,
        outcome: summary.outcome,
        native_session_id: summary.native_session_id.clone(),
        cost: summary.cost.clone(),
        limit: summary.limit.clone(),
        result_text: summary.result_text.clone(),
        stop_reason: summary.stop_reason,
        usage: None,
    };
    if let Some(ack) = client.send(&line)? {
        match ack {
            IngestAck::LeaseRevoked { by, at, .. } => {
                print_revoked(io, &by, at, ctx.tz_offset_minutes())?;
                return Ok(75);
            }
            IngestAck::Error { message, .. } => return Err(CliError::Remote(message)),
            _ => {}
        }
    }
    client.finish()?;
    if report_revoked(io, client.revoked(), ctx.tz_offset_minutes())? {
        return Ok(75);
    }
    print_status(ctx, io, &grant, false)?;
    Ok(0)
}

/// Reports a turn that could not start or finish, then returns the original error.
///
/// `begin_turn` already wrote the attach, `turn.started` and the run on the home. A
/// summary with [`RunOutcome::Error`] reuses the v0.1 failed-start finalisation there:
/// error event, `turn.ended`, detach and `finish_run`, so the session is not left
/// `running` with an open run.
fn fail_turn(client: &mut IngestClient, io: &mut dyn IoStreams, error: CliError) -> CliError {
    let message = error.to_string();
    let summary = RemoteLine::Summary {
        exit_code: None,
        outcome: RunOutcome::Error,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some(message),
        stop_reason: StopReason::Error,
        usage: None,
    };
    let reported = client.send(&summary).and_then(|ack| match ack {
        Some(IngestAck::Error { message, .. }) => Err(CliError::Remote(message)),
        _ => client.finish(),
    });
    if let Err(report_error) = reported {
        let _ = writeln!(io.err(), "could not report the failed turn to the home: {report_error}");
    }
    error
}

/// Prints the exact read-only takeover notice once; returns whether the lease was revoked.
fn report_revoked(io: &mut dyn IoStreams, ack: Option<&IngestAck>, tz_offset_minutes: i32) -> Result<bool, CliError> {
    if let Some(IngestAck::LeaseRevoked { by, at, .. }) = ack {
        print_revoked(io, by, *at, tz_offset_minutes)?;
        return Ok(true);
    }
    Ok(false)
}

/// Writes the spec's exact takeover notice for a revoked lease.
fn print_revoked(io: &mut dyn IoStreams, by: &str, at: Timestamp, tz_offset_minutes: i32) -> Result<(), CliError> {
    writeln!(io.err(), "{}", takeover_notice(by, at, tz_offset_minutes))?;
    Ok(())
}

/// Reads and validates the grant file: JSON and mode 0600.
pub fn read_grant(path: &Path) -> Result<TurnGrant, CliError> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(CliError::Guard(format!(
                "grant file {} must be mode 0600 (found {mode:03o})",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|error| CliError::Invalid(format!("{}: invalid grant: {error}", path.display())))
}

/// Resolves every secret name the grant lists from the surface vault, then its environment.
fn resolve_env(names: &[String], vault_env: &[(String, String)]) -> Result<Vec<(String, String)>, CliError> {
    let mut resolved: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (name, value) in vault_env {
        resolved.insert(name.as_str(), value.as_str());
    }
    let mut env = Vec::with_capacity(names.len());
    for name in names {
        let value = match resolved.get(name.as_str()) {
            Some(value) => (*value).to_owned(),
            None => std::env::var(name).map_err(|_| {
                CliError::Invalid(format!(
                    "environment variable {name} is not set on the executor surface"
                ))
            })?,
        };
        env.push((name.clone(), value));
    }
    Ok(env)
}

/// Converts one adapter event into its wire form, with a client-assigned stable id.
fn remote_event(ctx: &Ctx, event: &HarnessEvent, grant: &TurnGrant, executor: &Executor) -> RemoteEvent {
    RemoteEvent {
        id: vowp_protocol::EventId::new(),
        ts: event.ts.unwrap_or_else(|| ctx.clock().now()),
        event_type: event.body.event_type(),
        actor: event.actor,
        source: vowp_protocol::EventSource {
            harness: executor.harness,
            model: Some(executor.model.clone()),
            node: Some(grant.node.clone()),
            native_session_id: None,
        },
        payload: event.body.to_value(),
        raw: event.raw.clone(),
        replayable: !matches!(event.body, vowp_protocol::EventBody::AssistantReasoning(_)),
    }
}

/// Returns the configured harness entry for `executor`.
fn harness_config<'a>(ctx: &'a Ctx, executor: &Executor) -> Option<&'a vowp_protocol::HarnessConfig> {
    let key = match executor.harness {
        vowp_protocol::HarnessKind::ClaudeCode => "claude-code",
        vowp_protocol::HarnessKind::Opencode => "opencode",
        vowp_protocol::HarnessKind::Acp => "acp",
        vowp_protocol::HarnessKind::Cli => executor.model.as_str(),
        vowp_protocol::HarnessKind::Vowp => return None,
    };
    ctx.config()
        .harnesses
        .get(key)
        .or_else(|| ctx.config().harnesses.get(&format!("cli:{key}")))
}

/// Builds the home target: a control machine's ssh home, or this machine's local home.
fn home_target(ctx: &Ctx) -> Result<SshTarget, CliError> {
    if let Some(control) = read_control_config()? {
        return Ok(SshTarget {
            node: NodeName::parse(&control.home)?,
            ssh_target: control.ssh_target,
            remote_home: control.remote_home,
        });
    }
    let node = ctx.config().node.name.clone();
    Ok(SshTarget {
        node,
        ssh_target: String::new(),
        remote_home: ctx.paths().root().display().to_string(),
    })
}

/// The subset of `control.json` the executor needs.
#[derive(Debug, Clone, Deserialize)]
struct ControlPointer {
    /// Home node name.
    home: String,
    /// SSH destination of the home.
    ssh_target: String,
    /// Absolute home path on the remote machine.
    remote_home: String,
}

/// Reads `~/.vowp-control/control.json` or `$VOWP_CONTROL_HOME/control.json`, when present.
fn read_control_config() -> Result<Option<ControlPointer>, CliError> {
    let root = match std::env::var_os("VOWP_CONTROL_HOME").filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => {
            let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
                return Ok(None);
            };
            PathBuf::from(home).join(".vowp-control")
        }
    };
    let path = root.join("control.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let pointer: ControlPointer =
        serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))?;
    Ok(Some(pointer))
}

/// Prints the hidden command's status document.
///
/// The shape is `{run_id, session_id, done, resumed, ack_window}`. It is a terminal
/// summary of the client side, not a `RemoteLine`: those frames travel on the ingest
/// child's stdin and are never re-printed here.
fn print_status(ctx: &Ctx, io: &mut dyn IoStreams, grant: &TurnGrant, replay: bool) -> Result<(), CliError> {
    let document = serde_json::json!({
        "run_id": grant.run_id.to_string(),
        "session_id": grant.session_id.to_string(),
        "done": true,
        "resumed": replay,
        "ack_window": INGEST_ACK_WINDOW,
    });
    if ctx.json() || !io.is_tty() {
        writeln!(io.out(), "{document}")?;
    }
    Ok(())
}
