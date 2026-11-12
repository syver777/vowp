//! `vowp up` and `vowp down`: tmux layout, the node supervisor lifecycle and the stop file.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §4 and `docs/design/cli.md` §2.
//! `up --once` runs startup plus exactly one node tick (tests and cron); `--foreground`
//! runs the loop in this process and `--detach` (the default) re-executes the binary
//! with `--foreground`. The supervisor is a [`NodeSupervisor`]: one process ticks every
//! active project in round-robin order.
//!
//! `down` writes `<home>/stop` and waits for the lock to disappear. The foreground
//! supervisor watches that file and SIGTERM/SIGINT, interrupts its runners, waits for
//! adapter/gate children and marks the heartbeat stopped before it exits.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use serde::Serialize;
use vowp_adapters::proc::{process_alive, valid_pid};
use vowp_core::Deps;
use vowp_core::loops::{NodeReconcileReport, NodeSupervisor, NodeTickReport};
use vowp_protocol::{Clock, Level, Log, ProjectName, ProjectState, Timestamp};
use vowp_store::{MigrationReport, NodePaths, Store, StoreError};
use vowp_tmux::{LayoutSpec, TmuxError, WindowManager, WindowSpec, session_name_for_project};

use crate::cli::{DownArgs, Global, UpArgs, UpCommand};
use crate::{CliError, Ctx, IoStreams, signals};

/// How long `down` waits for a cooperative stop before reporting a conflict.
const DOWN_WAIT: Duration = Duration::from_secs(30);

/// How long `down --force` waits after SIGTERM before giving up.
///
/// Longer than the child cleanup window in a foreground supervisor, so a cooperative
/// shutdown is not reported as a conflict while it is still stopping adapters.
const FORCE_WAIT: Duration = Duration::from_secs(10);

/// How long a foreground supervisor waits for its adapter/gate children to exit.
const CHILD_EXIT_WAIT: Duration = Duration::from_secs(6);

/// Grace after SIGTERM before a direct child is killed with SIGKILL.
const CHILD_KILL_WAIT: Duration = Duration::from_secs(2);

/// How long the supervisor waits for the optional daemon to report its bound address.
///
/// The daemon binds `127.0.0.1:0`, opens the existing home and prints exactly one line.
/// A daemon that does not get that far must not hold the supervisor's startup open.
const DAEMON_START_WAIT: Duration = Duration::from_secs(5);

/// Grace after SIGTERM before the daemon child is killed with SIGKILL.
const DAEMON_STOP_WAIT: Duration = Duration::from_secs(5);

/// How long a detached `up` waits for the foreground supervisor's window address.
///
/// The foreground child starts the daemon after it owns the lock; the daemon normally
/// reports the address in milliseconds, so this only covers a slow first store open.
const WEB_WINDOW_WAIT: Duration = Duration::from_secs(3);

/// Runs `vowp up` or `vowp down` before a project context exists.
///
/// Both commands are node-scoped: they must work from `~/.vowp` with no checkout, on a
/// home that still has the v0.1 schema (which [`Ctx::build`] cannot open), and on a home
/// with several projects. `up` refuses on a configured control machine, migrates the home
/// and registers a checkout the caller stands in, then starts the one node supervisor for
/// every active project.
pub fn run_node(global: &Global, io: &mut dyn IoStreams, command: &UpCommand) -> Result<i32, CliError> {
    let paths = crate::ctx::node_paths(global)?;
    crate::ctx::require_initialised_home(&paths)?;
    match command {
        UpCommand::Up(args) => {
            if crate::node_ctx::is_control_machine()? {
                return Err(CliError::Guard(
                    "this is a control machine; the home runs the supervisor".to_owned(),
                ));
            }
            let deps = supervisor_deps(crate::node_ctx::home_deps(&paths)?, &paths);
            run_up(global, &paths, &deps, io, args)
        }
        UpCommand::Down(args) => {
            let deps = crate::node_ctx::home_shell_deps(&paths)?;
            run_down(&paths, &deps, global.json, io, args)
        }
    }
}

/// Runs `vowp up` or `vowp down` with a project context.
///
/// Kept for in-process callers that already built a [`Ctx`]; the CLI dispatches
/// node-scoped commands through [`run_node`] so a v0.1 home never reaches [`Ctx::build`].
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &UpCommand) -> Result<i32, CliError> {
    let deps = supervisor_deps(ctx.deps().clone(), ctx.paths());
    match command {
        UpCommand::Up(args) => {
            if crate::node_ctx::is_control_machine()? {
                return Err(CliError::Guard(
                    "this is a control machine; the home runs the supervisor".to_owned(),
                ));
            }
            let global = Global {
                json: ctx.json(),
                home: Some(ctx.paths().root().to_path_buf()),
                project: Some(ctx.project_root().display().to_string()),
                ..Global::default()
            };
            run_up(&global, ctx.paths(), &deps, io, args)
        }
        UpCommand::Down(args) => run_down(ctx.paths(), &deps, ctx.json(), io, args),
    }
}

/// Registers the selected checkout when `up` is run in one, keeping v0.1 compatibility.
fn register_checkout_for_up(paths: &NodePaths, deps: &Deps, checkout: Option<&Path>) -> Result<(), CliError> {
    let Some(root) = checkout else {
        return Ok(());
    };
    let store = Store::open_global(paths)?;
    if vowp_core::loops::registry::project_by_path(&store, root)?.is_some() {
        return Ok(());
    }
    crate::cmd::project::register_checkout(paths, deps, root, None, None)?;
    Ok(())
}

