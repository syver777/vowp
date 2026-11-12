//! `session` and the `chat` alias.
//!
//! Spec: `docs/design/cli.md` §4 and `docs/design/sessions.md`. Session content lives in the
//! append-only log; the rows here are the store index. Ids are accepted by unique prefix of
//! at least six characters.

use std::io::{BufRead, BufReader, Read, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use time::{OffsetDateTime, macros::format_description};
use vowp_core::session::bundle::{self, ImportMode};
use vowp_core::session::importer::{ImportCursor, import_file, line_uuid};
use vowp_core::session::remote::{BeginTurn, IngestGrant, IngestSession, begin_turn};
use vowp_core::session::service::{SessionHead, SessionService, local_head};
use vowp_core::surface::native::{TranscriptEntry, TranscriptKind};
use vowp_protocol::{
    Actor, AssistantMessage, AssistantReasoning, AttachAction, CapsuleTrigger, Cost, CostEvent, CostSource, EventBody,
    EventEnvelope, EventSource, EventType, Executor, ExecutorRef, HarnessKind, NewSession, NodeName, RunPurpose,
    Session, SessionId, SessionRole, SessionState, Switch, SwitchEvent, SwitchId, SwitchReason, SwitchState, Timestamp,
    ToolCall, ToolResult, UserMessage,
};
use vowp_store::reindex::reindex;
use vowp_store::session_log::{
    SESSION_DIR_MAX_BYTES, SessionDirManifest, SessionLog, session_dir_manifest, verify_session_dir,
    write_session_dir_manifest,
};
use vowp_store::{NodePaths, SessionFilter, Store};

use crate::cli::{
    ChatArgs, Global, SessionCapsuleArgs, SessionCommand, SessionDirExportArgs, SessionExportArgs, SessionImportArgs,
    SessionLsArgs, SessionNewArgs, SessionPullArgs, SessionShowArgs, SessionSwitchArgs, SessionTailArgs,
    SessionWatchArgs,
};
use crate::node_ctx::NodeRole;
use crate::output::{Table, local_time_at, print_json};
use crate::remote::{RemoteExit, RemoteStream};
use crate::{CliError, Ctx, IoStreams};

/// Runs one session subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &SessionCommand) -> Result<i32, CliError> {
    match command {
        SessionCommand::Ls(args) => list(ctx, io, args),
        SessionCommand::Show(args) => show(ctx, io, args),
        SessionCommand::New(args) => new_session(ctx, io, args),
        SessionCommand::Chat(args) => chat(ctx, io, args),
        SessionCommand::Switch(args) => switch(ctx, io, args),
        SessionCommand::Capsule(args) => capsule(ctx, io, args),
        SessionCommand::Export(args) => export(ctx, io, args),
        SessionCommand::Import(args) => import(ctx, io, args),
        SessionCommand::Pull(args) => pull(ctx, io, args),
        SessionCommand::DirExport(args) => dir_export(ctx, io, args),
        SessionCommand::Watch(args) => watch(ctx, io, args),
        SessionCommand::Tail(args) => tail(ctx, io, args),
        SessionCommand::TurnBegin(args) => turn_begin(ctx, io, args),
        SessionCommand::Ingest(args) => ingest(ctx, io, args),
        SessionCommand::Lease(args) => crate::cmd::lease::lease(ctx, io, args),
        SessionCommand::Takeover(args) => crate::cmd::lease::takeover(ctx, io, args),
    }
}

/// Runs a session subcommand that targets a remote node.
///
/// `cli.md` §7.5–§7.6 requires export, import and switch to cross the node boundary as a
/// `vowp-bundle-v1` document instead of forwarding an id or a local path over ssh. This
/// function is called before the generic `--node` redirect so it can run the local half of
/// those flows itself.
pub fn maybe_run_remote(
    global: &Global,
    io: &mut dyn IoStreams,
    command: &crate::cli::Command,
) -> Result<Option<i32>, CliError> {
    // `chat --node` names the executor node, but the global `--node` flag accepts the same
    // spelling. Clap stores it in `global.node`; treat it as chat's executor node instead
    // of forwarding the whole command over ssh. A control machine keeps the generic route.
    if let Some(args) = chat_args(command)
        && let Some(node) = &global.node
    {
        let mut local = global.clone();
        local.node = None;
        let ctx = Ctx::build(&local, io)?;
        if ctx.role() == NodeRole::Home {
            let node = NodeName::parse(node)?;
            return Ok(Some(run_chat_args(&ctx, io, args, Some(node))?));
        }
        return Ok(None);
    }

    let crate::cli::Command::Session { command } = command else {
        return Ok(None);
    };
    // `session pull` runs in this process: it fetches one bundle from the named source node,
    // verifies it and imports a local child. Its first positional argument is the source,
    // so it must not reach the generic `--node` SSH redirect below.
    if let SessionCommand::Pull(args) = command {
        let ctx = Ctx::build(global, io)?;
        return Ok(Some(pull(&ctx, io, args)?));
    }
    // The hidden session interfaces run on the home that owns the session. Their `--node`
    // names the executor or surface, not a redirect target, so they must not go through the
    // generic `--node` SSH redirect below.
    if matches!(
        command,
        SessionCommand::TurnBegin(_)
            | SessionCommand::Ingest(_)
            | SessionCommand::Lease(_)
            | SessionCommand::Takeover(_)
    ) {
        let ctx = Ctx::build(global, io)?;
        return Ok(Some(run(&ctx, io, command)?));
    }
    let node = match command {
        SessionCommand::New(args) => args.node.as_deref().or(global.node.as_deref()),
        SessionCommand::Switch(args) => args.node.as_deref().or(global.node.as_deref()),
        SessionCommand::Import(args) => args.node.as_deref().or(global.node.as_deref()),
        SessionCommand::Export(_) => global.node.as_deref(),
        _ => None,
    };
    let Some(node) = node else {
        return Ok(None);
    };
    let name = NodeName::parse(node)?;
    let paths = crate::ctx::node_paths(global)?;
    let store = Store::open(&paths)?;
    let row = store.require_node(&name)?;
    if row.is_local {
        return Ok(None);
    }
    let target = row
        .ssh_target
        .as_deref()
        .ok_or_else(|| CliError::Invalid(format!("node {name} has no ssh target")))?;
    let ctx = Ctx::build(global, io)?;
    let code = match command {
        SessionCommand::New(args) => remote_new(&ctx, io, target, args)?,
        SessionCommand::Switch(args) => remote_switch(&ctx, io, target, &name, args)?,
        SessionCommand::Export(args) => remote_export(&ctx, io, target, args)?,
        SessionCommand::Import(args) => remote_import(&ctx, io, target, args)?,
        _ => return Ok(None),
    };
    Ok(Some(code))
}

