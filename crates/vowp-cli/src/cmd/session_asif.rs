//! ASIF export and import for `vowp session export|import --format asif`.
//!
//! Spec: `docs/design/v0.4/asif-integration.md` §3 and §8 Job B. The native bundle
//! path in [`super::session`] is unchanged. This module owns the ASIF flag validation,
//! default naming, stdout/stdin handling, redaction literals, report/strict/allow-loss
//! policy, the hidden remote export stream and the remote import relay.
//!
//! ASIF 0.4 is a review draft: every command reports declared losses and unsupported
//! VOWP operations, and none of them claims lossless interchange or continuation.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command as ProcessCommand, Stdio};

use serde::{Deserialize, Serialize};
use vowp_core::CoreError;
use vowp_core::session::asif::{
    AsifExportOptions, AsifImportOptions, MAX_ASIF_ARCHIVE_BYTES, export_asif, import_asif,
};
use vowp_protocol::{AsifImportResponse, AsifReport, SessionId};
use vowp_store::NodePaths;

use crate::cli::{SessionExportArgs, SessionFormat, SessionImportArgs};
use crate::output::print_json;
use crate::{CliError, Ctx, IoStreams};

/// Format marker in the hidden remote export stream envelope.
const STREAM_FORMAT: &str = "vowp-asif-export-stream-v1";

/// Upper bound for the one JSON envelope line the remote stream prints first.
///
/// It matches the adapter's JSON limit (16 MiB) plus 4 KiB of envelope fields.
const MAX_STREAM_LINE_BYTES: usize = 16 * 1024 * 1024 + 4096;

/// Upper bound for relaying a failed remote stream's stdout as an error message.
const MAX_REMOTE_ERROR_BYTES: u64 = 1024 * 1024;

/// One private temporary directory under `<home>/tmp`, removed on drop.
struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// Creates a fresh 0700 directory below the node's `tmp` directory.
    fn new(paths: &NodePaths, prefix: &str) -> Result<Self, CliError> {
        let base = paths.tmp_dir();
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            path: super::session::private_dir_in(&base, prefix)?,
        })
    }

    /// Returns the directory path.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// One staged stdin document, removed on drop.
struct StagedInput {
    _dir: PrivateDir,
    path: PathBuf,
}

impl StagedInput {
    /// Writes `bytes` to a 0600 file in a fresh private directory.
    fn write(ctx: &Ctx, bytes: &[u8]) -> Result<Self, CliError> {
        let dir = PrivateDir::new(ctx.paths(), "vowp-asif-input")?;
        // Keep a JSON extension on a standalone document so the branch-id enrichment can
        // read it; a ZIP package is identified by its magic instead.
        let name = if bytes.starts_with(b"PK\x03\x04") {
            "input.asif"
        } else {
            "input.json"
        };
        let path = dir.path().join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(Self { _dir: dir, path })
    }

    /// Returns the staged path.
    fn path(&self) -> &Path {
        &self.path
    }
}

/// JSON shape of `vowp session export --format asif --json`.
#[derive(Debug, Serialize)]
struct AsifExportOutput {
    format: &'static str,
    path: String,
    session: SessionId,
    bytes: u64,
    report_path: Option<String>,
    report: AsifReport,
}

/// One remote export stream envelope, followed by exactly `bytes` package bytes.
#[derive(Debug, Serialize, Deserialize)]
struct AsifExportStream {
    format: String,
    session: SessionId,
    bytes: u64,
    sha256: String,
    report: AsifReport,
}

/// Rejects flags that are invalid with the ASIF export contract.
fn validate_export_flags(args: &SessionExportArgs) -> Result<(), CliError> {
    if args.format != SessionFormat::Asif {
        return Err(CliError::Invalid("the ASIF exporter requires --format asif".to_owned()));
    }
    if args.tail.is_some() {
        return Err(CliError::Invalid("--tail is not valid with --format asif".to_owned()));
    }
    if args.report.as_deref() == Some(Path::new("-")) {
        return Err(CliError::Invalid(
            "--report - is not supported; pass a file path".to_owned(),
        ));
    }
    if args.asif_stream {
        if args.out.as_deref() != Some(Path::new("-")) {
            return Err(CliError::Invalid("--asif-stream requires --out -".to_owned()));
        }
        if args.report.is_some() {
            return Err(CliError::Invalid("--report is not valid with --asif-stream".to_owned()));
        }
    }
    Ok(())
}