/// Returns `<home>/web-window`, the address reported by the supervisor's daemon child.
///
/// The foreground supervisor writes it so a detached `vowp up` can print the same line;
/// it is removed when the daemon stops or before a new supervisor starts.
fn web_window_file(paths: &NodePaths) -> PathBuf {
    paths.root().join("web-window")
}

/// Parses the daemon's one startup line into a printable loopback URL.
///
/// `vowp-daemon` binds `127.0.0.1` only and prints `vowp-daemon: listening on http://…`;
/// anything else is ignored rather than surfaced as a window address.
fn parse_web_window_address(line: &str) -> Option<String> {
    let address = line.strip_prefix("vowp-daemon: listening on ")?.trim();
    if !address.starts_with("http://127.0.0.1:") {
        return None;
    }
    Some(format!("{}/", address.trim_end_matches('/')))
}

/// Returns the loopback port from a printed window address.
///
/// [`parse_web_window_address`] only admits `http://127.0.0.1:<port>/`, so a malformed
/// tail means no hint rather than a guessed port.
fn web_window_port(address: &str) -> Option<u16> {
    let port = address.strip_prefix("http://127.0.0.1:")?.trim_end_matches('/');
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

/// Prints the window URL and the control-machine tunnel command for its actual port.
///
/// The local port is chosen by `vowp tunnel open` on the control machine, so the second
/// line shows the local URL shape instead of inventing a port. An address without a
/// usable port prints only the window URL.
fn print_web_window(io: &mut dyn IoStreams, address: &str) -> Result<(), CliError> {
    writeln!(io.out(), "web window: {address}")?;
    if let Some(port) = web_window_port(address) {
        writeln!(io.out(), "remote window: vowp tunnel open window --remote-port {port}")?;
        writeln!(
            io.out(),
            "  local URL: http://127.0.0.1:<local-port>/ (the command prints the chosen port)"
        )?;
    }
    Ok(())
}

/// Selects the one project the optional web window serves.
///
/// A checkout named by `--project` (or the current directory) wins. Without one, the home
/// must have exactly one non-archived project: the supervisor serves every project, but the
/// window serves one, so an ambiguous home leaves the window off instead of guessing.
fn select_daemon_project(paths: &NodePaths, checkout: Option<&Path>) -> Result<Option<ProjectName>, CliError> {
    let store = Store::open_global(paths)?;
    if let Some(root) = checkout {
        if let Some(entry) = vowp_core::loops::registry::project_by_path(&store, root)? {
            if entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT {
                return Ok(Some(entry.name));
            }
        }
    }
    let active: Vec<_> = vowp_core::loops::registry::project_entries(&store)?
        .into_iter()
        .filter(|entry| {
            entry.state != ProjectState::Archived && entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT
        })
        .collect();
    Ok(match active.as_slice() {
        [only] => Some(only.name.clone()),
        _ => None,
    })
}

/// Returns the daemon binary: `VOWP_DAEMON_BIN`, the `vowp` sibling, then `PATH`.
///
/// `VOWP_DAEMON_BIN` is an explicit override for tests and unusual installs; normal runs
/// keep the two shipped binaries together, so the sibling lookup wins before `PATH`.
fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("VOWP_DAEMON_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            let sibling = directory.join("vowp-daemon");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("vowp-daemon")
}

/// The optional `vowp-daemon` child that serves the web window.
///
/// The child is a direct child of the foreground supervisor: it inherits
/// `VOWP_API_TOKEN`, opens its own read-only store and keeps its read/write split. The
/// supervisor's lock remains the only home lock. Stopping the supervisor stops this child.
struct WebWindow {
    child: Option<Child>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    address_file: PathBuf,
    address: Option<String>,
}

impl WebWindow {
    /// Returns the printable loopback URL, when the child reported one.
    fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// Stops the child with SIGTERM, escalates to SIGKILL and reaps it.
    ///
    /// The daemon shares the supervisor's process group, so a terminal Ctrl-C or a
    /// control-group stop reaches it too; the explicit TERM covers `vowp down` and a signal
    /// sent to the supervisor pid alone. Waiting for the child is what keeps `vowp up` from
    /// leaving an orphan behind.
    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = send_term(child.id());
        let deadline = Instant::now() + DAEMON_STOP_WAIT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    break;
                }
                Err(_) => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let _ = child.wait();
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_file(&self.address_file);
    }
}

impl Drop for WebWindow {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the optional web-window daemon and waits briefly for its address.
///
/// A missing or unstartable binary prints a warning and returns `None`; the supervisor is
/// not optional. `io` receives that warning, while the daemon's own stderr is forwarded to
/// this process's stderr with the token value and home path redacted.
fn start_web_window(paths: &NodePaths, project: &ProjectName, io: &mut dyn IoStreams) -> Option<WebWindow> {
    let binary = daemon_binary();
    let address_file = web_window_file(paths);
    let _ = std::fs::remove_file(&address_file);

    let mut command = Command::new(&binary);
    command
        .arg("--home")
        .arg(paths.root())
        .arg("--project")
        .arg(project.as_str())
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = writeln!(
                io.err(),
                "warning: web window not started: cannot run {}: {error}",
                binary.display()
            );
            return None;
        }
    };

    let token = std::env::var("VOWP_API_TOKEN").ok().filter(|value| !value.is_empty());
    let home_text = paths.root().display().to_string();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader_file = address_file.clone();
    let stdout_reader = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Some(address) = parse_web_window_address(&line) {
                    let _ = std::fs::write(&reader_file, format!("{address}\n"));
                    let _ = sender.send(address);
                }
            }
        })
    });
    let stderr_reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(mut line) = line else {
                    break;
                };
                if let Some(token) = token.as_deref() {
                    line = line.replace(token, "[redacted]");
                }
                line = line.replace(&home_text, "[home]");
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        })
    });

    let address = match receiver.recv_timeout(DAEMON_START_WAIT) {
        Ok(address) => Some(address),
        Err(RecvTimeoutError::Timeout) => {
            let _ = writeln!(
                io.err(),
                "warning: the web window daemon did not report an address; continuing without it"
            );
            None
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = writeln!(
                io.err(),
                "warning: the web window daemon exited before reporting an address; continuing without the window"
            );
            None
        }
    };

    Some(WebWindow {
        child: Some(child),
        stdout_reader,
        stderr_reader,
        address_file,
        address,
    })
}