/// One captured remote invocation.
struct RemoteRun {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `ssh … vowp <args>` with optional stdin and captured output.
fn run_remote_capture(target: &str, args: &[String], stdin: Option<&str>) -> Result<RemoteRun, CliError> {
    let argv = crate::remote::build_ssh_argv(target, args, false);
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| CliError::Remote("empty ssh argv".to_owned()))?;
    let mut command = ProcessCommand::new(program);
    command
        .args(rest)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| CliError::Remote(format!("ssh {target}: {error}")))?;
    if let Some(text) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(text.as_bytes())?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| CliError::Remote(format!("ssh {target}: {error}")))?;
    let code = output
        .status
        .code()
        .ok_or_else(|| CliError::Remote(format!("ssh {target} terminated by a signal")))?;
    if code == 255 {
        return Err(CliError::Remote(format!("ssh {target} failed with exit 255")));
    }
    Ok(RemoteRun {
        code,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Returns the global arguments a remote session command needs to select the project.
fn remote_prefix(ctx: &Ctx) -> Vec<String> {
    vec!["--project".to_owned(), ctx.project().to_string()]
}

/// Writes a failed remote invocation's output and returns its exit code.
fn relay_failure(io: &mut dyn IoStreams, run: &RemoteRun) -> Result<i32, CliError> {
    if !run.stdout.is_empty() {
        ia_write(io.out(), &run.stdout)?;
    }
    if !run.stderr.is_empty() {
        ia_write(io.err(), &run.stderr)?;
    }
    Ok(run.code)
}

/// Writes bytes, adding no formatting of its own.
fn ia_write(stream: &mut dyn std::io::Write, text: &str) -> Result<(), CliError> {
    stream.write_all(text.as_bytes())?;
    Ok(())
}

/// Parses a remote `--json` session document.
fn parse_remote_session(run: &RemoteRun) -> Result<Session, CliError> {
    serde_json::from_str(&run.stdout)
        .map_err(|error| CliError::Remote(format!("remote printed an invalid session document: {error}")))
}

/// Creates a session on a remote node.
fn remote_new(ctx: &Ctx, io: &mut dyn IoStreams, target: &str, args: &SessionNewArgs) -> Result<i32, CliError> {
    let mut remote = remote_prefix(ctx);
    remote.extend(["--json".to_owned(), "session".to_owned(), "new".to_owned()]);
    if let Some(title) = &args.title {
        remote.extend(["--title".to_owned(), title.clone()]);
    }
    if let Some(executor) = &args.executor {
        remote.extend(["--executor".to_owned(), executor.clone()]);
    }
    if let Some(cwd) = &args.cwd {
        remote.extend(["--cwd".to_owned(), cwd.display().to_string()]);
    }
    let run = run_remote_capture(target, &remote, None)?;
    if run.code != 0 {
        return relay_failure(io, &run);
    }
    let session = parse_remote_session(&run)?;
    if ctx.json() {
        print_json(io, &session)?;
    } else {
        writeln!(io.out(), "{}", session.id)?;
    }
    Ok(0)
}

/// Exports a remote session to a local bundle file.
fn remote_export(ctx: &Ctx, io: &mut dyn IoStreams, target: &str, args: &SessionExportArgs) -> Result<i32, CliError> {
    let mut remote = remote_prefix(ctx);
    remote.extend([
        "session".to_owned(),
        "export".to_owned(),
        args.id.clone(),
        "--out".to_owned(),
        "-".to_owned(),
    ]);
    if let Some(tail) = args.tail {
        remote.extend(["--tail".to_owned(), tail.to_string()]);
    }
    let run = run_remote_capture(target, &remote, None)?;
    if run.code != 0 {
        return relay_failure(io, &run);
    }
    let bundle = bundle::from_json(&run.stdout)?;
    if args.out.as_deref() == Some(std::path::Path::new("-")) {
        ia_write(io.out(), &run.stdout)?;
        return Ok(0);
    }
    let path = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.vowp-bundle.json", bundle.session.id)));
    std::fs::write(&path, &run.stdout)?;
    if ctx.json() {
        print_json(
            io,
            &ExportOutput {
                path: Some(path.display().to_string()),
                session: bundle.session.id.clone(),
                format: bundle::BUNDLE_FORMAT,
                events: bundle.tail.len(),
            },
        )?;
    } else {
        writeln!(io.out(), "{}", path.display())?;
    }
    Ok(0)
}

/// Imports a local bundle on a remote node, streaming it over ssh stdin.
fn remote_import(ctx: &Ctx, io: &mut dyn IoStreams, target: &str, args: &SessionImportArgs) -> Result<i32, CliError> {
    let text = if args.file == "-" {
        crate::read_stdin(io)?
    } else {
        std::fs::read_to_string(&args.file)?
    };
    let mut remote = remote_prefix(ctx);
    remote.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "import".to_owned(),
        "-".to_owned(),
    ]);
    if let Some(executor) = &args.executor {
        remote.extend(["--executor".to_owned(), executor.clone()]);
    }
    if let Some(cwd) = &args.cwd {
        remote.extend(["--cwd".to_owned(), cwd.display().to_string()]);
    }
    if args.same_id {
        remote.push("--same-id".to_owned());
    }
    let run = run_remote_capture(target, &remote, Some(&text))?;
    if run.code != 0 {
        return relay_failure(io, &run);
    }
    let session = parse_remote_session(&run)?;
    if ctx.json() {
        print_json(io, &session)?;
    } else {
        writeln!(io.out(), "{}", session.id)?;
    }
    Ok(0)
}

/// JSON shape for a cross-node pull.
///
/// The default child mode serialises `child_session`; `--same-id` serialises
/// `same_id_session` instead, so existing consumers keep their exact document.
#[derive(Debug, serde::Serialize)]
struct PullOutput {
    node: NodeName,
    source_session: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_session: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_id_session: Option<SessionId>,
}

/// Pulls a session bundle from another node and imports it locally.
///
/// The default path captures a bundle from the remote `session export` stdout, stages it in
/// a private temporary directory and verifies it by [`bundle::from_json`] before
/// [`bundle::import_with_mode`] creates anything. A failed remote run, malformed bundle or
/// checksum mismatch therefore imports nothing. The default creates a child whose parent is
/// the source; `--same-id` keeps the source id and refuses a duplicate instead of
/// overwriting. `--full` instead copies the whole session directory; see [`pull_full`].
fn pull(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionPullArgs) -> Result<i32, CliError> {
    let (node, target) = pull_target(ctx, &args.node)?;
    if args.full {
        return pull_full(ctx, io, &node, &target, args);
    }

    let mut remote = remote_prefix(ctx);
    remote.extend([
        "session".to_owned(),
        "export".to_owned(),
        args.session.clone(),
        "--out".to_owned(),
        "-".to_owned(),
    ]);
    let run = run_remote_capture(&target, &remote, None)?;
    if run.code != 0 {
        return relay_failure(io, &run);
    }

    let staged = StagedBundle::write(&run.stdout)?;
    let bundle = bundle::from_json(&staged.read()?)?;
    let source = bundle.session.id.clone();
    // A same-id pull never overwrites: when the id is already local, report both heads
    // and let the operator choose a side before anything is imported.
    if args.same_id && local_session_present(ctx, &source)? {
        let local = local_head(ctx.deps(), &source)?;
        let remote = bundle::head(&bundle);
        return report_divergence(ctx, io, &node, &source, &local, &remote);
    }
    let mode = if args.same_id {
        ImportMode::SameId
    } else {
        ImportMode::Child
    };
    let imported = bundle::import_with_mode(ctx.deps(), &bundle, None, None, mode)?;
    if ctx.json() {
        let output = if args.same_id {
            PullOutput {
                node,
                source_session: source.clone(),
                child_session: None,
                same_id_session: Some(imported.clone()),
            }
        } else {
            PullOutput {
                node,
                source_session: source.clone(),
                child_session: Some(imported.clone()),
                same_id_session: None,
            }
        };
        print_json(io, &output)?;
    } else if args.same_id {
        writeln!(io.out(), "pulled session {source} from {node} with same id {imported}")?;
    } else {
        writeln!(io.out(), "pulled session {source} from {node} as child {imported}")?;
    }
    Ok(0)
}

/// Resolves the source node and refuses a local node before any remote work.
fn pull_target(ctx: &Ctx, node: &str) -> Result<(NodeName, String), CliError> {
    let node = NodeName::parse(node)?;
    let store = ctx.store()?;
    let row = store.require_node(&node)?;
    if row.is_local {
        return Err(CliError::Invalid(format!(
            "node {node} is local; use `vowp session export` and `vowp session import` for a local copy"
        )));
    }
    let target = row
        .ssh_target
        .ok_or_else(|| CliError::Invalid(format!("node {node} has no ssh target")))?;
    Ok((node, target))
}

/// JSON shape for `session pull --full`.
#[derive(Debug, serde::Serialize)]
struct FullPullOutput {
    node: NodeName,
    source_session: SessionId,
    full_session: SessionId,
    files: usize,
    bytes: u64,
}