/// Rejects flags that are invalid with the ASIF import contract.
fn validate_import_flags(args: &SessionImportArgs) -> Result<(), CliError> {
    if args.format != SessionFormat::Asif {
        return Err(CliError::Invalid("the ASIF importer requires --format asif".to_owned()));
    }
    if args.same_id {
        return Err(CliError::Invalid(
            "--same-id is not valid with --format asif; ASIF import always creates a new session".to_owned(),
        ));
    }
    if args.executor.is_some() {
        return Err(CliError::Invalid(
            "--executor is not valid with --format asif; executor evidence comes from the document".to_owned(),
        ));
    }
    if args.report.as_deref() == Some(Path::new("-")) {
        return Err(CliError::Invalid(
            "--report - is not supported; pass a file path".to_owned(),
        ));
    }
    Ok(())
}

/// Reads `--redact` values and every `--redact-file` literal without echoing them.
fn redact_literals(args: &SessionExportArgs) -> Result<Vec<String>, CliError> {
    let mut literals = args.redact.clone();
    for path in &args.redact_file {
        let text = std::fs::read_to_string(path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                "cannot read a --redact-file literal list (the path and value are not printed)",
            )
        })?;
        for line in text.lines() {
            if !line.is_empty() {
                literals.push(line.to_owned());
            }
        }
    }
    Ok(literals)
}

/// Copies one file to a writer without changing it.
fn copy_file_to(path: &Path, writer: &mut dyn Write) -> Result<(), CliError> {
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, writer)?;
    Ok(())
}

/// Prints the loss count and unique loss kinds to stderr.
fn print_loss_summary(io: &mut dyn IoStreams, report: &AsifReport) -> Result<(), CliError> {
    let mut kinds: Vec<&str> = Vec::new();
    for loss in &report.losses {
        if !kinds.contains(&loss.kind.as_str()) {
            kinds.push(&loss.kind);
        }
    }
    if kinds.is_empty() {
        writeln!(io.err(), "asif: no declared losses")?;
    } else {
        writeln!(
            io.err(),
            "asif: {} declared loss{}: {}",
            report.loss_count,
            if report.loss_count == 1 { "" } else { "es" },
            kinds.join(", ")
        )?;
    }
    if !report.unsupported.is_empty() {
        writeln!(io.err(), "asif: not captured: {}", report.unsupported.join(", "))?;
    }
    Ok(())
}

/// Prints the reindex warnings that are not ASIF losses.
fn print_warnings(io: &mut dyn IoStreams, warnings: &[String]) -> Result<(), CliError> {
    for warning in warnings {
        writeln!(io.err(), "warning: {warning}")?;
    }
    Ok(())
}

/// Exports one session through the core ASIF facade.
pub fn export(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionExportArgs) -> Result<i32, CliError> {
    validate_export_flags(args)?;
    if args.out.as_deref() == Some(Path::new("-")) {
        if io.is_tty() {
            return Err(CliError::Invalid(
                "refusing to write a binary ASIF package to a terminal; pass --out <file> or redirect stdout"
                    .to_owned(),
            ));
        }
        if ctx.json() && !args.asif_stream {
            return Err(CliError::Invalid(
                "--json conflicts with --out - for --format asif".to_owned(),
            ));
        }
    }
    let store = ctx.store()?;
    let session = super::session::resolve_session(&store, &args.id)?;
    drop(store);
    let redact = redact_literals(args)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.asif.zip", session.id)));
    let stdout = out == Path::new("-");
    if args.asif_stream {
        return stream_local(ctx, io, &session.id, args, &redact);
    }

    let options = AsifExportOptions {
        redact,
        keep_host_paths: args.keep_host_paths,
        strict: args.strict,
        // Strict mode asks the adapter to declare recoverable integrity/ambiguity
        // problems instead of refusing them, then the core filter refuses any
        // Blocking/Integrity loss. Without this, those problems would never reach the
        // strict filter and the mode could not inspect them.
        allow_loss: args.allow_loss || args.strict,
        report_path: args.report.clone(),
    };
    if stdout {
        let dir = PrivateDir::new(ctx.paths(), "vowp-asif-export")?;
        let package = dir.path().join("session.asif.zip");
        let exported = export_asif(ctx.paths(), &session.id, &package, &options)?;
        copy_file_to(&exported.output, io.out())?;
        print_loss_summary(io, &exported.report)?;
        return Ok(0);
    }

    let exported = export_asif(ctx.paths(), &session.id, &out, &options)?;
    if ctx.json() {
        print_json(
            io,
            &AsifExportOutput {
                format: "asif",
                path: out.display().to_string(),
                session: session.id.clone(),
                bytes: exported.bytes,
                report_path: args.report.as_ref().map(|path| path.display().to_string()),
                report: exported.report,
            },
        )?;
    } else {
        writeln!(io.out(), "{}", out.display())?;
        print_loss_summary(io, &exported.report)?;
    }
    Ok(0)
}

