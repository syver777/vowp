//! `vowp up` and `vowp down`: tmux layout, supervisor lifecycle and the stop file.
//!
//! Spec: `docs/design/cli.md` §2, `docs/design/loops.md` §1 steps 12–13 and
//! `docs/design/crates/vowp-cli.md` §1. `up --once` runs startup plus exactly one
//! supervisor tick (tests and cron); `--foreground` runs the loop in this process and
//! `--detach` (the default) re-executes the binary with `--foreground`.
//!
//! `down` writes `<home>/stop` and waits for the lock to disappear. The foreground
//! supervisor watches that file and SIGTERM/SIGINT, interrupts its runners, waits for
//! adapter/gate children and marks the heartbeat stopped before it exits.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use vowp_adapters::proc::{process_alive, valid_pid};
use vowp_core::Deps;
use vowp_core::loops::{Supervisor, TickReport};
use vowp_protocol::{Clock, Level, Log, ProjectName, Timestamp};
use vowp_store::NodePaths;
use vowp_tmux::{LayoutSpec, WindowManager, session_name_for_project};

use crate::cli::{DownArgs, UpArgs, UpCommand};
use crate::signals;
use crate::{CliError, Ctx, IoStreams};

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

/// Runs `vowp up` or `vowp down`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &UpCommand) -> Result<i32, CliError> {
    match command {
        UpCommand::Up(args) => run_up(ctx, io, args),
        UpCommand::Down(args) => run_down(ctx, io, args),
    }
}

/// Runs one supervisor tick with the supplied dependencies.
///
/// This is the seam `up --once` uses and the one the hermetic test drives with fake
/// adapters, a temporary node home and a temporary git repository.
pub fn tick_once(deps: &Deps, project: &ProjectName) -> Result<TickReport, CliError> {
    let mut supervisor = Supervisor::new(deps.clone(), project.clone())?;
    supervisor.startup()?;
    Ok(supervisor.tick()?)
}

/// Creates the fixed tmux layout for `project`.
///
/// `program` is the binary each window runs (`vowp`, or an absolute path in the detached
/// supervisor). Tests pass a harmless placeholder command on a private tmux socket.
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

/// What `up --json` prints.
#[derive(Debug, Serialize)]
struct UpOutput {
    running: bool,
    pid: Option<u32>,
    mode: &'static str,
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

/// Runs `vowp up`.
fn run_up(ctx: &Ctx, io: &mut dyn IoStreams, args: &UpArgs) -> Result<i32, CliError> {
    let paths = ctx.paths();
    if let Some(pid) = live_supervisor_pid(paths) {
        return Err(CliError::Conflict(format!(
            "supervisor already running (pid {pid}); `vowp status` shows it"
        )));
    }
    signals::clear_stop(paths)?;
    signals::clear_stopped(paths)?;

    if !args.no_tmux {
        let exe = std::env::current_exe()?;
        let program = vec![exe.to_string_lossy().into_owned()];
        match ensure_project_windows(
            ctx.deps().windows.as_ref(),
            ctx.project().as_str(),
            Some(ctx.project_root()),
            &program,
        ) {
            Ok(()) => {}
            Err(CliError::Core(vowp_core::CoreError::Tmux(vowp_tmux::TmuxError::NotInstalled))) => {
                writeln!(io.err(), "warning: tmux is not installed; continuing without windows")?;
            }
            Err(error) => return Err(error),
        }
    }

    if args.once {
        return run_once(ctx, io);
    }
    if args.foreground {
        return run_foreground(ctx, io);
    }
    run_detached(ctx, io)
}

/// Runs `startup()` plus one tick, prints the report and releases the lock.
fn run_once(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let paths = ctx.paths();
    let pid = std::process::id();
    write_lock(paths, pid, ctx.clock().now())?;
    let result = tick_once(&supervisor_deps(ctx), ctx.project());
    remove_lock_if_own(paths, pid);
    let report = result?;

    let ok = report.steps.iter().all(|step| step.ok);
    if ctx.json() {
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
            dispatched: report.dispatched.iter().map(ToString::to_string).collect(),
            transitions: report
                .transitions
                .iter()
                .map(|(job, from, to)| TransitionOutput {
                    job: job.to_string(),
                    from: from.as_str().to_owned(),
                    to: to.as_str().to_owned(),
                })
                .collect(),
            merged: report.merged.iter().map(ToString::to_string).collect(),
        };
        crate::output::print_json(io, &output)?;
    } else {
        let completed = report.steps.iter().filter(|step| step.ok).count();
        writeln!(
            io.out(),
            "tick {}: {}/{} steps ok, {} dispatched",
            report.at.as_rfc3339(),
            completed,
            report.steps.len(),
            report.dispatched.len()
        )?;
        for step in report.steps.iter().filter(|step| !step.ok) {
            writeln!(io.out(), "  degraded {}: {}", step.name, step.detail)?;
        }
    }
    Ok(0)
}