/// Pulls the full `sessions/<id>/` directory and imports it under the origin id.
///
/// The remote side streams one manifest line followed by the file bodies. The manifest is
/// checked for the size cap and the local id before a staging directory is created; every
/// digest is verified before the staged directory is renamed into place. The source is
/// never modified and no lease is moved: the caller must leave it quiescent.
fn pull_full(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    node: &NodeName,
    target: &str,
    args: &SessionPullArgs,
) -> Result<i32, CliError> {
    // The source manifest resolves a prefix and names the exact origin. A present local id
    // is reported with both heads after the manifest arrives; the snapshot is not copied
    // into the local session store.
    let mut remote = remote_prefix(ctx);
    remote.extend([
        "session".to_owned(),
        "dir-export".to_owned(),
        args.session.clone(),
        "--max-bytes".to_owned(),
        SESSION_DIR_MAX_BYTES.to_string(),
    ]);
    let mut stream = RemoteStream::spawn(target, &remote)?;
    let line = match read_manifest_line(stream.stdout()) {
        Ok(line) => line,
        Err(error) => return finish_stream_error(io, stream, error),
    };
    let manifest: SessionDirManifest = match serde_json::from_slice(&line) {
        Ok(manifest) => manifest,
        Err(error) => {
            return finish_stream_error(
                io,
                stream,
                CliError::Invalid(format!("remote sent an invalid session directory manifest: {error}")),
            );
        }
    };
    if let Err(error) = manifest.validate() {
        return finish_stream_error(io, stream, error.into());
    }
    if let Ok(requested) = SessionId::parse(&args.session)
        && manifest.origin != requested
    {
        return finish_stream_error(
            io,
            stream,
            CliError::Invalid(format!(
                "remote resolved session {} but the pull named {requested}",
                manifest.origin
            )),
        );
    }

    let origin = manifest.origin.clone();
    if local_session_present(ctx, &origin)? {
        // Stop the snapshot stream before it is copied anywhere, then read the source's
        // head with a cheap read-only command. Nothing local is written.
        let exit = stream.abort()?;
        if exit.code != 0 {
            return relay_remote_exit(io, exit);
        }
        let local = local_head(ctx.deps(), &origin)?;
        let remote = remote_full_head(ctx, target, &origin)?;
        return report_divergence(ctx, io, node, &origin, &local, &remote);
    }
    let bytes = manifest.total_bytes();
    if bytes > SESSION_DIR_MAX_BYTES {
        return finish_stream_error(
            io,
            stream,
            CliError::Invalid(format!(
                "session {origin} is {bytes} bytes; the full pull limit is {SESSION_DIR_MAX_BYTES} bytes"
            )),
        );
    }

    let staged = StagedSession::create(ctx, &origin)?;
    let mut copy_error = None;
    for file in &manifest.files {
        if let Err(error) = copy_stream_file(stream.stdout(), &staged.dir().join(&file.path), file.bytes) {
            copy_error = Some(error);
            break;
        }
    }
    if copy_error.is_none() {
        let mut trailing = [0_u8; 1];
        match stream.stdout().read(&mut trailing) {
            Ok(0) => {}
            Ok(_) => {
                copy_error = Some(CliError::Invalid(
                    "remote sent trailing bytes after the session directory snapshot".to_owned(),
                ));
            }
            Err(error) => copy_error = Some(error.into()),
        }
    }
    if let Some(error) = copy_error {
        return finish_stream_error(io, stream, error);
    }
    let finished = stream.finish()?;
    if finished.code != 0 {
        return relay_remote_exit(io, finished);
    }
    verify_session_dir(&staged.paths, &origin, &manifest)?;
    write_session_dir_manifest(&staged.paths, &origin, &manifest)?;

    let final_dir = ctx.paths().session_dir(&origin);
    if let Some(parent) = final_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if final_dir.exists() {
        return Err(full_exists(&origin));
    }
    std::fs::rename(staged.dir(), &final_dir)?;

    let store = ctx.store()?;
    let report = match reindex(&store, ctx.paths(), Some(&origin)) {
        Ok(report) => report,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&final_dir);
            return Err(error.into());
        }
    };
    if report.sessions != 1 {
        let _ = std::fs::remove_dir_all(&final_dir);
        let detail = if report.problems.is_empty() {
            "reindex did not index the pulled session".to_owned()
        } else {
            report.problems.join("; ")
        };
        return Err(CliError::Conflict(format!(
            "full pull of {origin} was not indexed cleanly: {detail}"
        )));
    }
    // An absent local project or job row is the only expected warning: reindex keeps the
    // session and leaves those foreign keys null. The index row exists, so removing the
    // directory here would leave a dangling session.
    for problem in &report.problems {
        writeln!(io.err(), "warning: {problem}")?;
    }

    let file_count = manifest.files.len();
    if ctx.json() {
        print_json(
            io,
            &FullPullOutput {
                node: node.clone(),
                source_session: origin.clone(),
                full_session: origin,
                files: file_count,
                bytes,
            },
        )?;
    } else {
        writeln!(
            io.out(),
            "pulled session {origin} from {node} as a full directory snapshot ({file_count} files, {bytes} bytes)"
        )?;
    }
    Ok(0)
}

/// Returns whether a session id already exists locally as a row or a directory.
fn local_session_present(ctx: &Ctx, id: &SessionId) -> Result<bool, CliError> {
    Ok(ctx.store()?.session(id)?.is_some() || ctx.paths().session_dir(id).exists())
}

/// The refusal for a full pull whose id already exists locally.
fn full_exists(id: &SessionId) -> CliError {
    CliError::Conflict(format!(
        "session {id} already exists locally; --full refuses to overwrite"
    ))
}

/// Reads the source head for a full-pull divergence report.
///
/// The directory manifest names the origin but carries no log position, so the source's
/// existing read-only `session show --json --events 1` supplies its last seq, event,
/// state and executor. This does not copy the directory and changes nothing.
fn remote_full_head(ctx: &Ctx, target: &str, origin: &SessionId) -> Result<SessionHead, CliError> {
    let mut remote = remote_prefix(ctx);
    remote.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "show".to_owned(),
        "--events".to_owned(),
        "1".to_owned(),
        origin.to_string(),
    ]);
    let run = run_remote_capture(target, &remote, None)?;
    if run.code != 0 {
        return Err(CliError::Remote(format!(
            "remote session show for {origin} failed with exit {}: {}",
            run.code,
            run.stderr.trim()
        )));
    }

    #[derive(serde::Deserialize)]
    struct Show {
        #[serde(flatten)]
        session: Session,
        #[serde(default)]
        events: Vec<EventEnvelope>,
    }
    let show: Show = serde_json::from_str(&run.stdout)
        .map_err(|error| CliError::Remote(format!("remote printed an invalid session document: {error}")))?;
    if show.session.id != *origin {
        return Err(CliError::Remote(format!(
            "remote resolved session {} but the manifest named {origin}",
            show.session.id
        )));
    }
    Ok(SessionHead::from_session(&show.session, show.events.last()))
}

/// The stable JSON code for a pull whose id exists on both sides.
const DIVERGENCE_CODE: &str = "session_divergence";

/// The manual-choice note printed with a divergence report.
const DIVERGENCE_NOTE: &str = "choose which side advances; no merge performed";

/// The exit code for a divergence report: the conflict code from `cli.md` §1.1.
const DIVERGENCE_EXIT: i32 = 4;

/// JSON shape for a divergent pull.
#[derive(Debug, serde::Serialize)]
struct DivergenceOutput {
    error: DivergenceError,
}

/// The `error` document a divergent pull prints under `--json`.
#[derive(Debug, serde::Serialize)]
struct DivergenceError {
    kind: &'static str,
    code: &'static str,
    message: String,
    hint: &'static str,
    node: NodeName,
    source_session: SessionId,
    local: HeadOutput,
    remote: HeadOutput,
    note: &'static str,
}

/// One head in a divergence report.
#[derive(Debug, serde::Serialize)]
struct HeadOutput {
    session: SessionId,
    node: NodeName,
    last_seq: u64,
    last_event: Option<EventType>,
    last_event_at: Option<Timestamp>,
    state: Option<SessionState>,
    executor: Option<String>,
    updated_at: Option<Timestamp>,
}

impl HeadOutput {
    /// Converts a collected head into its JSON form.
    fn from_head(head: &SessionHead) -> Self {
        Self {
            session: head.session_id.clone(),
            node: head.node.clone(),
            last_seq: head.last_seq,
            last_event: head.last_event,
            last_event_at: head.last_event_at,
            state: head.state,
            executor: head.executor.as_ref().map(ToString::to_string),
            updated_at: head.updated_at,
        }
    }
}

/// Prints a divergence report and returns its conflict exit code.
///
/// This is an error path, but it owns the whole document: the generic error renderer only
/// carries `kind`, `message` and `hint`, and the report must expose both heads with one
/// stable shape. Nothing is written to the local session store.
fn report_divergence(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    node: &NodeName,
    source: &SessionId,
    local: &SessionHead,
    remote: &SessionHead,
) -> Result<i32, CliError> {
    let message = format!("session {source} exists locally and on {node}; refusing to overwrite or merge");
    let hint = "delete or export one side, then pull again";
    // The source row names its own node; the operator knows it by the local registry name.
    let remote = SessionHead {
        node: node.clone(),
        ..remote.clone()
    };
    if ctx.json() {
        print_json(
            io,
            &DivergenceOutput {
                error: DivergenceError {
                    kind: "conflict",
                    code: DIVERGENCE_CODE,
                    message,
                    hint,
                    node: node.clone(),
                    source_session: source.clone(),
                    local: HeadOutput::from_head(local),
                    remote: HeadOutput::from_head(&remote),
                    note: DIVERGENCE_NOTE,
                },
            },
        )?;
    } else {
        writeln!(io.err(), "vowp: {message}")?;
        writeln!(io.err(), "local:  {}", head_line(local))?;
        writeln!(io.err(), "remote: {}", head_line(&remote))?;
        writeln!(io.err(), "{DIVERGENCE_NOTE}")?;
        writeln!(io.err(), "hint: {hint}")?;
    }
    Ok(DIVERGENCE_EXIT)
}

/// Renders one head as a single human line, without the session cwd or any secret.
fn head_line(head: &SessionHead) -> String {
    let last = match (head.last_event, head.last_event_at) {
        (Some(event), Some(ts)) => format!("{event} at {}", ts.as_rfc3339()),
        (Some(event), None) => event.to_string(),
        (None, _) => "no readable log event".to_owned(),
    };
    format!(
        "{} on {}: seq {}, last {}, state {}, executor {}",
        head.session_id,
        head.node,
        head.last_seq,
        last,
        head.state.map(state_name).unwrap_or("-"),
        head.executor
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )
}

/// Upper bound on one manifest line; larger streams are not trusted.
const PULL_MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Reads the first newline-terminated line of the remote snapshot stream.
fn read_manifest_line(reader: &mut BufReader<std::process::ChildStdout>) -> Result<Vec<u8>, CliError> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(PULL_MANIFEST_MAX_BYTES + 1)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(CliError::Invalid(
            "remote sent no session directory manifest".to_owned(),
        ));
    }
    if line.len() as u64 > PULL_MANIFEST_MAX_BYTES {
        return Err(CliError::Invalid(format!(
            "remote session directory manifest is larger than {PULL_MANIFEST_MAX_BYTES} bytes"
        )));
    }
    if !line.ends_with(b"\n") {
        return Err(CliError::Invalid(
            "remote session directory manifest has no terminator".to_owned(),
        ));
    }
    line.pop();
    Ok(line)
}