/// Writes the hidden one-line envelope and the exact package bytes.
fn stream_local(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    session: &SessionId,
    args: &SessionExportArgs,
    redact: &[String],
) -> Result<i32, CliError> {
    let dir = PrivateDir::new(ctx.paths(), "vowp-asif-stream")?;
    let package = dir.path().join("session.asif.zip");
    let exported = export_asif(
        ctx.paths(),
        session,
        &package,
        &AsifExportOptions {
            redact: redact.to_vec(),
            keep_host_paths: args.keep_host_paths,
            strict: args.strict,
            allow_loss: args.allow_loss || args.strict,
            report_path: None,
        },
    )?;
    let sha256 = vowp_store::blobs::sha256_file(&package)?;
    let envelope = AsifExportStream {
        format: STREAM_FORMAT.to_owned(),
        session: session.clone(),
        bytes: exported.bytes,
        sha256,
        report: exported.report,
    };
    let line = serde_json::to_string(&envelope).map_err(vowp_protocol::ProtocolError::from)?;
    writeln!(io.out(), "{line}")?;
    copy_file_to(&package, io.out())?;
    Ok(0)
}

/// Imports one ASIF package, standalone document or directory into a new session.
pub fn import(ctx: &Ctx, io: &mut dyn IoStreams, args: &SessionImportArgs) -> Result<i32, CliError> {
    validate_import_flags(args)?;
    let staged = if args.file == "-" {
        let bytes = read_stdin_bytes(io, MAX_ASIF_ARCHIVE_BYTES)?;
        Some(StagedInput::write(ctx, &bytes)?)
    } else {
        None
    };
    let input = staged
        .as_ref()
        .map(StagedInput::path)
        .unwrap_or_else(|| Path::new(&args.file));
    let cwd = match &args.cwd {
        Some(path) if path.is_dir() => path.clone(),
        Some(path) => {
            return Err(CliError::Invalid(format!(
                "--cwd {} is not a directory",
                path.display()
            )));
        }
        None => ctx.project_root().to_path_buf(),
    };
    let store = ctx.store()?;
    let imported = import_asif(
        ctx.paths(),
        &store,
        input,
        &AsifImportOptions {
            cwd,
            branch: args.branch.clone(),
            strict: args.strict,
            // See the export path: strict asks the adapter to declare recoverable
            // problems so the core filter can refuse them.
            allow_loss: args.allow_loss || args.strict,
            session_id: None,
            report_path: args.report.clone(),
        },
    )
    .map_err(|error| enrich_branch_error(error, input))?;
    let response = AsifImportResponse {
        session: imported.session,
        report: imported.report,
        warnings: imported.warnings,
    };
    if ctx.json() {
        print_json(io, &response)?;
    } else {
        writeln!(io.out(), "{}", response.session.id)?;
    }
    print_warnings(io, &response.warnings)?;
    if !ctx.json() {
        print_loss_summary(io, &response.report)?;
    }
    Ok(0)
}