/// Waits for a detached supervisor's foreground child to publish the window address.
fn wait_for_web_window(paths: &NodePaths, timeout: Duration) -> Option<String> {
    let path = web_window_file(paths);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(address) = text.lines().next().map(str::trim).filter(|line| !line.is_empty()) {
                return Some(address.to_owned());
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Runs one node tick with the supplied dependencies.
///
/// This is the seam `up --once` uses and the one the hermetic test drives with fake
/// adapters, a temporary node home and a temporary git repository.
pub fn tick_once(deps: &Deps) -> Result<NodeTickReport, CliError> {
    Ok(tick_once_report(deps, None)?.1)
}

/// Runs startup plus one node tick, returning both reports.
///
/// `migration` is the report `vowp up` received from the automatic v0.1 migration (if
/// any); it is recorded on the supervisor so `NodeReconcileReport.migration` carries it.
fn tick_once_report(
    deps: &Deps,
    migration: Option<MigrationReport>,
) -> Result<(NodeReconcileReport, NodeTickReport), CliError> {
    let mut supervisor = NodeSupervisor::new(deps.clone())?;
    if let Some(report) = migration {
        supervisor.record_migration(report);
    }
    let startup = supervisor.startup()?;
    Ok((startup, supervisor.tick()?))
}

/// Creates the fixed tmux layout for `project`.
///
/// `program` is the binary and its global flags (`vowp`, or an absolute path in the
/// detached supervisor). Tests pass a harmless placeholder command on a private tmux
/// socket.
pub fn ensure_project_windows(
    windows: &dyn WindowManager,
    project: &str,
    cwd: Option<&Path>,
    program: &[String],
) -> Result<(), CliError> {
    let window = |name: &str| {
        let mut argv = program.to_vec();
        argv.push("__window".to_owned());
        argv.push(name.to_owned());
        argv
    };
    let spec = LayoutSpec {
        board: window("board"),
        supervisor: window("supervisor"),
        planner: window("planner"),
        keeper: window("keeper"),
        inbox: window("inbox"),
        cwd: cwd.map(Path::to_path_buf),
    };
    windows
        .ensure_layout(project, &spec)
        .map_err(|error| CliError::Core(vowp_core::CoreError::Tmux(error)))
}

/// Creates the node-level `vowp-home` session from spec §4.4.
///
/// Windows: `board` (the node board) and `capacity` (the `__window capacity` renderer).
pub fn ensure_home_windows(
    windows: &dyn WindowManager,
    program: &[String],
    cwd: Option<&Path>,
) -> Result<(), CliError> {
    let session = "vowp-home";
    windows.ensure_session(session).map_err(tmux_error)?;
    for name in ["board", "capacity"] {
        if windows.window_exists(session, name).map_err(tmux_error)? {
            continue;
        }
        let mut argv = program.to_vec();
        argv.push("__window".to_owned());
        argv.push(name.to_owned());
        let spec = WindowSpec {
            argv,
            cwd: cwd.map(Path::to_path_buf),
            env: Vec::new(),
        };
        match windows.new_window(session, name, &spec) {
            Ok(()) | Err(TmuxError::WindowExists { .. }) => {}
            Err(error) => return Err(tmux_error(error)),
        }
    }
    // `ensure_session` starts with a tagged scratch window; the fixed layout drops it
    // exactly like `ensure_layout` does for a project session.
    if windows.window_exists(session, "scratch").map_err(tmux_error)? {
        let _ = windows.kill_window(session, "scratch");
    }
    Ok(())
}

/// Maps a tmux failure onto the CLI's typed core error.
fn tmux_error(error: TmuxError) -> CliError {
    CliError::Core(vowp_core::CoreError::Tmux(error))
}

/// Creates `vowp-home` and `vowp-<project>` for every project that is not archived.
fn ensure_node_windows(paths: &NodePaths, windows: &dyn WindowManager) -> Result<(), CliError> {
    let store = Store::open_global(paths)?;
    let exe = std::env::current_exe()?;
    let home_program = vec![
        exe.to_string_lossy().into_owned(),
        "--home".to_owned(),
        paths.root().display().to_string(),
    ];
    ensure_home_windows(windows, &home_program, Some(paths.root()))?;
    for entry in vowp_core::loops::registry::project_entries(&store)? {
        if entry.state == ProjectState::Archived || entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        let program = vec![
            exe.to_string_lossy().into_owned(),
            "--home".to_owned(),
            paths.root().display().to_string(),
            "--project".to_owned(),
            entry.path.clone(),
        ];
        match ensure_project_windows(windows, entry.name.as_str(), Some(Path::new(&entry.path)), &program) {
            Ok(()) => {}
            Err(CliError::Core(vowp_core::CoreError::Tmux(vowp_tmux::TmuxError::NotInstalled))) => {
                return Err(CliError::Core(vowp_core::CoreError::Tmux(
                    vowp_tmux::TmuxError::NotInstalled,
                )));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// What `up --json` prints.
#[derive(Debug, Serialize)]
struct UpOutput {
    running: bool,
    pid: Option<u32>,
    mode: &'static str,
    /// The optional web-window address, written only when a daemon reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    web_window: Option<String>,
}

/// What `up --once --json` prints after the tick.
#[derive(Debug, Serialize)]
struct TickOutput {
    at: String,
    ok: bool,
    steps: Vec<StepOutput>,
    dispatched: Vec<String>,
    transitions: Vec<TransitionOutput>,
    merged: Vec<String>,
    /// Automatic v0.1 migration performed before this tick, when there was one.
    migration: Option<MigrationReport>,
}

/// One tick step in JSON output.
#[derive(Debug, Serialize)]
struct StepOutput {
    name: String,
    ok: bool,
    detail: String,
}

/// One state transition in JSON output.
#[derive(Debug, Serialize)]
struct TransitionOutput {
    job: String,
    from: String,
    to: String,
}

/// Runs `vowp up` over an explicit home.
fn run_up(
    global: &Global,
    paths: &NodePaths,
    deps: &Deps,
    io: &mut dyn IoStreams,
    args: &UpArgs,
) -> Result<i32, CliError> {
    let json = global.json;
    if let Some(pid) = live_supervisor_pid(paths) {
        return Err(CliError::Conflict(format!(
            "supervisor already running (pid {pid}); `vowp status` shows it"
        )));
    }
    signals::clear_stop(paths)?;
    signals::clear_stopped(paths)?;
    // A crashed supervisor may leave an address behind; never surface a stale window.
    let _ = std::fs::remove_file(web_window_file(paths));

    let mut migration = None;
    if let Some(report) = migrate_if_needed(paths, !args.no_migrate)? {
        if !json {
            writeln!(
                io.out(),
                "migrated v0.1 home to the split layout ({} project(s)); backup: {}",
                report.projects.len(),
                report.backup
            )?;
        }
        migration = Some(report);
    }
    crate::node_ctx::ensure_home_project_file(paths)?;
    let checkout = crate::ctx::selected_checkout_root(global, paths)?;
    register_checkout_for_up(paths, deps, checkout.as_deref())?;
    if !args.no_tmux {
        match ensure_node_windows(paths, deps.windows.as_ref()) {
            Ok(()) => {}
            Err(CliError::Core(vowp_core::CoreError::Tmux(vowp_tmux::TmuxError::NotInstalled))) => {
                writeln!(io.err(), "warning: tmux is not installed; continuing without windows")?;
            }
            Err(error) => return Err(error),
        }
    }

    if args.once {
        return run_once(paths, deps, json, io, migration);
    }
    if args.foreground {
        return run_foreground(paths, deps, json, io, migration, checkout.as_deref());
    }
    run_detached(paths, checkout.as_deref(), json, io)
}

/// Migrates a v0.1 home before the supervisor opens it.
///
/// The store's `open_global` refuses a version-1 file with [`StoreError::MigrationRequired`];
/// that is the only signal needed. `allow=false` (the `--no-migrate` path) turns the refusal
/// into a clear conflict, so a unit started with `--no-migrate` on a v0.1 home fails with
/// `vowp migrate` in the message instead of a raw schema error.
fn migrate_if_needed(paths: &NodePaths, allow: bool) -> Result<Option<vowp_store::MigrationReport>, CliError> {
    match Store::open_global(paths) {
        Ok(_) => Ok(None),
        Err(StoreError::MigrationRequired) if allow => Ok(Some(vowp_store::migrate_v01(paths)?)),
        Err(StoreError::MigrationRequired) => Err(CliError::Conflict(
            "the home is still the v0.1 single-file schema; run `vowp migrate` or drop --no-migrate".to_owned(),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Runs `startup()` plus one node tick, prints the report and releases the lock.
fn run_once(
    paths: &NodePaths,
    deps: &Deps,
    json: bool,
    io: &mut dyn IoStreams,
    migration: Option<MigrationReport>,
) -> Result<i32, CliError> {
    let pid = std::process::id();
    write_lock(paths, pid, deps.clock.now())?;
    let result = tick_once_report(deps, migration);
    remove_lock_if_own(paths, pid);
    let (startup, report) = result?;
    print_tick_report(io, &report, startup.migration.as_ref(), json)
}

/// Prints one node tick and returns the exit code for `up --once`.
///
/// A failed node step or a failed project step makes the tick non-zero: `--once` is a
/// diagnostic run (tests and cron), so a degraded project must not look like success on
/// the human path while `--json` already reports `ok: false`.
fn print_tick_report(
    io: &mut dyn IoStreams,
    report: &NodeTickReport,
    migration: Option<&MigrationReport>,
    json: bool,
) -> Result<i32, CliError> {
    let project_steps = || {
        report
            .projects
            .iter()
            .flat_map(|(project, tick)| tick.steps.iter().map(move |step| (project, step)))
    };
    let ok = report.steps.iter().all(|step| step.ok) && project_steps().all(|(_, step)| step.ok);
    if json {
        let output = TickOutput {
            at: report.at.as_rfc3339(),
            ok,
            steps: report
                .steps
                .iter()
                .map(|step| StepOutput {
                    name: step.name.to_owned(),
                    ok: step.ok,
                    detail: step.detail.clone(),
                })
                .collect(),
            dispatched: report
                .projects
                .values()
                .flat_map(|project| project.dispatched.iter().map(ToString::to_string))
                .collect(),
            transitions: report
                .projects
                .values()
                .flat_map(|project| {
                    project.transitions.iter().map(|(job, from, to)| TransitionOutput {
                        job: job.to_string(),
                        from: from.as_str().to_owned(),
                        to: to.as_str().to_owned(),
                    })
                })
                .collect(),
            merged: report
                .projects
                .values()
                .flat_map(|project| project.merged.iter().map(ToString::to_string))
                .collect(),
            migration: migration.cloned(),
        };
        crate::output::print_json(io, &output)?;
    } else {
        let node_steps = report.steps.len();
        let project_step_count = project_steps().count();
        let completed =
            report.steps.iter().filter(|step| step.ok).count() + project_steps().filter(|(_, step)| step.ok).count();
        writeln!(
            io.out(),
            "tick {}: {}/{} steps ok, {} project(s), {} dispatched",
            report.at.as_rfc3339(),
            completed,
            node_steps + project_step_count,
            report.projects.len(),
            report
                .projects
                .values()
                .map(|project| project.dispatched.len())
                .sum::<usize>()
        )?;
        for step in report.steps.iter().filter(|step| !step.ok) {
            writeln!(io.out(), "  degraded {}: {}", step.name, step.detail)?;
        }
        for (project, step) in project_steps().filter(|(_, step)| !step.ok) {
            writeln!(io.out(), "  degraded {project}/{}: {}", step.name, step.detail)?;
        }
        for (project, detail) in &report.skipped {
            writeln!(io.out(), "  skipped {project}: {detail}")?;
        }
    }
    Ok(if ok { 0 } else { 1 })
}

/// Runs the supervisor loop in this process until the stop file appears.
fn run_foreground(
    paths: &NodePaths,
    deps: &Deps,
    json: bool,
    io: &mut dyn IoStreams,
    migration: Option<MigrationReport>,
    checkout: Option<&Path>,
) -> Result<i32, CliError> {
    run_foreground_with_paths(
        paths,
        deps,
        json,
        io,
        migration,
        checkout,
        signals::SignalWatcher::install,
    )
}

/// Runs the foreground supervisor with an injectable signal-watcher installer.
///
/// Production calls [`run_foreground`], which passes [`signals::SignalWatcher::install`].
/// Tests use the seam to make installation fail and prove that the lock written before it
/// is removed again.
pub fn run_foreground_with<F>(ctx: &Ctx, io: &mut dyn IoStreams, install: F) -> Result<i32, CliError>
where
    F: FnOnce(&NodePaths) -> std::io::Result<signals::SignalWatcher>,
{
    run_foreground_with_paths(
        ctx.paths(),
        &supervisor_deps(ctx.deps().clone(), ctx.paths()),
        ctx.json(),
        io,
        None,
        Some(ctx.project_root()),
        install,
    )
}

/// The foreground loop over an explicit home, dependencies and JSON flag.
fn run_foreground_with_paths<F>(
    paths: &NodePaths,
    deps: &Deps,
    json: bool,
    io: &mut dyn IoStreams,
    migration: Option<MigrationReport>,
    checkout: Option<&Path>,
    install: F,
) -> Result<i32, CliError>
where
    F: FnOnce(&NodePaths) -> std::io::Result<signals::SignalWatcher>,
{
    let pid = std::process::id();
    write_lock(paths, pid, deps.clock.now())?;

    let watcher = match install(paths) {
        Ok(watcher) => watcher,
        Err(error) => {
            remove_lock_if_own(paths, pid);
            return Err(CliError::Io(error));
        }
    };
    let mut supervisor = match NodeSupervisor::new(deps.clone()) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            remove_lock_if_own(paths, pid);
            return Err(error.into());
        }
    };
    if let Some(report) = migration {
        supervisor.record_migration(report);
    }
    if !json {
        writeln!(io.out(), "supervisor running (pid {pid}); stop it with `vowp down`")?;
    }
    // The web window is optional: one project, one daemon child. A missing or ambiguous
    // project, a missing binary and a daemon that never prints an address all warn and let
    // the supervisor run. The child keeps its own read-only store; the lock stays ours.
    let mut web_window = match select_daemon_project(paths, checkout) {
        Ok(Some(project)) => start_web_window(paths, &project, io),
        Ok(None) => {
            let _ = writeln!(
                io.err(),
                "warning: web window not started: register one project or run `vowp up --project <name>` from its checkout"
            );
            None
        }
        Err(error) => {
            let _ = writeln!(io.err(), "warning: web window not started: {error}");
            None
        }
    };
    if !json {
        if let Some(address) = web_window.as_ref().and_then(WebWindow::address) {
            print_web_window(io, address)?;
        }
    }
    // The supervisor waits up to `tick_secs` between ticks; a cancellation alone
    // would keep `vowp down` waiting for that whole interval. Wake it as soon as the
    // stop watcher cancels so shutdown is prompt.
    let cancel = watcher.cancel_token();
    let wake = supervisor.wake_handle();
    let wake_cancel = cancel.clone();
    let wake_thread = std::thread::spawn(move || {
        while !wake_cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(200));
        }
        wake.wake();
    });
    let result = supervisor.run(&cancel);
    // A failed startup returns before the stop watcher cancels; release the wake thread.
    watcher.cancel();
    let _ = wake_thread.join();
    // Stop the daemon before `wait_for_children`: it is our direct child, and it must not
    // keep the adapter/gate grace period open or outlive the supervisor.
    if let Some(window) = web_window.as_mut() {
        window.stop();
    }
    // Runner cancellation is cooperative: wait for adapter/gate children, escalating once
    // the grace expires, so a clean signal shutdown does not orphan harness processes.
    wait_for_children();
    // Remove the live heartbeat and record the deliberate stop separately, so neither
    // `status --check` nor the watchdog mistakes a fresh timestamp for a live supervisor.
    let _ = signals::mark_stopped(paths, deps.clock.now());
    remove_lock_if_own(paths, pid);
    result?;
    if !json {
        writeln!(io.out(), "supervisor stopped (pid {pid})")?;
    }
    Ok(0)
}

/// Returns the live supervisor pid from `<home>/supervisor.lock`, if one holds the lock.
///
/// The lock may be stale or contain a value that does not fit a signed `pid_t`; both count
/// as not running and are removed by the next `up` or `down`.
fn live_supervisor_pid(paths: &NodePaths) -> Option<u32> {
    let text = std::fs::read_to_string(paths.root().join("supervisor.lock")).ok()?;
    let pid: u32 = text.lines().next()?.trim().parse().ok()?;
    process_alive(pid).then_some(pid)
}

/// Waits for direct children (adapter and gate processes) to exit after cancellation.
///
/// The supervisor's runner threads are cooperative and may still be interrupting a child
/// when `run` returns. Wait briefly, then signal the remaining process groups so a clean
/// shutdown never leaves orphaned harnesses behind.
fn wait_for_children() {
    let parent = std::process::id();
    let deadline = Instant::now() + CHILD_EXIT_WAIT;
    while Instant::now() < deadline {
        if child_pids(parent).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    for pid in child_pids(parent) {
        let _ = signal_child(pid, "TERM");
    }
    let deadline = Instant::now() + CHILD_KILL_WAIT;
    while Instant::now() < deadline {
        if child_pids(parent).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for pid in child_pids(parent) {
        let _ = signal_child(pid, "KILL");
    }
}

/// Returns the pids whose parent is `parent`.
fn child_pids(parent: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        child_pids_from_proc(parent)
    }
    #[cfg(not(target_os = "linux"))]
    {
        child_pids_from_ps(parent)
    }
}

/// Returns the direct children of `parent` from `/proc` (Linux).
#[cfg(target_os = "linux")]
fn child_pids_from_proc(parent: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name).map(str::trim));
        // A zombie child has already exited and only waits to be reaped; it must not hold
        // the shutdown in the full grace period.
        if field("State:").is_some_and(|state| state.starts_with('Z')) {
            continue;
        }
        let ppid = field("PPid:").and_then(|value| value.parse::<u32>().ok());
        if ppid == Some(parent) && valid_pid(pid) {
            pids.push(pid);
        }
    }
    pids
}

/// Returns the direct children of `parent` from `ps` (macOS and other Unixes).
#[cfg(any(not(target_os = "linux"), test))]
fn child_pids_from_ps(parent: u32) -> Vec<u32> {
    let Ok(child) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    // `ps` lists itself: it is a short-lived child of this process and must not be
    // mistaken for an adapter or gate that shutdown still has to wait for.
    let ps_pid = child.id();
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_ps_children(&String::from_utf8_lossy(&output.stdout), parent)
        .into_iter()
        .filter(|pid| *pid != ps_pid)
        .collect()
}

/// Parses `ps -A -o pid=,ppid=,stat=` output into direct children of `parent`.
///
/// Lines must have exactly three whitespace-separated fields. Zombies (`stat` starts with
/// `Z`) and ids that cannot be signalled safely are skipped.
#[cfg(any(not(target_os = "linux"), test))]
fn parse_ps_children(output: &str, parent: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(state)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if fields.next().is_some() {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if state.starts_with('Z') || !valid_pid(pid) {
            continue;
        }
        if ppid == parent {
            pids.push(pid);
        }
    }
    pids
}

/// Sends `signal` to a child's process group, falling back to the pid itself.
fn signal_child(pid: u32, signal: &str) -> std::io::Result<()> {
    if !valid_pid(pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to signal invalid pid {pid}"),
        ));
    }
    let group = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if group.success() {
        return Ok(());
    }
    let direct = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if direct.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("kill -{signal} {pid} failed")))
    }
}