/// Copies exactly `bytes` from the snapshot stream into one staged file.
fn copy_stream_file(
    reader: &mut BufReader<std::process::ChildStdout>,
    path: &Path,
    bytes: u64,
) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    let copied = std::io::copy(&mut reader.by_ref().take(bytes), &mut file)?;
    if copied != bytes {
        return Err(CliError::Invalid(format!(
            "remote session directory stream ended early for {}: expected {bytes} bytes, got {copied}",
            path.display()
        )));
    }
    file.sync_all()?;
    Ok(())
}

/// Aborts a snapshot stream and relays a remote failure when the source produced one.
fn finish_stream_error(io: &mut dyn IoStreams, stream: RemoteStream, error: CliError) -> Result<i32, CliError> {
    let exit = stream.abort()?;
    if exit.code != 0 {
        return relay_remote_exit(io, exit);
    }
    Err(error)
}

/// Prints a failed remote stream's stderr and returns its exit code.
fn relay_remote_exit(io: &mut dyn IoStreams, exit: RemoteExit) -> Result<i32, CliError> {
    if !exit.stderr.is_empty() {
        ia_write(io.err(), &exit.stderr)?;
    }
    Ok(exit.code)
}

/// One full session directory staged under `<home>/tmp` until it is verified.
struct StagedSession {
    root: PathBuf,
    paths: NodePaths,
    id: SessionId,
}

impl StagedSession {
    /// Creates a fresh private staging root containing `sessions/<id>/`.
    fn create(ctx: &Ctx, id: &SessionId) -> Result<Self, CliError> {
        let staging_base = ctx.paths().tmp_dir();
        std::fs::create_dir_all(&staging_base)?;
        let root = private_dir_in(&staging_base, "session-pull")?;
        let paths = NodePaths::new(&root);
        std::fs::create_dir_all(paths.session_dir(id))?;
        Ok(Self {
            root,
            paths,
            id: id.clone(),
        })
    }

    /// Returns the staged `sessions/<id>/` path.
    fn dir(&self) -> PathBuf {
        self.paths.session_dir(&self.id)
    }
}

impl Drop for StagedSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Streams one full session directory for `session pull --full`.
///
/// The first line is the JSON manifest; the remaining bytes are the listed files in
/// manifest order. Nothing is written to local state on the source and no lock is taken.
fn dir_export(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionDirExportArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    if !ctx.paths().log_path(&session.id).is_file() {
        return Err(CliError::Invalid(format!(
            "session {} has no log.jsonl (it was compacted); use the bundle pull, which carries its capsule",
            session.id
        )));
    }
    let max_bytes = args.max_bytes.unwrap_or(SESSION_DIR_MAX_BYTES);
    let manifest = session_dir_manifest(ctx.paths(), &session.id, max_bytes)?;

    let line = serde_json::to_vec(&manifest)
        .map_err(|error| CliError::Invalid(format!("cannot encode session directory manifest: {error}")))?;
    io.out().write_all(&line)?;
    io.out().write_all(b"\n")?;
    for file in &manifest.files {
        let path = ctx.paths().session_dir(&session.id).join(&file.path);
        let mut source = std::fs::File::open(&path)?;
        let copied = std::io::copy(&mut Read::by_ref(&mut source).take(file.bytes), io.out())?;
        if copied != file.bytes {
            return Err(CliError::Invalid(format!(
                "session {} file {} changed while it was copied",
                session.id, file.path
            )));
        }
    }
    io.out().flush()?;
    Ok(0)
}

/// One pulled bundle staged in a private temporary directory until import.
struct StagedBundle {
    dir: PathBuf,
    path: PathBuf,
}

impl StagedBundle {
    /// Writes `text` into a fresh private temporary directory.
    fn write(text: &str) -> Result<Self, CliError> {
        let dir = private_temp_dir()?;
        let staged = Self {
            path: dir.join("bundle.json"),
            dir,
        };
        std::fs::write(&staged.path, text)?;
        std::fs::set_permissions(&staged.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(staged)
    }

    /// Reads the staged document back for verification and import.
    fn read(&self) -> Result<String, CliError> {
        Ok(std::fs::read_to_string(&self.path)?)
    }
}

impl Drop for StagedBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Creates a fresh private directory under the OS temporary root.
fn private_temp_dir() -> Result<PathBuf, CliError> {
    private_dir_in(&std::env::temp_dir(), "vowp-pull")
}

/// Creates a fresh 0700 directory named `<prefix>-<pid>-<millis>-<counter>` under `base`.
fn private_dir_in(base: &Path, prefix: &str) -> Result<PathBuf, CliError> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let name = format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            Timestamp::now().unix_millis(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        let dir = base.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => match std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)) {
                Ok(()) => return Ok(dir),
                Err(error) => {
                    let _ = std::fs::remove_dir(&dir);
                    return Err(error.into());
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(CliError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("could not create a private temporary directory for {prefix}"),
    )))
}

/// Moves a session to a remote node: export locally, import remotely, switch there.
fn remote_switch(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    target: &str,
    remote_node: &NodeName,
    args: &SessionSwitchArgs,
) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    if session.state == SessionState::Running && !args.force {
        return Err(CliError::Conflict(format!(
            "session {} is running a turn; pass --force to switch",
            session.id
        )));
    }
    let executor = Executor::parse(&args.to)?;
    let reason = parse_reason(args.reason.as_deref())?;
    let provider = executor.provider();
    if store
        .active_limits(ctx.clock().now())?
        .iter()
        .any(|limit| limit.provider == provider)
    {
        return Err(CliError::Limited(format!("provider {provider} is limited")));
    }

    let bundle = bundle::export(ctx.deps(), &session.id, 20)?;
    let text = bundle::to_json(&bundle)?;
    let mut import = remote_prefix(ctx);
    import.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "import".to_owned(),
        "-".to_owned(),
        "--executor".to_owned(),
        args.to.clone(),
    ]);
    let imported = run_remote_capture(target, &import, Some(&text))?;
    if imported.code != 0 {
        return relay_failure(io, &imported);
    }
    let child = parse_remote_session(&imported)?;

    let mut remote = remote_prefix(ctx);
    remote.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "switch".to_owned(),
        child.id.to_string(),
        "--to".to_owned(),
        args.to.clone(),
        "--reason".to_owned(),
        reason_name(reason).to_owned(),
    ]);
    if args.force {
        remote.push("--force".to_owned());
    }
    let switched = run_remote_capture(target, &remote, None)?;
    if switched.code != 0 {
        return relay_failure(io, &switched);
    }

    let local = record_local_cross_switch(
        ctx,
        &session,
        executor,
        remote_node.clone(),
        bundle.capsule.capsule_id.clone(),
        reason,
    )?;
    if ctx.json() {
        print_json(io, &local)?;
    } else {
        let capsule = local
            .capsule_id
            .as_ref()
            .and_then(|id| capsule_path(ctx, &session.id, id))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none".to_owned());
        writeln!(
            io.out(),
            "switched session {} to {} on {} (capsule {capsule}, remote session {})",
            session.id,
            local.to_executor,
            remote_node,
            child.id
        )?;
    }
    Ok(0)
}

/// Appends the local side of a cross-node switch and inserts its completed row.
fn record_local_cross_switch(
    ctx: &Ctx,
    session: &Session,
    executor: Executor,
    remote_node: NodeName,
    capsule_id: vowp_protocol::CapsuleId,
    reason: SwitchReason,
) -> Result<Switch, CliError> {
    let store = ctx.store()?;
    let now = ctx.clock().now();
    let switch_id = SwitchId::new();
    let from = session.executor.clone().map(|executor| ExecutorRef {
        executor,
        node: Some(session.node.clone()),
    });
    let to = ExecutorRef {
        executor: executor.clone(),
        node: Some(remote_node.clone()),
    };
    let log =
        SessionLog::open(ctx.paths(), &session.id, ctx.deps().clock.clone()).map_err(vowp_core::CoreError::from)?;
    let appended = log
        .append(vowp_store::session_log::NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(ctx.config().node.name.clone()),
                native_session_id: None,
            },
            body: EventBody::Switch(SwitchEvent {
                switch_id: switch_id.clone(),
                from: from.clone(),
                to: to.clone(),
                capsule_id: Some(capsule_id.clone()),
                reason,
            }),
            raw: None,
            ts: None,
        })
        .map_err(vowp_core::CoreError::from)?;
    store.index_event(&vowp_store::session_log::index_row(&appended))?;
    log.sync().map_err(vowp_core::CoreError::from)?;
    let switch = Switch {
        id: switch_id,
        session_id: session.id.clone(),
        from_executor: session.executor.clone(),
        to_executor: executor,
        from_node: Some(session.node.clone()),
        to_node: remote_node,
        capsule_id: Some(capsule_id),
        reason,
        state: SwitchState::Completed,
        error: None,
        started_at: now,
        ended_at: Some(now),
        at_seq: session.last_seq,
    };
    store.insert_switch(&switch)?;
    Ok(switch)
}