/// Adds the available branch ids to a multi-branch refusal when the document is readable.
fn enrich_branch_error(error: CoreError, input: &Path) -> CliError {
    let CoreError::Asif { code, message, class } = error else {
        return CliError::Core(error);
    };
    if code != "BRANCH_AMBIGUOUS" {
        return CliError::Core(CoreError::Asif { code, message, class });
    }
    let message = match branch_ids(input) {
        Some(ids) if !ids.is_empty() => {
            format!("{message}; available branches: {}", ids.join(", "))
        }
        _ => format!("{message}; extract session.json from the package and list `.branches[].id`"),
    };
    CliError::Core(CoreError::Asif { code, message, class })
}

/// Reads branch ids from a standalone JSON document or a directory holding `session.json`.
///
/// A ZIP package is left to the adapter; this helper only enriches an error message and
/// never parses or re-implements the ASIF mapping.
fn branch_ids(input: &Path) -> Option<Vec<String>> {
    let path = if input.is_dir() {
        input.join("session.json")
    } else if input.extension().is_some_and(|extension| extension == "json") {
        input.to_path_buf()
    } else {
        return None;
    };
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let branches = value.get("branches")?.as_array()?;
    Some(
        branches
            .iter()
            .filter_map(|branch| branch.get("id")?.as_str().map(str::to_owned))
            .collect(),
    )
}

/// Reads stdin into memory, refusing a document over the adapter archive limit.
fn read_stdin_bytes(io: &mut dyn IoStreams, limit: u64) -> Result<Vec<u8>, CliError> {
    let mut bytes = Vec::new();
    let input = io.input();
    input.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(oversized_input(limit));
    }
    Ok(bytes)
}

/// Builds the documented oversized-input refusal.
fn oversized_input(limit: u64) -> CliError {
    CliError::Invalid(format!("the ASIF input exceeds the {limit}-byte archive limit"))
}

/// Imports a local ASIF input on a remote node, relaying its response.
pub fn remote_import(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    target: &str,
    args: &SessionImportArgs,
) -> Result<i32, CliError> {
    validate_import_flags(args)?;
    if args.report.is_some() {
        return Err(CliError::Invalid(
            "--report is not supported with --node; the full report stays on the node".to_owned(),
        ));
    }
    if args.file != "-" && Path::new(&args.file).is_dir() {
        return Err(CliError::Invalid(
            "--node with a directory is not supported; use a package file or stdin".to_owned(),
        ));
    }
    let mut remote = super::session::remote_prefix(ctx);
    remote.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "import".to_owned(),
        "-".to_owned(),
        "--format".to_owned(),
        "asif".to_owned(),
    ]);
    if let Some(branch) = &args.branch {
        remote.extend(["--branch".to_owned(), branch.clone()]);
    }
    if let Some(cwd) = &args.cwd {
        remote.extend(["--cwd".to_owned(), cwd.display().to_string()]);
    }
    if args.strict {
        remote.push("--strict".to_owned());
    }
    if args.allow_loss {
        remote.push("--allow-loss".to_owned());
    }
    let run = if args.file == "-" {
        let input = read_stdin_bytes(io, MAX_ASIF_ARCHIVE_BYTES)?;
        super::session::run_remote_capture_bytes(target, &remote, Some(&input))?
    } else {
        let path = Path::new(&args.file);
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_ASIF_ARCHIVE_BYTES {
            return Err(oversized_input(MAX_ASIF_ARCHIVE_BYTES));
        }
        let file = std::fs::File::open(path)?;
        super::session::run_remote_capture_stream(target, &remote, Some(Box::new(file)))?
    };
    if run.code != 0 {
        return relay_bytes(io, &run);
    }
    let response: AsifImportResponse = serde_json::from_slice(&run.stdout)
        .map_err(|error| CliError::Remote(format!("remote printed an invalid ASIF import response: {error}")))?;
    if ctx.json() {
        print_json(io, &response)?;
    } else {
        writeln!(io.out(), "{}", response.session.id)?;
    }
    print_warnings(io, &response.warnings)?;
    if !ctx.json() {
        print_loss_summary(io, &response.report)?;
    }
    Ok(0)
}