/// Runs the supervisor loop in this process until the stop file appears.
fn run_foreground(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    run_foreground_with(ctx, io, signals::SignalWatcher::install)
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
    let paths = ctx.paths();
    let pid = std::process::id();
    write_lock(paths, pid, ctx.clock().now())?;

    let watcher = match install(paths) {
        Ok(watcher) => watcher,
        Err(error) => {
            remove_lock_if_own(paths, pid);
            return Err(CliError::Io(error));
        }
    };
    let mut supervisor = match Supervisor::new(supervisor_deps(ctx), ctx.project().clone()) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            remove_lock_if_own(paths, pid);
            return Err(error.into());
        }
    };
    if !ctx.json() {
        writeln!(io.out(), "supervisor running (pid {pid}); stop it with `vowp down`")?;
    }
    // `Supervisor::run` waits up to `tick_secs` between ticks; a cancellation alone
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
    // Runner cancellation is cooperative: wait for adapter/gate children, escalating once
    // the grace expires, so a clean signal shutdown does not orphan harness processes.
    wait_for_children();
    // Remove the live heartbeat and record the deliberate stop separately, so neither
    // `status --check` nor the watchdog mistakes a fresh timestamp for a live supervisor.
    let _ = signals::mark_stopped(paths, ctx.clock().now());
    remove_lock_if_own(paths, pid);
    result?;
    if !ctx.json() {
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
fn run_detached(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let paths = ctx.paths();
    std::fs::create_dir_all(paths.logs_dir())?;
    let log_path = paths.logs_dir().join("supervisor.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;

    let exe = std::env::current_exe()?;
    let configure = |command: &mut Command| {
        command
            .arg("--home")
            .arg(paths.root())
            .arg("--project")
            .arg(ctx.project_root())
            .arg("up")
            .arg("--foreground")
            .arg("--no-tmux");
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

    if ctx.json() {
        crate::output::print_json(
            io,
            &UpOutput {
                running: true,
                pid: Some(pid),
                mode: "detached",
            },
        )?;
    } else {
        writeln!(io.out(), "supervisor started (pid {pid})")?;
    }
    Ok(0)
}

/// Runs `vowp down`.
fn run_down(ctx: &Ctx, io: &mut dyn IoStreams, args: &DownArgs) -> Result<i32, CliError> {
    let paths = ctx.paths();
    let stop = signals::stop_file(paths);
    std::fs::write(&stop, ctx.clock().now().unix_secs().to_string())?;

    let Some(pid) = live_supervisor_pid(paths) else {
        remove_lock(paths);
        if args.kill_windows {
            kill_windows(ctx)?;
        }
        if ctx.json() {
            crate::output::print_json(
                io,
                &UpOutput {
                    running: false,
                    pid: None,
                    mode: "down",
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
        kill_windows(ctx)?;
    }
    if ctx.json() {
        crate::output::print_json(
            io,
            &UpOutput {
                running: false,
                pid: Some(pid),
                mode: "down",
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

/// Kills every tagged window in the project's session.
fn kill_windows(ctx: &Ctx) -> Result<(), CliError> {
    let session = session_name_for_project(ctx.project().as_str());
    let windows = match ctx.deps().windows.list_windows(&session) {
        Ok(windows) => windows,
        Err(vowp_tmux::TmuxError::NotInstalled) => return Ok(()),
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    };
    for window in windows {
        let _ = ctx.deps().windows.kill_window(&session, &window.name);
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
fn supervisor_deps(ctx: &Ctx) -> Deps {
    let mut deps = ctx.deps().clone();
    deps.log = Arc::new(FileLog::new(
        ctx.paths().logs_dir().join("supervisor.log"),
        Arc::clone(&ctx.deps().clock),
    ));
    deps
}

/// Appends supervisor lines to `<home>/logs/supervisor.log`.
///
/// Lines are `<rfc3339> <LEVEL> <target> <message>`, matching `conventions.md` §4.
struct FileLog {
    path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
}

impl FileLog {
    /// Creates a sink for `path`; the directory is created by [`vowp_store::NodePaths`].
    fn new(path: std::path::PathBuf, clock: Arc<dyn Clock>) -> Self {
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