/// Returns the wire name of a switch reason.
fn reason_name(reason: SwitchReason) -> &'static str {
    match reason {
        SwitchReason::User => "user",
        SwitchReason::Limit => "limit",
        SwitchReason::Crash => "crash",
        SwitchReason::Context => "context",
        SwitchReason::Failover => "failover",
        SwitchReason::ProbeBack => "probe_back",
        SwitchReason::Takeover => "takeover",
    }
}

/// Begins a remote turn and prints its grant.
fn turn_begin(ctx: &Ctx, io: &mut dyn IoStreams, args: &crate::cli::TurnBeginArgs) -> Result<i32, CliError> {
    let text = if args.request == "-" {
        crate::read_stdin(io)?
    } else {
        std::fs::read_to_string(&args.request)?
    };
    let request: crate::cmd::exec::TurnBeginRequest = serde_json::from_str(&text)
        .map_err(|error| CliError::Invalid(format!("{}: invalid turn request: {error}", args.request)))?;
    if let Some(executor) = &request.executor
        && executor != &args.executor
    {
        return Err(CliError::Invalid(format!(
            "request executor {executor} does not match --executor {}",
            args.executor
        )));
    }
    if let Some(purpose) = &request.purpose
        && purpose != &args.purpose
    {
        return Err(CliError::Invalid(format!(
            "request purpose {purpose} does not match --purpose {}",
            args.purpose
        )));
    }
    if let Some(node) = &request.node
        && node != &args.node
    {
        return Err(CliError::Invalid(format!(
            "request node {node} does not match --node {}",
            args.node
        )));
    }
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let executor = Executor::parse(&args.executor)?;
    let purpose = parse_purpose(&args.purpose)?;
    let node = NodeName::parse(&args.node)?;
    let grant = begin_turn(
        ctx.deps(),
        &store,
        &BeginTurn {
            session: session.id,
            lease: request.lease,
            executor: ExecutorRef {
                executor,
                node: Some(node.clone()),
            },
            purpose,
            node,
            cwd: request.cwd,
            prompt: request.prompt,
        },
    )?;
    print_json(io, &grant)?;
    Ok(0)
}

/// Ingests remote JSON lines on stdin and writes one ack per line on stdout.
fn ingest(ctx: &Ctx, io: &mut dyn IoStreams, args: &crate::cli::IngestArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let run = vowp_protocol::RunId::parse(&args.run)?;
    let grant = IngestGrant {
        session: session.id,
        // The token travels in RemoteLine::Hello, never on argv or in `ps`.
        lease: String::new(),
        run,
    };
    let mut ingest = IngestSession::begin(ctx.deps(), &store, &grant)?;
    loop {
        let mut line = String::new();
        let read = io.input().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if ingest.line(&line, io.out())? {
            break;
        }
    }
    // Streaming commands write only their wire frames on stdout: an extra report document
    // after the acknowledgements would make `--json` output unparsable.
    ingest.finish()?;
    Ok(0)
}

/// Parses a run purpose name.
fn parse_purpose(value: &str) -> Result<RunPurpose, CliError> {
    match value {
        "start" => Ok(RunPurpose::Start),
        "continue" => Ok(RunPurpose::Continue),
        "fix" => Ok(RunPurpose::Fix),
        "verify" => Ok(RunPurpose::Verify),
        "plan" => Ok(RunPurpose::Plan),
        "keep" => Ok(RunPurpose::Keep),
        "capsule" => Ok(RunPurpose::Capsule),
        "probe" => Ok(RunPurpose::Probe),
        "merge" => Ok(RunPurpose::Merge),
        "gate" => Ok(RunPurpose::Gate),
        other => Err(CliError::Invalid(format!("unknown run purpose: {other}"))),
    }
}