/// Writes a failed remote invocation's output and returns its exit code.
fn relay_bytes(io: &mut dyn IoStreams, run: &super::session::RemoteBytesRun) -> Result<i32, CliError> {
    if !run.stdout.is_empty() {
        io.out().write_all(&run.stdout)?;
    }
    if !run.stderr.is_empty() {
        io.err().write_all(run.stderr.as_bytes())?;
    }
    Ok(run.code)
}

/// Exports one local ASIF package through the remote hidden stream.
pub fn remote_export(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    target: &str,
    args: &SessionExportArgs,
) -> Result<i32, CliError> {
    validate_export_flags(args)?;
    if args.report.is_some() {
        return Err(CliError::Invalid(
            "--report is not supported with --node; the full report stays on the node".to_owned(),
        ));
    }
    let out = args.out.clone();
    if out.as_deref() == Some(Path::new("-")) && io.is_tty() {
        return Err(CliError::Invalid(
            "refusing to write a binary ASIF package to a terminal; pass --out <file> or redirect stdout".to_owned(),
        ));
    }
    if out.as_deref() == Some(Path::new("-")) && ctx.json() {
        return Err(CliError::Invalid(
            "--json conflicts with --out - for --format asif".to_owned(),
        ));
    }
    let mut remote = super::session::remote_prefix(ctx);
    remote.extend([
        "--json".to_owned(),
        "session".to_owned(),
        "export".to_owned(),
        args.id.clone(),
        "--format".to_owned(),
        "asif".to_owned(),
        "--out".to_owned(),
        "-".to_owned(),
        "--asif-stream".to_owned(),
    ]);
    for literal in redact_literals(args)? {
        remote.extend(["--redact".to_owned(), literal]);
    }
    if args.keep_host_paths {
        remote.push("--keep-host-paths".to_owned());
    }
    if args.strict {
        remote.push("--strict".to_owned());
    }
    if args.allow_loss {
        remote.push("--allow-loss".to_owned());
    }
    stream_remote_export(ctx, io, target, &remote, out.as_deref())
}

/// Reads and verifies the remote stream, then writes the package or the summary.
fn stream_remote_export(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    target: &str,
    remote: &[String],
    out: Option<&Path>,
) -> Result<i32, CliError> {
    let argv = crate::remote::build_ssh_argv(target, remote, false);
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| CliError::Remote("empty ssh argv".to_owned()))?;
    let mut child = ProcessCommand::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CliError::Remote(format!("ssh {target}: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Remote("remote stream has no stdout".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CliError::Remote("remote stream has no stderr".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let heading = read_limited_line(&mut reader, &mut line);
    match heading {
        Ok(()) => match serde_json::from_slice::<AsifExportStream>(&line) {
            Ok(envelope) => receive_stream(ctx, io, &mut child, &mut reader, stderr_reader, envelope, out),
            Err(_) => fail_stream(io, &mut child, &mut reader, stderr_reader, &line),
        },
        Err(_) => fail_stream(io, &mut child, &mut reader, stderr_reader, &[]),
    }
}

/// Reads one newline-terminated envelope line, bounded by the adapter JSON limit.
fn read_limited_line(reader: &mut BufReader<ChildStdout>, line: &mut Vec<u8>) -> Result<(), CliError> {
    let mut limited = reader.by_ref().take(MAX_STREAM_LINE_BYTES as u64);
    limited.read_until(b'\n', line)?;
    if line.is_empty() || !line.ends_with(b"\n") {
        return Err(CliError::Remote(
            "remote printed a malformed ASIF export stream heading".to_owned(),
        ));
    }
    line.pop();
    Ok(())
}

/// Streams exactly the announced bytes, verifies the hash and installs the package.
fn receive_stream(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    child: &mut std::process::Child,
    reader: &mut BufReader<ChildStdout>,
    stderr_reader: std::thread::JoinHandle<Vec<u8>>,
    envelope: AsifExportStream,
    out: Option<&Path>,
) -> Result<i32, CliError> {
    if envelope.format != STREAM_FORMAT
        || envelope.bytes > MAX_ASIF_ARCHIVE_BYTES
        || envelope.sha256.len() != 64
        || !envelope
            .sha256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Err(CliError::Remote(
            "remote printed an invalid ASIF export stream envelope".to_owned(),
        ));
    }
    let private = PrivateDir::new(ctx.paths(), "vowp-asif-receive")?;
    let destination = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}.asif.zip", envelope.session)));
    let stdout = destination == Path::new("-");
    let staged = if stdout {
        private.path().join("session.asif.zip")
    } else {
        if let Some(parent) = destination.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut name = destination.as_os_str().to_owned();
        name.push(".tmp");
        PathBuf::from(name)
    };
    let result = receive_bytes(reader, child, stderr_reader, &envelope, &staged);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    if stdout {
        copy_file_to(&staged, io.out())?;
        print_loss_summary(io, &envelope.report)?;
    } else {
        if let Err(error) = std::fs::rename(&staged, &destination) {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        if ctx.json() {
            print_json(
                io,
                &AsifExportOutput {
                    format: "asif",
                    path: destination.display().to_string(),
                    session: envelope.session,
                    bytes: envelope.bytes,
                    report_path: None,
                    report: envelope.report,
                },
            )?;
        } else {
            writeln!(io.out(), "{}", destination.display())?;
            print_loss_summary(io, &envelope.report)?;
        }
    }
    Ok(0)
}