/// Re-executes this binary with `--foreground` and returns as soon as it owns the lock.
fn run_detached(
    paths: &NodePaths,
    project: Option<&Path>,
    json: bool,
    io: &mut dyn IoStreams,
) -> Result<i32, CliError> {
    std::fs::create_dir_all(paths.logs_dir())?;
    let log_path = paths.logs_dir().join("supervisor.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;

    let exe = std::env::current_exe()?;
    let configure = |command: &mut Command| {
        command.arg("--home").arg(paths.root());
        if let Some(project) = project {
            command.arg("--project").arg(project);
        }
        command.arg("up").arg("--foreground").arg("--no-tmux");
    };

    // `setsid` detaches the supervisor from the caller's terminal; when it is not
    // installed the child still gets its own process group and inherited stdio is gone.
    let mut detached = Command::new("setsid");
    detached.arg(&exe);
    configure(&mut detached);
    detached
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let mut child = match detached.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut direct = Command::new(&exe);
            configure(&mut direct);
            direct
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .process_group(0)
                .spawn()?
        }
        Err(error) => return Err(CliError::Io(error)),
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut started_pid = None;
    let mut exited = None;
    while Instant::now() < deadline {
        if let Some(pid) = live_supervisor_pid(paths) {
            started_pid = Some(pid);
            break;
        }
        // A child that exited immediately (bad config, startup panic) must not be
        // reported as a running supervisor; notice it without waiting for the deadline.
        match child.try_wait() {
            Ok(Some(status)) => {
                exited = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => return Err(CliError::Io(error)),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = match (started_pid, exited) {
        (Some(pid), _) => pid,
        (None, Some(status)) => {
            return Err(CliError::Conflict(format!(
                "supervisor exited during startup ({status}); see {}",
                log_path.display()
            )));
        }
        (None, None) => {
            // Re-check once at the deadline so a child that exited just as the wait ended
            // is still reported as failed startup, not as running.
            if let Ok(Some(status)) = child.try_wait() {
                return Err(CliError::Conflict(format!(
                    "supervisor exited during startup ({status}); see {}",
                    log_path.display()
                )));
            }
            child.id()
        }
    };

    // The foreground child starts the daemon after it owns the lock; wait briefly for the
    // address it publishes so the detached caller prints the same window line.
    let web_window = wait_for_web_window(paths, WEB_WINDOW_WAIT);
    if json {
        crate::output::print_json(
            io,
            &UpOutput {
                running: true,
                pid: Some(pid),
                mode: "detached",
                web_window: web_window.clone(),
            },
        )?;
    } else {
        writeln!(io.out(), "supervisor started (pid {pid})")?;
        if let Some(address) = &web_window {
            print_web_window(io, address)?;
        }
    }
    if web_window.is_none() {
        let _ = writeln!(
            io.err(),
            "warning: the web window did not start; the supervisor keeps running"
        );
    }
    Ok(0)
}

/// Runs `vowp down`.
fn run_down(
    paths: &NodePaths,
    deps: &Deps,
    json: bool,
    io: &mut dyn IoStreams,
    args: &DownArgs,
) -> Result<i32, CliError> {
    let stop = signals::stop_file(paths);
    std::fs::write(&stop, deps.clock.now().unix_secs().to_string())?;

    let Some(pid) = live_supervisor_pid(paths) else {
        remove_lock(paths);
        // A crashed supervisor may leave only its window address behind.
        let _ = std::fs::remove_file(web_window_file(paths));
        if args.kill_windows {
            kill_windows(paths, deps)?;
        }
        if json {
            crate::output::print_json(
                io,
                &UpOutput {
                    running: false,
                    pid: None,
                    mode: "down",
                    web_window: None,
                },
            )?;
        } else {
            writeln!(io.out(), "supervisor is not running")?;
        }
        return Ok(0);
    };

    let deadline = Instant::now() + DOWN_WAIT;
    while Instant::now() < deadline && live_supervisor_pid(paths) == Some(pid) {
        std::thread::sleep(Duration::from_millis(100));
    }

    if live_supervisor_pid(paths) == Some(pid) {
        if args.force {
            send_term(pid)?;
            let force_deadline = Instant::now() + FORCE_WAIT;
            while Instant::now() < force_deadline && live_supervisor_pid(paths) == Some(pid) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if live_supervisor_pid(paths) == Some(pid) {
            return Err(CliError::Conflict(format!(
                "supervisor (pid {pid}) did not stop; use `vowp down --force`"
            )));
        }
    }

    remove_lock(paths);
    if args.kill_windows {
        kill_windows(paths, deps)?;
    }
    if json {
        crate::output::print_json(
            io,
            &UpOutput {
                running: false,
                pid: Some(pid),
                mode: "down",
                web_window: None,
            },
        )?;
    } else {
        writeln!(io.out(), "supervisor stopped (pid {pid})")?;
    }
    Ok(0)
}

/// Sends SIGTERM through `/bin/kill` (no `unsafe`, no libc dependency).
fn send_term(pid: u32) -> Result<(), CliError> {
    if !valid_pid(pid) {
        return Err(CliError::Invalid(format!("refusing to signal invalid pid {pid}")));
    }
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map_err(CliError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::Conflict(format!("cannot signal supervisor pid {pid}")))
    }
}

/// Kills every tagged window in the node session and every project session.
fn kill_windows(paths: &NodePaths, deps: &Deps) -> Result<(), CliError> {
    match deps.windows.list_windows("vowp-home") {
        Ok(windows) => {
            for window in windows {
                if window.tagged {
                    let _ = deps.windows.kill_window("vowp-home", &window.name);
                }
            }
        }
        Err(TmuxError::NotInstalled) => {}
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    }
    let store = Store::open_global(paths)?;
    for entry in vowp_core::loops::registry::project_entries(&store)? {
        if entry.state == ProjectState::Archived || entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
            continue;
        }
        kill_project_session(deps, &entry.name)?;
    }
    Ok(())
}

/// Kills the tagged windows and the session of one project.
fn kill_project_session(deps: &Deps, project: &ProjectName) -> Result<(), CliError> {
    let session = session_name_for_project(project.as_str());
    match deps.windows.kill_session(&session) {
        Ok(()) | Err(TmuxError::NotInstalled) => return Ok(()),
        Err(TmuxError::NotTagged { .. } | TmuxError::Refused(_)) => {}
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    }
    let windows = match deps.windows.list_windows(&session) {
        Ok(windows) => windows,
        Err(TmuxError::NotInstalled) => return Ok(()),
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    };
    for window in windows {
        if window.tagged {
            let _ = deps.windows.kill_window(&session, &window.name);
        }
    }
    Ok(())
}

/// Writes the supervisor lock: pid on line 1, RFC 3339 start time on line 2.
fn write_lock(paths: &NodePaths, pid: u32, now: Timestamp) -> Result<(), CliError> {
    let text = format!("{pid}\n{}\n", now.as_rfc3339());
    let path = paths.root().join("supervisor.lock");
    let temp = paths.tmp_dir().join("supervisor.lock.tmp");
    std::fs::create_dir_all(paths.tmp_dir())?;
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Removes the lock when it still names `pid`.
fn remove_lock_if_own(paths: &NodePaths, pid: u32) {
    let path = paths.root().join("supervisor.lock");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if text.lines().next().and_then(|line| line.trim().parse::<u32>().ok()) == Some(pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// Removes the lock file unconditionally (stale lock after a stopped supervisor).
fn remove_lock(paths: &NodePaths) {
    let _ = std::fs::remove_file(paths.root().join("supervisor.lock"));
}

/// Clones the context dependencies and replaces the log with the node's supervisor log.
fn supervisor_deps(mut deps: Deps, paths: &NodePaths) -> Deps {
    deps.log = Arc::new(FileLog::new(
        paths.logs_dir().join("supervisor.log"),
        Arc::clone(&deps.clock),
    ));
    deps
}

/// Appends supervisor lines to `<home>/logs/supervisor.log`.
///
/// Lines are `<rfc3339> <LEVEL> <target> <message>`, matching `conventions.md` §4.
struct FileLog {
    path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl FileLog {
    /// Creates a sink for `path`; the directory is created by [`vowp_store::NodePaths`].
    fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
    }
}

impl Log for FileLog {
    fn log(&self, level: Level, target: &str, msg: &str) {
        let level = match level {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        };
        let line = format!(
            "{} {level} {target} {}\n",
            self.clock.now().as_rfc3339(),
            msg.replace('\n', " ")
        );
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) else {
            return;
        };
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_tick_report_surfaces_a_degraded_project_and_fails_the_tick() {
        use vowp_core::loops::{StepReport, TickReport};

        let at = Timestamp::parse("2026-11-10T10:00:00Z").expect("timestamp");
        let mut projects = std::collections::BTreeMap::new();
        projects.insert(
            ProjectName::parse("demo").expect("project"),
            TickReport {
                at,
                steps: vec![StepReport {
                    name: "harvest",
                    ok: false,
                    detail: "disk full".to_owned(),
                }],
                dispatched: Vec::new(),
                transitions: Vec::new(),
                merged: Vec::new(),
            },
        );
        let report = NodeTickReport {
            at,
            steps: vec![StepReport {
                name: "sample",
                ok: true,
                detail: "host sampled".to_owned(),
            }],
            projects,
            skipped: Vec::new(),
        };

        let mut io = crate::BufferIo::new("");
        let code = print_tick_report(&mut io, &report, None, false).expect("print");
        assert_eq!(code, 1, "a failed project step must fail `up --once`");
        let stdout = io.stdout();
        assert!(stdout.contains("1/2 steps ok"), "{stdout}");
        assert!(stdout.contains("degraded demo/harvest: disk full"), "{stdout}");
    }

    #[test]
    fn window_ports_come_only_from_the_loopback_url() {
        assert_eq!(web_window_port("http://127.0.0.1:54321/"), Some(54321));
        assert_eq!(web_window_port("http://127.0.0.1:1"), Some(1));
        assert_eq!(web_window_port("http://127.0.0.1:0/"), None);
        assert_eq!(web_window_port("http://127.0.0.1:/"), None);
        assert_eq!(web_window_port("http://192.0.2.1:54321/"), None);
        assert_eq!(web_window_port("https://127.0.0.1:54321/"), None);
        assert_eq!(web_window_port("http://127.0.0.1:99999/"), None);
    }

    #[test]
    fn parse_ps_children_skips_zombies_malformed_lines_and_invalid_pids() {
        let output = "\
              1     0 Ss
            100     1 S
            101   100 S+
            102   100 Z+
            103   100 S
            104   100 S extra
            105   100
              0   100 S
     4294967295   100 S
     2147483648   100 S
            106   100 R
";
        assert_eq!(parse_ps_children(output, 100), vec![101, 103, 106]);
    }

    #[test]
    fn invalid_pids_are_never_signalled() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            let error = send_term(pid).expect_err("send_term must refuse");
            assert!(matches!(error, CliError::Invalid(_)), "pid {pid}: {error}");

            let error = signal_child(pid, "TERM").expect_err("signal_child must refuse");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "pid {pid}");
        }
    }

    #[test]
    fn live_supervisor_pid_rejects_invalid_lock_values_and_finds_this_process() {
        let home = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(home.path());
        let lock = paths.root().join("supervisor.lock");

        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            std::fs::write(&lock, format!("{pid}\n")).expect("lock");
            assert_eq!(live_supervisor_pid(&paths), None, "pid {pid}");
        }

        std::fs::write(&lock, format!("{}\n", std::process::id())).expect("lock");
        assert_eq!(live_supervisor_pid(&paths), Some(std::process::id()));
    }

    #[test]
    fn child_pids_finds_a_direct_child() {
        let mut child = Command::new("sleep").arg("5").spawn().expect("spawn sleep");
        let parent = std::process::id();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let platform = child_pids(parent).contains(&child.id());
            let ps = child_pids_from_ps(parent).contains(&child.id());
            if platform && ps {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("direct child {} was not listed", child.id());
    }
}