/// Lists sessions with the documented columns.
fn list(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionLsArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let project = match args.project.as_deref() {
        Some(value) => match vowp_protocol::ProjectName::parse(value) {
            Ok(name) => Some(name),
            Err(error) => {
                let path = std::path::Path::new(value);
                if path.is_dir() && path.join("vowp.toml").is_file() {
                    Some(
                        crate::ctx::load_project_config(&path.join("vowp.toml"), ctx.paths())?
                            .project
                            .name,
                    )
                } else {
                    return Err(error.into());
                }
            }
        },
        None => Some(ctx.project().clone()),
    };
    let filter = SessionFilter {
        project,
        job_id: args.job.as_deref().map(vowp_protocol::JobId::parse).transpose()?,
        state: args.state.as_deref().map(parse_state).transpose()?,
        limit: args.limit,
    };
    let sessions = store.list_sessions(&filter)?;
    if ctx.json() {
        print_json(io, &sessions)?;
        return Ok(0);
    }

    let now = ctx.clock().now();
    let mut table = Table::new(&["ID", "TITLE", "EXECUTOR", "NODE", "STATE", "LAST", "UPDATED", "EVENTS"]);
    for session in &sessions {
        let last = last_event(&store, session)
            .map(|event| local_time_at(event.ts, now, ctx.tz_offset_minutes()))
            .unwrap_or_else(|| "-".to_owned());
        table.row(vec![
            session.id.to_string(),
            session.title.clone(),
            session
                .executor
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "-".to_owned()),
            session.node.to_string(),
            state_name(session.state).to_owned(),
            last,
            local_time_at(session.updated_at, now, ctx.tz_offset_minutes()),
            session.last_seq.to_string(),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Shows one session, its executor history, switches and optional capsule/events.
fn show(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionShowArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    // Compaction deletes log.jsonl, so the compacted check must happen before any read
    // path opens the log. `--capsule` alone is a valid inspection of the surviving history.
    let compacted = compacted_capsule(ctx, &store, &session)?;
    if let Some(capsule_id) = &compacted
        && args.events.is_some()
    {
        return Err(CliError::NotFound(compacted_message(capsule_id)));
    }
    let events = if compacted.is_some() {
        Vec::new()
    } else {
        SessionLog::open_read(ctx.paths(), &session.id)
            .map_err(vowp_core::CoreError::from)?
            .read_all()
            .map_err(vowp_core::CoreError::from)?
    };
    let executor_history: Vec<EventEnvelope> = events
        .iter()
        .filter(|event| event.event_type == vowp_protocol::EventType::SessionExecutor)
        .cloned()
        .collect();
    let switches = store.list_switches(&session.id)?;
    let capsule = if args.capsule {
        latest_capsule(ctx, &store, &session)?
    } else {
        None
    };
    let last_events = match args.events {
        Some(count) => {
            let start = events.len().saturating_sub(count);
            events[start..].to_vec()
        }
        None => Vec::new(),
    };

    if ctx.json() {
        #[derive(serde::Serialize)]
        struct Show<'a> {
            #[serde(flatten)]
            session: &'a Session,
            #[serde(skip_serializing_if = "Option::is_none")]
            capsule: Option<&'a vowp_protocol::Capsule>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            switches: Vec<&'a vowp_protocol::Switch>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            events: Vec<EventEnvelope>,
        }
        print_json(
            io,
            &Show {
                session: &session,
                capsule: capsule.as_ref(),
                switches: if args.lineage {
                    switches.iter().collect()
                } else {
                    Vec::new()
                },
                events: last_events,
            },
        )?;
        return Ok(0);
    }

    let now = ctx.clock().now();
    writeln!(io.out(), "id: {}", session.id)?;
    writeln!(io.out(), "title: {}", session.title)?;
    writeln!(io.out(), "state: {}", state_name(session.state))?;
    writeln!(
        io.out(),
        "project: {}",
        session
            .project
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(
        io.out(),
        "job: {}",
        session
            .job_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(io.out(), "node: {}", session.node)?;
    writeln!(io.out(), "cwd: {}", session.cwd)?;
    writeln!(
        io.out(),
        "executor: {}",
        session
            .executor
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(
        io.out(),
        "created: {}",
        local_time_at(session.created_at, now, ctx.tz_offset_minutes())
    )?;
    writeln!(
        io.out(),
        "updated: {}",
        local_time_at(session.updated_at, now, ctx.tz_offset_minutes())
    )?;
    writeln!(io.out(), "events: {}", session.last_seq)?;

    if !executor_history.is_empty() {
        writeln!(io.out(), "\nexecutor history:")?;
        for event in &executor_history {
            if let Ok(EventBody::SessionExecutor(payload)) = event.body() {
                let action = match payload.action {
                    AttachAction::Attach => "attach",
                    AttachAction::Detach => "detach",
                };
                writeln!(
                    io.out(),
                    "  {} {} {} on {}",
                    clock_time(event.ts, ctx.tz_offset_minutes()),
                    action,
                    payload.executor,
                    payload.node
                )?;
            }
        }
    }
    if args.lineage && !switches.is_empty() {
        writeln!(io.out(), "\nswitches:")?;
        for switch in &switches {
            writeln!(
                io.out(),
                "  {} {} -> {} ({:?}, {:?})",
                switch.id,
                switch
                    .from_executor
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".to_owned()),
                switch.to_executor,
                switch.reason,
                switch.state
            )?;
        }
    }
    if let Some(capsule) = &capsule {
        writeln!(
            io.out(),
            "\ncapsule: {} (trigger {:?}, at seq {})",
            capsule.capsule_id,
            capsule.trigger,
            capsule.at_seq
        )?;
    }
    if !last_events.is_empty() {
        writeln!(io.out(), "\nevents:")?;
        for event in &last_events {
            writeln!(io.out(), "{}", pretty_event(event, ctx.tz_offset_minutes()))?;
        }
    }
    Ok(0)
}

/// Returns the capsule that replaced the log when `session` was compacted.
///
/// `sessions.compacted_at` is the authoritative flag; a missing log with no flag is treated
/// the same way because there is nothing left to replay. Callers must check this before
/// opening `log.jsonl`, which compaction deletes.
fn compacted_capsule(
    ctx: &Ctx,
    store: &Store,
    session: &Session,
) -> Result<Option<vowp_protocol::CapsuleId>, CliError> {
    let compacted = store.lifecycle_compacted_at(&session.id)?.is_some();
    if !compacted && ctx.paths().log_path(&session.id).is_file() {
        return Ok(None);
    }
    Ok(latest_capsule(ctx, store, session)?
        .map(|capsule| capsule.capsule_id)
        .or_else(|| session.last_capsule_id.clone()))
}

/// Returns the latest capsule from the index, or from the authoritative capsule files.
///
/// Compaction keeps the capsule files even when the index cache is rebuilt, so the file
/// scan keeps `show --capsule` and the compacted-session refusals working.
fn latest_capsule(ctx: &Ctx, store: &Store, session: &Session) -> Result<Option<vowp_protocol::Capsule>, CliError> {
    if let Some(capsule) = store.latest_capsule(&session.id)? {
        return Ok(Some(capsule));
    }
    let mut latest: Option<vowp_protocol::Capsule> = None;
    for path in vowp_store::session_files::list_capsule_files(ctx.paths(), &session.id)? {
        let capsule = vowp_store::session_files::read_capsule_file(&path)?;
        if capsule.session_id != session.id {
            continue;
        }
        let newer = latest
            .as_ref()
            .is_none_or(|current| (capsule.at_seq, capsule.created_at) > (current.at_seq, current.created_at));
        if newer {
            latest = Some(capsule);
        }
    }
    Ok(latest)
}

/// Returns the exact refusal for an event read on a compacted session.
fn compacted_message(capsule_id: &vowp_protocol::CapsuleId) -> String {
    format!("session compacted to capsule {capsule_id}; use 'session show --capsule'")
}

/// Creates an interactive session and prints its id.
fn new_session(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionNewArgs) -> Result<i32, CliError> {
    if let Some(title) = &args.title
        && title.trim().is_empty()
    {
        return Err(CliError::Invalid("session title must not be empty".to_owned()));
    }
    let executor = args.executor.as_deref().map(Executor::parse).transpose()?;
    let cwd = args.cwd.clone().unwrap_or_else(|| ctx.project_root().to_path_buf());
    if let Some(node) = &args.node {
        let name = vowp_protocol::NodeName::parse(node)?;
        let store = ctx.store()?;
        let row = store.require_node(&name)?;
        if !row.is_local {
            let target = row
                .ssh_target
                .as_deref()
                .ok_or_else(|| CliError::Invalid(format!("node {name} has no ssh target")))?;
            return remote_new(ctx, io, target, args);
        }
    }
    let title = args.title.clone().unwrap_or_else(|| "interactive".to_owned());
    let service = SessionService::new(ctx.deps().clone());
    let id = service.create_session(NewSession {
        project: Some(ctx.project().clone()),
        job_id: None,
        role: SessionRole::User,
        title,
        cwd,
        parent: None,
        executor,
    })?;
    let store = ctx.store()?;
    let session = store.require_session(&id)?;
    if ctx.json() {
        print_json(io, &session)?;
    } else {
        writeln!(io.out(), "{id}")?;
    }
    Ok(0)
}

/// Runs the interactive chat loop.
fn chat(ctx: &Ctx, io: &mut dyn IoStreams, args: &ChatArgs) -> Result<i32, CliError> {
    run_chat_args(ctx, io, args, None)
}

/// Runs the chat handler with an executor-node override from the global `--node` flag.
fn run_chat_args(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    args: &ChatArgs,
    node_override: Option<vowp_protocol::NodeName>,
) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let node = match node_override {
        Some(node) => Some(node),
        None => args.node.as_deref().map(vowp_protocol::NodeName::parse).transpose()?,
    };
    let executor = args
        .executor
        .as_deref()
        .map(Executor::parse)
        .transpose()?
        .map(|executor| ExecutorRef {
            executor,
            node: node.clone(),
        });
    crate::chat::run_shared(
        ctx,
        io,
        &crate::chat::ChatOptions {
            session: session.id,
            executor,
            node,
            follow: args.follow,
            takeover: args.takeover,
        },
    )
}

/// Returns the chat arguments of either chat spelling.
fn chat_args(command: &crate::cli::Command) -> Option<&ChatArgs> {
    match command {
        crate::cli::Command::Chat(args) => Some(args),
        crate::cli::Command::Session {
            command: SessionCommand::Chat(args),
        } => Some(args),
        _ => None,
    }
}

/// Switches a session, checking the running-turn guard and target limits first.
fn switch(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionSwitchArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let target = Executor::parse(&args.to)?;
    let node = args.node.as_deref().map(vowp_protocol::NodeName::parse).transpose()?;
    if let Some(node) = &node {
        let row = store.require_node(node)?;
        if !row.is_local {
            let target = row
                .ssh_target
                .as_deref()
                .ok_or_else(|| CliError::Invalid(format!("node {node} has no ssh target")))?;
            return remote_switch(ctx, io, target, node, args);
        }
    }
    let reason = parse_reason(args.reason.as_deref())?;
    let switch = switch_to(ctx, &session.id, target, node, reason, args.force)?;
    if ctx.json() {
        print_json(io, &switch)?;
    } else {
        let capsule = switch
            .capsule_id
            .as_ref()
            .and_then(|id| capsule_path(ctx, &session.id, id))
            .map(|path| path.display().to_string())
            .or_else(|| switch.capsule_id.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| "none".to_owned());
        writeln!(
            io.out(),
            "switched session {} to {} (capsule {capsule})",
            session.id,
            switch.to_executor
        )?;
    }
    Ok(0)
}

/// Runs the core switch protocol after the CLI guards.
pub(crate) fn switch_to(
    ctx: &Ctx,
    session: &SessionId,
    target: Executor,
    node: Option<vowp_protocol::NodeName>,
    reason: SwitchReason,
    force: bool,
) -> Result<vowp_protocol::Switch, CliError> {
    let store = ctx.store()?;
    let row = store.require_session(session)?;
    if row.state == SessionState::Running && !force {
        return Err(CliError::Conflict(format!(
            "session {session} is running a turn; pass --force to switch"
        )));
    }
    let provider = target.provider();
    if store
        .active_limits(ctx.clock().now())?
        .iter()
        .any(|limit| limit.provider == provider)
    {
        return Err(CliError::Limited(format!("provider {provider} is limited")));
    }
    let service = SessionService::new(ctx.deps().clone());
    Ok(service.switch(&vowp_protocol::SwitchRequest {
        session_id: session.clone(),
        to: ExecutorRef { executor: target, node },
        reason,
        drain_secs: 5,
    })?)
}

/// Writes a manual capsule and prints its id or Markdown.
fn capsule(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionCapsuleArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let service = SessionService::new(ctx.deps().clone());
    let capsule = service.write_capsule(&session.id, CapsuleTrigger::Manual)?;
    if ctx.json() {
        print_json(io, &capsule)?;
    } else if args.print {
        write!(io.out(), "{}", capsule.render_markdown())?;
    } else {
        writeln!(io.out(), "{}", capsule.capsule_id)?;
    }
    Ok(0)
}

/// JSON shape for an export to a file.
#[derive(Debug, serde::Serialize)]
struct ExportOutput {
    path: Option<String>,
    session: SessionId,
    format: &'static str,
    events: usize,
}

/// Exports a session bundle.
fn export(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionExportArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    let bundle = bundle::export(ctx.deps(), &session.id, args.tail.unwrap_or(0))?;
    let text = bundle::to_json(&bundle)?;
    if args.out.as_deref() == Some(std::path::Path::new("-")) {
        write!(io.out(), "{text}")?;
        if !text.ends_with('\n') {
            writeln!(io.out())?;
        }
        return Ok(0);
    }
    let path = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.vowp-bundle.json", session.id)));
    std::fs::write(&path, text)?;
    if ctx.json() {
        print_json(
            io,
            &ExportOutput {
                path: Some(path.display().to_string()),
                session: session.id,
                format: bundle::BUNDLE_FORMAT,
                events: bundle.tail.len(),
            },
        )?;
    } else {
        writeln!(io.out(), "{}", path.display())?;
    }
    Ok(0)
}

/// Imports a bundle as a child session.
fn import(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionImportArgs) -> Result<i32, CliError> {
    if let Some(node) = &args.node {
        let name = vowp_protocol::NodeName::parse(node)?;
        let store = ctx.store()?;
        let row = store.require_node(&name)?;
        if !row.is_local {
            let target = row
                .ssh_target
                .as_deref()
                .ok_or_else(|| CliError::Invalid(format!("node {name} has no ssh target")))?;
            return remote_import(ctx, io, target, args);
        }
    }
    let text = if args.file == "-" {
        crate::read_stdin(io)?
    } else {
        std::fs::read_to_string(&args.file)?
    };
    let bundle = bundle::from_json(&text)?;
    let executor = match &args.executor {
        Some(text) => Some(ExecutorRef {
            executor: Executor::parse(text)?,
            node: None,
        }),
        None => None,
    };
    let mode = if args.same_id {
        ImportMode::SameId
    } else {
        ImportMode::Child
    };
    let id = bundle::import_with_mode(ctx.deps(), &bundle, executor, args.cwd.as_deref(), mode)?;
    let store = ctx.store()?;
    let session = store.require_session(&id)?;
    if ctx.json() {
        print_json(io, &session)?;
    } else {
        writeln!(io.out(), "{id}")?;
    }
    Ok(0)
}

/// Two-second poll interval for the follow mode.
const WATCH_POLL: Duration = Duration::from_secs(2);

/// How often the follow wait re-checks the interrupt flag.
const WATCH_WAIT_SLICE: Duration = Duration::from_millis(100);

/// Executor recorded for a session imported from Claude Code.
const CLAUDE_EXECUTOR: &str = "claude-code:claude-sonnet-5-5@medium";

/// Model recorded on imported Claude events.
const CLAUDE_MODEL: &str = "claude-sonnet-5-5";

/// Records interactive Claude Code transcripts (the optional v0.1 importer).
///
/// `--follow`, or a plain terminal run without `--once`, follows the transcript tree
/// every two seconds until Ctrl-C; `--once`, or a non-TTY run without `--follow`, makes
/// one incremental pass so scripts and tests do not hang. Every file is opened
/// read-only.
fn watch(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionWatchArgs) -> Result<i32, CliError> {
    let dir = args.claude_dir.clone().unwrap_or_else(default_claude_dir);
    if !dir.is_dir() {
        return Err(CliError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Claude transcript directory {} is unreadable", dir.display()),
        )));
    }
    let follow = args.follow || (!args.once && io.is_tty());
    let (transcripts, imported) = if follow {
        let count = transcript_files(&dir)?.len();
        if !ctx.json() {
            writeln!(io.out(), "watching {count} transcripts (press Ctrl-C to stop)")?;
        }
        let interrupted = interrupt_flag()?;
        watch_until(ctx, &dir, WATCH_POLL, &|| interrupted.load(Ordering::SeqCst))?
    } else {
        watch_pass(ctx, &dir)?
    };
    if ctx.json() {
        print_json(
            io,
            &serde_json::json!({
                "watching": transcripts,
                "imported": imported,
                "dir": dir.display().to_string(),
            }),
        )?;
    } else {
        writeln!(
            io.out(),
            "watching {transcripts} transcripts ({imported} events imported)"
        )?;
    }
    Ok(0)
}

/// Returns the default Claude transcript root without reading it in tests.
fn default_claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

/// Lists `*.jsonl` transcript files recursively under `dir`.
fn transcript_files(dir: &Path) -> Result<Vec<PathBuf>, CliError> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Runs one incremental pass over every transcript under `dir`.
fn watch_pass(ctx: &Ctx, dir: &Path) -> Result<(usize, usize), CliError> {
    let transcripts = transcript_files(dir)?;
    let store = ctx.store()?;
    let mut imported = 0;
    for path in &transcripts {
        imported += import_transcript(ctx, &store, path)?;
    }
    Ok((transcripts.len(), imported))
}

/// Repeats [`watch_pass`] every `poll` until `stop` returns true.
///
/// Public so integration tests can inject a short poll and a stop flag. The command
/// passes [`WATCH_POLL`] and a flag set by SIGINT/SIGTERM, so Ctrl-C ends the loop
/// after the pass in flight and returns `0`.
#[doc(hidden)]
pub fn watch_until(
    ctx: &Ctx,
    dir: &Path,
    poll: Duration,
    stop: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(usize, usize), CliError> {
    let mut imported = 0;
    let mut transcripts = 0;
    loop {
        if stop() {
            break;
        }
        let (count, events) = watch_pass(ctx, dir)?;
        transcripts = count;
        imported += events;
        if stop() {
            break;
        }
        wait_for_stop(poll, stop);
    }
    Ok((transcripts, imported))
}

/// Sleeps for up to `poll`, returning early when `stop` observes an interrupt.
fn wait_for_stop(poll: Duration, stop: &dyn Fn() -> bool) {
    let deadline = Instant::now() + poll;
    while !stop() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(WATCH_WAIT_SLICE.min(deadline - now));
    }
}

/// Registers SIGINT/SIGTERM to flip one interrupt flag.
fn interrupt_flag() -> Result<Arc<AtomicBool>, CliError> {
    let interrupted = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))?;
    }
    Ok(interrupted)
}