/// Copies the announced bytes, checks for an extra byte, the exit code and the digest.
fn receive_bytes(
    reader: &mut BufReader<ChildStdout>,
    child: &mut std::process::Child,
    stderr_reader: std::thread::JoinHandle<Vec<u8>>,
    envelope: &AsifExportStream,
    staged: &Path,
) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(staged)?;
    let copied = std::io::copy(&mut reader.by_ref().take(envelope.bytes), &mut file)?;
    if copied != envelope.bytes {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Err(CliError::Remote(format!(
            "remote ASIF export stream ended early: expected {} bytes, got {copied}",
            envelope.bytes
        )));
    }
    file.sync_all()?;
    drop(file);
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Err(CliError::Remote(
            "remote ASIF export stream carried extra bytes".to_owned(),
        ));
    }
    let status = child
        .wait()
        .map_err(|error| CliError::Remote(format!("ssh: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CliError::Remote("remote stderr reader failed".to_owned()))?;
    let code = status
        .code()
        .ok_or_else(|| CliError::Remote("remote command terminated by a signal".to_owned()))?;
    if code == 255 {
        return Err(CliError::Remote("ssh failed with exit 255".to_owned()));
    }
    if code != 0 {
        return Err(CliError::Remote(format!(
            "remote ASIF export failed with exit {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    let actual = vowp_store::blobs::sha256_file(staged)?;
    if actual != envelope.sha256 {
        return Err(CliError::Remote(
            "remote ASIF export stream digest does not match its envelope".to_owned(),
        ));
    }
    Ok(())
}

/// Drains an unrecognised remote stream, relays its error output and returns its exit code.
fn fail_stream(
    io: &mut dyn IoStreams,
    child: &mut std::process::Child,
    reader: &mut BufReader<ChildStdout>,
    stderr_reader: std::thread::JoinHandle<Vec<u8>>,
    heading: &[u8],
) -> Result<i32, CliError> {
    let mut rest = Vec::new();
    reader.by_ref().take(MAX_REMOTE_ERROR_BYTES).read_to_end(&mut rest)?;
    std::io::copy(&mut reader.by_ref(), &mut std::io::sink())?;
    let status = child
        .wait()
        .map_err(|error| CliError::Remote(format!("ssh: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CliError::Remote("remote stderr reader failed".to_owned()))?;
    let code = status
        .code()
        .ok_or_else(|| CliError::Remote("remote command terminated by a signal".to_owned()))?;
    if code == 255 {
        return Err(CliError::Remote("ssh failed with exit 255".to_owned()));
    }
    if code != 0 {
        if !heading.is_empty() {
            io.err().write_all(heading)?;
            writeln!(io.err())?;
        }
        if !rest.is_empty() {
            io.err().write_all(&rest)?;
        }
        if !stderr.is_empty() {
            io.err().write_all(&stderr)?;
        }
        return Ok(code);
    }
    Err(CliError::Remote(
        "remote printed a malformed ASIF export stream".to_owned(),
    ))
}