/// Imports one Claude transcript file into its one canonical session.
fn import_transcript(ctx: &Ctx, store: &Store, path: &Path) -> Result<usize, CliError> {
    let Some(native) = transcript_native_id(path)? else {
        return Ok(0);
    };
    let marker_key = format!("watch:{native}");
    let (mut cursor, legacy_marker) = load_cursor(store, &marker_key, path)?;
    let start_offset = cursor.offset;
    let batch = import_file(path, &mut cursor)?;
    let cursor_moved = cursor.offset != start_offset;
    if batch.entries.is_empty() {
        if cursor_moved || legacy_marker {
            save_cursor(store, &marker_key, &cursor)?;
        }
        return Ok(0);
    }

    let session = canonical_session(ctx, store, &native, &batch.entries)?;
    let log =
        SessionLog::open(ctx.paths(), &session.id, ctx.deps().clock.clone()).map_err(vowp_core::CoreError::from)?;
    let harness = HarnessKind::ClaudeCode;
    let mut imported = 0;
    for line in &batch.lines {
        for entry in &line.entries {
            // The native-id map makes a re-read idempotent, including after a crash
            // before the cursor was saved or a file that was rewritten in place.
            if store.native_event(&session.id, harness, &entry.native_id)?.is_some() {
                continue;
            }
            let Some((body, actor)) = transcript_event(entry) else {
                continue;
            };
            let appended = log
                .append(vowp_store::session_log::NewEvent {
                    actor,
                    source: EventSource {
                        harness,
                        model: Some(CLAUDE_MODEL.to_owned()),
                        node: Some(ctx.config().node.name.clone()),
                        native_session_id: Some(native.clone()),
                    },
                    body,
                    raw: Some(line.value.clone()),
                    ts: None,
                })
                .map_err(vowp_core::CoreError::from)?;
            store.index_event(&vowp_store::session_log::index_row(&appended))?;
            store.insert_native_event(
                &session.id,
                harness,
                &entry.native_id,
                &appended.envelope.id,
                appended.envelope.ts,
            )?;
            imported += 1;
        }
    }
    log.sync().map_err(vowp_core::CoreError::from)?;
    if cursor_moved || imported > 0 || legacy_marker {
        save_cursor(store, &marker_key, &cursor)?;
    }
    Ok(imported)
}

/// Returns the Claude native session uuid from a transcript's JSON lines.
fn transcript_native_id(path: &Path) -> Result<Option<String>, CliError> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let native = value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !native.is_empty() {
            return Ok(Some(native.to_owned()));
        }
    }
}

/// Loads a cursor and reports whether the stored marker is not already a JSON cursor.
///
/// The value is JSON under `watch:<native>`. A bare integer is the v0.1 line count:
/// it is converted to the byte offset after that many complete lines so an upgrade
/// does not re-import the prefix. A line count past the end of a truncated file
/// starts over from zero. An unreadable marker starts over and is rewritten.
fn load_cursor(store: &Store, key: &str, path: &Path) -> Result<(ImportCursor, bool), CliError> {
    let Some(value) = store.kv_get(key)? else {
        return Ok((ImportCursor::default(), false));
    };
    let value = value.trim();
    if value.starts_with('{') {
        return Ok((serde_json::from_str(value).unwrap_or_default(), true));
    }
    let Ok(lines) = value.parse::<usize>() else {
        return Ok((ImportCursor::default(), true));
    };
    Ok((legacy_cursor(path, lines)?, true))
}

/// Converts a v0.1 processed-line count into a byte cursor.
fn legacy_cursor(path: &Path, lines: usize) -> Result<ImportCursor, CliError> {
    if lines == 0 {
        return Ok(ImportCursor::default());
    }
    let bytes = std::fs::read(path)?;
    let mut offset = 0usize;
    let mut last_uuid = None;
    for _ in 0..lines {
        let Some(relative) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
            return Ok(ImportCursor::default());
        };
        let end = offset + relative;
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes[offset..end]) {
            if let Some(uuid) = line_uuid(&value) {
                last_uuid = Some(uuid);
            }
        }
        offset = end + 1;
    }
    Ok(ImportCursor {
        offset: u64::try_from(offset).map_err(|_| CliError::Invalid("transcript is too large".to_owned()))?,
        last_uuid,
    })
}

/// Persists one cursor in the node database.
fn save_cursor(store: &Store, key: &str, cursor: &ImportCursor) -> Result<(), CliError> {
    let value = serde_json::to_string(cursor)
        .map_err(|error| CliError::Invalid(format!("cannot encode watch cursor: {error}")))?;
    store.kv_set(key, &value)?;
    Ok(())
}

/// Finds the one canonical session for a Claude uuid, creating it on the first pass.
fn canonical_session(ctx: &Ctx, store: &Store, native: &str, entries: &[TranscriptEntry]) -> Result<Session, CliError> {
    if let Some(session) = store.session_for_native(HarnessKind::ClaudeCode, native)? {
        return Ok(session);
    }
    let title = entries
        .iter()
        .find_map(|entry| match entry.kind {
            TranscriptKind::UserMessage => entry
                .text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .map(|text| text.chars().take(60).collect::<String>()),
            _ => None,
        })
        .unwrap_or_else(|| format!("claude {native}"));
    let service = SessionService::new(ctx.deps().clone());
    let id = service.create_session(NewSession {
        project: Some(ctx.project().clone()),
        job_id: None,
        role: SessionRole::User,
        title,
        cwd: ctx.project_root().to_path_buf(),
        parent: None,
        executor: Some(Executor::parse(CLAUDE_EXECUTOR)?),
    })?;
    let session = store.require_session(&id)?;
    store.upsert_native(
        &id,
        HarnessKind::ClaudeCode,
        native,
        &ctx.config().node.name,
        session.last_seq.saturating_add(1),
    )?;
    Ok(session)
}

/// Maps one transcript entry to a canonical event body and actor.
fn transcript_event(entry: &TranscriptEntry) -> Option<(EventBody, Actor)> {
    let mapped = match entry.kind {
        TranscriptKind::UserMessage => (
            EventBody::UserMessage(UserMessage {
                text: entry.text.clone()?,
                attachments: Vec::new(),
                note: None,
            }),
            Actor::User,
        ),
        TranscriptKind::AssistantMessage => (
            EventBody::AssistantMessage(AssistantMessage {
                text: entry.text.clone()?,
            }),
            Actor::Assistant,
        ),
        TranscriptKind::Reasoning => (
            EventBody::AssistantReasoning(AssistantReasoning {
                text: entry.text.clone()?,
            }),
            Actor::Assistant,
        ),
        TranscriptKind::ToolCall => (
            EventBody::ToolCall(ToolCall {
                call_id: entry.call_id.clone().unwrap_or_else(|| entry.native_id.clone()),
                name: entry.tool.clone().unwrap_or_else(|| "tool".to_owned()),
                input: entry.input.clone().unwrap_or(serde_json::Value::Null),
            }),
            Actor::Assistant,
        ),
        TranscriptKind::ToolResult => (
            EventBody::ToolResult(ToolResult {
                call_id: entry.call_id.clone().unwrap_or_else(|| entry.native_id.clone()),
                ok: entry.ok.unwrap_or(true),
                output: entry.output.clone(),
                output_ref: None,
                exit_code: None,
            }),
            Actor::Tool,
        ),
        TranscriptKind::Cost => {
            let tokens = entry.tokens.unwrap_or_default();
            let cost = match entry.usd {
                Some(usd) => Cost::receipt(usd, tokens),
                None => Cost {
                    usd: None,
                    tokens,
                    source: CostSource::Unknown,
                },
            };
            (EventBody::Cost(CostEvent { cost, run_id: None }), Actor::System)
        }
        TranscriptKind::TurnStarted | TranscriptKind::TurnEnded | TranscriptKind::Error | TranscriptKind::Note => {
            return None;
        }
    };
    Some(mapped)
}

/// Prints a session's event log as JSONL or pretty lines.
fn tail(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionTailArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.id)?;
    if let Some(capsule_id) = compacted_capsule(ctx, &store, &session)? {
        return Err(CliError::NotFound(compacted_message(&capsule_id)));
    }
    let reader = SessionLog::open_read(ctx.paths(), &session.id).map_err(vowp_core::CoreError::from)?;
    let after = args.from.unwrap_or(0);
    let mut last = after;
    loop {
        let events = reader.read_from(last, None).map_err(vowp_core::CoreError::from)?;
        for event in &events {
            if args.pretty {
                writeln!(io.out(), "{}", pretty_event(event, ctx.tz_offset_minutes()))?;
            } else {
                writeln!(
                    io.out(),
                    "{}",
                    serde_json::to_string(event).map_err(vowp_protocol::ProtocolError::from)?
                )?;
            }
            last = last.max(event.seq);
        }
        io.out().flush()?;
        if !args.follow {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(0)
}

/// Formats one event as `HH:MM:SS #seq type  summary`.
fn pretty_event(event: &EventEnvelope, offset_minutes: i32) -> String {
    let summary = event
        .body()
        .map(|body| body.summary())
        .unwrap_or_else(|_| event.payload.to_string());
    let summary = summary.lines().next().unwrap_or_default();
    let summary: String = if summary.chars().count() > 160 {
        let mut clipped: String = summary.chars().take(159).collect();
        clipped.push('…');
        clipped
    } else {
        summary.to_owned()
    };
    format!(
        "{} #{} {}  {}",
        clock_time(event.ts, offset_minutes),
        event.seq,
        event.event_type.as_str(),
        summary
    )
}

/// Formats a timestamp as `HH:MM:SS` in a fixed offset.
fn clock_time(ts: Timestamp, offset_minutes: i32) -> String {
    let Ok(base) = OffsetDateTime::from_unix_timestamp(ts.unix_secs()) else {
        return ts.as_rfc3339();
    };
    let Ok(offset) = time::UtcOffset::from_whole_seconds(offset_minutes.saturating_mul(60)) else {
        return ts.as_rfc3339();
    };
    base.to_offset(offset)
        .format(format_description!("[hour]:[minute]:[second]"))
        .unwrap_or_else(|_| ts.as_rfc3339())
}

/// Returns the last indexed event of a session.
fn last_event(store: &Store, session: &Session) -> Option<vowp_protocol::EventIndexRow> {
    if session.last_seq == 0 {
        return None;
    }
    store
        .event_rows(&session.id, session.last_seq.saturating_sub(1), 1)
        .ok()
        .and_then(|mut rows| rows.pop())
}

/// Returns the on-disk capsule file matching `capsule_id`.
fn capsule_path(ctx: &Ctx, session: &SessionId, capsule_id: &vowp_protocol::CapsuleId) -> Option<PathBuf> {
    let files = vowp_store::session_files::list_capsule_files(ctx.paths(), session).ok()?;
    files.into_iter().find(|path| {
        vowp_store::session_files::read_capsule_file(path)
            .map(|capsule| &capsule.capsule_id == capsule_id)
            .unwrap_or(false)
    })
}

/// Resolves a session id or unique prefix.
fn resolve_session(store: &Store, input: &str) -> Result<Session, CliError> {
    if let Ok(id) = SessionId::parse(input)
        && let Some(session) = store.session(&id)?
    {
        return Ok(session);
    }
    if input.len() < 6 {
        return Err(CliError::Invalid(
            "session id prefix must be at least 6 characters".to_owned(),
        ));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<Session> = store
        .list_sessions(&SessionFilter::default())?
        .into_iter()
        .filter(|session| session.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [session] => Ok(session.clone()),
        [] => Err(CliError::NotFound(format!("session {input}"))),
        many => Err(CliError::Invalid(format!(
            "ambiguous session prefix {input}: {}",
            many.iter()
                .map(|session| session.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Parses a session state name.
fn parse_state(value: &str) -> Result<SessionState, CliError> {
    match value {
        "idle" => Ok(SessionState::Idle),
        "running" => Ok(SessionState::Running),
        "blocked" => Ok(SessionState::Blocked),
        "switching" => Ok(SessionState::Switching),
        "closed" => Ok(SessionState::Closed),
        other => Err(CliError::Invalid(format!("unknown session state: {other}"))),
    }
}

/// Returns the wire name of a session state.
fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Running => "running",
        SessionState::Blocked => "blocked",
        SessionState::Switching => "switching",
        SessionState::Closed => "closed",
    }
}

/// Parses a switch reason.
fn parse_reason(value: Option<&str>) -> Result<SwitchReason, CliError> {
    match value.unwrap_or("user") {
        "user" => Ok(SwitchReason::User),
        "limit" => Ok(SwitchReason::Limit),
        "crash" => Ok(SwitchReason::Crash),
        "context" => Ok(SwitchReason::Context),
        "failover" => Ok(SwitchReason::Failover),
        "probe_back" | "probe-back" => Ok(SwitchReason::ProbeBack),
        other => Err(CliError::Invalid(format!("unknown switch reason: {other}"))),
    }
}
