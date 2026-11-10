//! The real tmux implementation of [`WindowManager`].
//!
//! Every command runs without a shell, with an optional `-L <socket>` first,
//! and under a polling watchdog. The crate deliberately exposes no respawn or
//! keystroke capability; recycling means kill then create.
//!
//! Spec: `docs/design/crates/vowp-tmux.md` §3.

use std::cell::RefCell;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::TmuxError;
use crate::manager::{
    LayoutSpec, WindowInfo, WindowManager, WindowSpec, require_vowp_session, require_window_name,
    session_name_for_project,
};
use crate::quote::{validate_name, window_command};

const TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const WINDOW_FORMAT: &str = "#{window_id}|#{window_index}|#{window_name}|#{@vowp}|#{pane_pid}|#{window_active}";
/// File under the node home that records which tmux server `vowp up` created.
const SERVER_RECORD: &str = "tmux-server";
/// Environment override for scripts and tests that need a fixed private socket name.
const SOCKET_ENV: &str = "VOWP_TMUX_SOCKET";

#[derive(Debug, Clone)]
struct Scope {
    socket: String,
    tmpdir: PathBuf,
}

thread_local! {
    /// The private scope selected by [`Tmux::for_home`] for this process.
    ///
    /// `vowp attach` constructs its own [`Tmux`] for the argv it execs; the
    /// thread-local makes that construction use the same private server as the
    /// command's [`crate::Tmux`] without changing `attach`'s signature.
    static CURRENT_SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Real tmux.
///
/// A [`Tmux`] always addresses a named private socket. `Tmux::new(Some("name"))`
/// uses `tmux -L name ...`; `Tmux::new(None)` inherits the process scope set by
/// [`Tmux::for_home`]. Commands that address a server are refused when no
/// private socket is known, so a missing `-L` can never fall back to the user's
/// default server.
#[derive(Debug, Clone)]
pub struct Tmux {
    socket: Option<String>,
    binary: String,
    tmpdir: Option<PathBuf>,
    owner: Option<PathBuf>,
    explicit_socket: bool,
}

struct Completed {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct RawWindow {
    info: WindowInfo,
    id: String,
}

impl Tmux {
    /// Create a manager for a named private socket, or inherit the process
    /// scope set by [`Tmux::for_home`] when `socket` is `None`.
    ///
    /// A manager without a socket (or with the literal socket name `default`)
    /// refuses every server command, so it can never reach the user's default
    /// tmux server.
    pub fn new(socket: Option<String>) -> Tmux {
        match socket {
            Some(socket) => Tmux {
                socket: Some(socket),
                binary: "tmux".to_owned(),
                tmpdir: None,
                owner: None,
                explicit_socket: true,
            },
            None => {
                let scope = current_scope();
                Tmux {
                    socket: scope.as_ref().map(|scope| scope.socket.clone()),
                    binary: "tmux".to_owned(),
                    tmpdir: scope.as_ref().map(|scope| scope.tmpdir.clone()),
                    owner: None,
                    explicit_socket: false,
                }
            }
        }
    }

    /// Create a manager with an overridden tmux binary (tests, CI).
    pub fn with_binary(socket: Option<String>, binary: impl Into<String>) -> Tmux {
        let mut tmux = Tmux::new(socket);
        tmux.binary = binary.into();
        tmux
    }

    /// Create the private manager owned by one node home.
    ///
    /// The socket name is derived from the node home (or taken from
    /// `VOWP_TMUX_SOCKET` when a script needs a fixed name), and every command
    /// runs with `TMUX_TMPDIR=<home>/tmux` and `-L <socket>`. The first layout
    /// creation records the socket and server pid under `<home>/tmux-server`;
    /// destructive operations refuse a server whose pid or socket does not
    /// match that record.
    pub fn for_home(home: impl AsRef<Path>) -> Tmux {
        let socket = std::env::var(SOCKET_ENV).ok();
        Tmux::for_home_with_socket(home, socket)
    }

    /// Create the private manager owned by one node home with an optional
    /// socket-name override.
    ///
    /// Tests use this constructor with `None` so an ambient
    /// `VOWP_TMUX_SOCKET` cannot leak into their expectations. An invalid or
    /// `default` override is ignored and the home-derived name is used.
    pub fn for_home_with_socket(home: impl AsRef<Path>, socket: Option<String>) -> Tmux {
        let home = canonical_or_original(home.as_ref());
        let socket = socket
            .filter(|value| value != "default" && validate_name("socket", value).is_ok())
            .unwrap_or_else(|| socket_name_for_home(&home));
        let tmpdir = home.join("tmux");
        let scope = Scope {
            socket: socket.clone(),
            tmpdir: tmpdir.clone(),
        };
        CURRENT_SCOPE.with(|current| {
            *current.borrow_mut() = Some(scope);
        });
        Tmux {
            socket: Some(socket),
            binary: "tmux".to_owned(),
            tmpdir: Some(tmpdir),
            owner: Some(home),
            explicit_socket: false,
        }
    }

    /// Create a manager bound to an explicit private socket directory.
    ///
    /// This is the test helper constructor: both `-L <socket>` and
    /// `TMUX_TMPDIR=<dir>` are always set, and `default` is rejected.
    pub fn in_private_dir(socket: impl Into<String>, dir: impl Into<PathBuf>) -> Result<Tmux, TmuxError> {
        let socket = socket.into();
        validate_name("socket", &socket)?;
        if socket == "default" {
            return Err(TmuxError::Refused("refusing the default tmux server"));
        }
        Ok(Tmux {
            socket: Some(socket),
            binary: "tmux".to_owned(),
            tmpdir: Some(dir.into()),
            owner: None,
            explicit_socket: true,
        })
    }

    /// Return the private socket name, when one is configured.
    pub fn socket_name(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    /// Return the private `TMUX_TMPDIR`, when one is configured.
    pub fn socket_dir(&self) -> Option<&Path> {
        self.tmpdir.as_deref()
    }

    /// Return `<socket_dir>/<socket>` for containment assertions.
    ///
    /// This is the private root tmux uses, not the final socket path: tmux
    /// creates its own `tmux-<uid>` directory under `TMUX_TMPDIR`, so the real
    /// socket is `<socket_dir>/tmux-<uid>/<socket>`. Use `TMUX_TMPDIR` rather
    /// than a `-S` built from this value when starting a client.
    pub fn socket_path(&self) -> Option<PathBuf> {
        match (&self.socket, &self.tmpdir) {
            (Some(socket), Some(dir)) => Some(dir.join(socket)),
            _ => None,
        }
    }

    /// Return the pid of the tmux server this manager is connected to.
    ///
    /// `None` means no server is running on the private socket.
    pub fn server_pid(&self) -> Result<Option<u32>, TmuxError> {
        let args = vec!["list-sessions".to_owned(), "-F".to_owned(), "#{pid}".to_owned()];
        let output = self.execute(&args)?;
        if !output.status.success() {
            if output.stderr.contains("no server running") {
                return Ok(None);
            }
            return Err(Self::command_error(&args, &output));
        }
        Ok(output.stdout.lines().next().and_then(|line| line.trim().parse().ok()))
    }

    /// Whether `tmux -V` runs successfully.
    pub fn has_tmux(&self) -> bool {
        let args = vec!["-V".to_owned()];
        match self.execute(&args) {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Whether the session exists. Foreign sessions are refused.
    pub fn session_exists(&self, session: &str) -> Result<bool, TmuxError> {
        require_vowp_session(session)?;
        self.session_exists_raw(session)
    }

    /// Set the `@vowp_title` window option (never rename the window).
    pub fn set_window_title(&self, session: &str, name: &str, title: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        validate_name("title", title)?;
        let target = self.window_target(session, name)?;
        self.set_option(&target, "@vowp_title", title)
    }

    /// Display only: capture the last `lines` lines of a pane.
    ///
    /// Decisions must never depend on this text; logs drive decisions.
    pub fn capture_pane(&self, session: &str, name: &str, lines: u32) -> Result<String, TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        let target = self.window_target(session, name)?;
        let lines = lines.clamp(1, 2000);
        let args = vec![
            "capture-pane".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            target,
            "-S".to_owned(),
            format!("-{lines}"),
        ];
        self.run_checked(&args)
    }

    /// Kill a session, but only when the server was recorded by `vowp up`,
    /// every window in it is tagged and the session is in the `vowp-`
    /// namespace.
    pub fn kill_session(&self, session: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        if !self.session_exists_raw(session)? {
            return Ok(());
        }
        self.require_owned_server()?;
        let windows = self.list_windows(session)?;
        if let Some(window) = windows.iter().find(|w| !w.tagged) {
            return Err(TmuxError::NotTagged {
                session: session.to_owned(),
                name: window.name.clone(),
            });
        }
        let args = vec!["kill-session".to_owned(), "-t".to_owned(), format!("={session}")];
        self.run_checked(&args).map(|_| ())
    }

    /// The argv a caller execs or spawns to attach to a session.
    ///
    /// With a private directory this starts with
    /// `["env", "-u", "TMUX", "-u", "TMUX_PANE", "TMUX_TMPDIR=<dir>", "tmux", ...]`,
    /// so the client finds the same private server even when `vowp attach` is
    /// run from inside the user's tmux session. Without one it is
    /// `["tmux", ("-L", sock)?, "attach-session", "-t", session]`.
    pub fn attach_argv(&self, session: &str) -> Vec<String> {
        let mut argv = Vec::new();
        if let Some(dir) = &self.tmpdir {
            argv.push("env".to_owned());
            argv.push("-u".to_owned());
            argv.push("TMUX".to_owned());
            argv.push("-u".to_owned());
            argv.push("TMUX_PANE".to_owned());
            argv.push(format!("TMUX_TMPDIR={}", dir.display()));
        }
        argv.push(self.binary.clone());
        if let Some(socket) = &self.socket {
            argv.push("-L".to_owned());
            argv.push(socket.clone());
        }
        argv.push("attach-session".to_owned());
        argv.push("-t".to_owned());
        argv.push(session.to_owned());
        argv
    }

    /// Kill the private server owned by a test guard.
    ///
    /// Returns [`TmuxError::Refused`] when the caller did not name an explicit
    /// private socket, before spawning anything, so it can never touch the
    /// user's default server or a process-scoped socket it does not own.
    pub fn kill_server_for_tests(&self) -> Result<(), TmuxError> {
        match &self.socket {
            Some(socket) if self.explicit_socket && socket != "default" => {}
            _ => {
                return Err(TmuxError::Refused(
                    "refusing to kill a tmux server this guard did not create",
                ));
            }
        }
        let args = vec!["kill-server".to_owned()];
        match self.execute(&args) {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) if output.stderr.contains("no server running") => Ok(()),
            Ok(output) => Err(Self::command_error(&args, &output)),
            Err(error) => Err(error),
        }
    }

    fn session_exists_raw(&self, session: &str) -> Result<bool, TmuxError> {
        let args = vec!["has-session".to_owned(), "-t".to_owned(), format!("={session}")];
        let output = self.execute(&args)?;
        Ok(output.status.success())
    }

    fn command(&self, args: &[String]) -> Result<Command, TmuxError> {
        let version_only = args.len() == 1 && args[0] == "-V";
        if !version_only {
            match self.socket.as_deref() {
                Some("default") => {
                    return Err(TmuxError::Refused("refusing the default tmux server"));
                }
                None => {
                    return Err(TmuxError::Refused(
                        "refusing the default tmux server: no private socket is configured",
                    ));
                }
                Some(_) => {}
            }
        }
        let mut command = Command::new(&self.binary);
        if let Some(socket) = &self.socket {
            command.arg("-L").arg(socket);
        }
        if let Some(dir) = &self.tmpdir {
            fs::create_dir_all(dir)?;
            command.env("TMUX_TMPDIR", dir);
        }
        command.args(args);
        command
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    fn execute(&self, args: &[String]) -> Result<Completed, TmuxError> {
        let mut child = self.command(args)?.spawn().map_err(spawn_error)?;
        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stdout = join_reader(stdout_reader)?;
                    let stderr = join_reader(stderr_reader)?;
                    return Ok(Completed { status, stdout, stderr });
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        discard_reader(stdout_reader);
                        discard_reader(stderr_reader);
                        return Err(TmuxError::Timeout(TIMEOUT));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    discard_reader(stdout_reader);
                    discard_reader(stderr_reader);
                    return Err(TmuxError::Io(error));
                }
            }
        }
    }

    /// Record the private socket and the current server pid under the node home.
    fn record_server(&self) -> Result<(), TmuxError> {
        let Some(home) = &self.owner else {
            return Ok(());
        };
        let socket = self.socket.as_deref().ok_or(TmuxError::Refused(
            "cannot record a tmux server without a private socket",
        ))?;
        let pid = self
            .server_pid()?
            .ok_or(TmuxError::Refused("cannot record a tmux server that is not running"))?;
        fs::create_dir_all(home)?;
        let path = home.join(SERVER_RECORD);
        let temp = home.join(format!("{SERVER_RECORD}.tmp"));
        fs::write(&temp, format!("socket={socket}\npid={pid}\n"))?;
        fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Refuse a destructive operation when the server is not the one `vowp up`
    /// recorded for this node home.
    fn require_owned_server(&self) -> Result<(), TmuxError> {
        let Some(home) = &self.owner else {
            return Ok(());
        };
        let path = home.join(SERVER_RECORD);
        let Ok(text) = fs::read_to_string(&path) else {
            return Err(TmuxError::Refused(
                "tmux server was not recorded by vowp up; refusing to clean it up",
            ));
        };
        let mut recorded_socket = None;
        let mut recorded_pid = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("socket=") {
                recorded_socket = Some(value.trim().to_owned());
            }
            if let Some(value) = line.strip_prefix("pid=") {
                recorded_pid = value.trim().parse::<u32>().ok();
            }
        }
        let Some(socket) = self.socket.as_deref() else {
            return Err(TmuxError::Refused("refusing a tmux server without a private socket"));
        };
        if recorded_socket.as_deref() != Some(socket) {
            return Err(TmuxError::Refused(
                "tmux server socket does not match the server recorded by vowp up",
            ));
        }
        let current = self
            .server_pid()?
            .ok_or(TmuxError::Refused("tmux server is not running"))?;
        if recorded_pid != Some(current) {
            return Err(TmuxError::Refused(
                "tmux server pid does not match the server recorded by vowp up",
            ));
        }
        Ok(())
    }

    fn run_checked(&self, args: &[String]) -> Result<String, TmuxError> {
        self.run_checked_display(args, args)
    }

    fn run_checked_display(&self, display_args: &[String], args: &[String]) -> Result<String, TmuxError> {
        let output = self.execute(args)?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(Self::command_error(display_args, &output))
        }
    }

    fn command_error(display_args: &[String], output: &Completed) -> TmuxError {
        TmuxError::Command {
            args: display_args.to_vec(),
            status: output.status.code(),
            stderr: output.stderr.trim_end().to_owned(),
        }
    }

    fn list_windows_raw(&self, session: &str) -> Result<Vec<RawWindow>, TmuxError> {
        let args = vec![
            "list-windows".to_owned(),
            "-t".to_owned(),
            format!("={session}"),
            "-F".to_owned(),
            WINDOW_FORMAT.to_owned(),
        ];
        let output = self.execute(&args)?;
        if !output.status.success() {
            if self.session_exists_raw(session)? {
                return Err(Self::command_error(&args, &output));
            }
            return Ok(Vec::new());
        }
        let mut windows = Vec::new();
        for line in output.stdout.lines() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('|').collect();
            if fields.len() != 6 {
                return Err(TmuxError::Command {
                    args: args.clone(),
                    status: Some(0),
                    stderr: format!("unexpected list-windows output: {line:?}"),
                });
            }
            let index = fields[1].parse::<u32>().map_err(|_| TmuxError::Command {
                args: args.clone(),
                status: Some(0),
                stderr: format!("unexpected window index in {line:?}"),
            })?;
            let pane_pid = if fields[4].is_empty() {
                None
            } else {
                Some(fields[4].parse::<u32>().map_err(|_| TmuxError::Command {
                    args: args.clone(),
                    status: Some(0),
                    stderr: format!("unexpected pane pid in {line:?}"),
                })?)
            };
            windows.push(RawWindow {
                info: WindowInfo {
                    index,
                    name: fields[2].to_owned(),
                    tagged: fields[3] == "1",
                    pane_pid,
                    active: fields[5] == "1",
                },
                id: fields[0].to_owned(),
            });
        }
        Ok(windows)
    }

    fn window_target(&self, session: &str, name: &str) -> Result<String, TmuxError> {
        let windows = self.list_windows_raw(session)?;
        windows
            .into_iter()
            .find(|window| window.info.name == name)
            .map(|window| window.id)
            .ok_or_else(|| TmuxError::WindowNotFound {
                session: session.to_owned(),
                name: name.to_owned(),
            })
    }

    fn set_option(&self, target: &str, option: &str, value: &str) -> Result<(), TmuxError> {
        let args = vec![
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            target.to_owned(),
            option.to_owned(),
            value.to_owned(),
        ];
        self.run_checked(&args).map(|_| ())
    }

    fn create_session(
        &self,
        session: &str,
        window: &str,
        argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<(), TmuxError> {
        if argv.is_empty() {
            return Err(TmuxError::EmptyCommand);
        }
        if let Some(cwd) = cwd {
            if !cwd.is_dir() {
                return Err(TmuxError::InvalidCwd(cwd.display().to_string()));
            }
        }
        let command = window_command(argv, &[])?;
        let args = session_args(session, window, cwd, command);
        let id = self.run_checked(&args)?.trim().to_owned();
        if id.is_empty() {
            return Err(TmuxError::Command {
                args,
                status: Some(0),
                stderr: "tmux new-session returned no window id".to_owned(),
            });
        }
        if let Err(error) = self.set_option(&id, "@vowp", "1") {
            let _ = self.execute(&["kill-session".to_owned(), "-t".to_owned(), format!("={session}")]);
            return Err(error);
        }
        Ok(())
    }

    fn check_cwd(cwd: Option<&Path>) -> Result<(), TmuxError> {
        if let Some(cwd) = cwd {
            if !cwd.is_dir() {
                return Err(TmuxError::InvalidCwd(cwd.display().to_string()));
            }
        }
        Ok(())
    }
}

fn spawn_error(error: std::io::Error) -> TmuxError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => TmuxError::NotInstalled,
        _ => TmuxError::Io(error),
    }
}

fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<std::io::Result<String>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        pipe.read_to_end(&mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    })
}

fn join_reader(handle: Option<std::thread::JoinHandle<std::io::Result<String>>>) -> Result<String, TmuxError> {
    match handle {
        Some(handle) => match handle.join() {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(error)) => Err(TmuxError::Io(error)),
            Err(_) => Err(TmuxError::Io(std::io::Error::other("tmux output reader panicked"))),
        },
        None => Ok(String::new()),
    }
}

fn discard_reader(handle: Option<std::thread::JoinHandle<std::io::Result<String>>>) {
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

fn session_args(session: &str, window: &str, cwd: Option<&Path>, command: String) -> Vec<String> {
    let mut args = vec![
        "new-session".to_owned(),
        "-d".to_owned(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{window_id}".to_owned(),
        "-s".to_owned(),
        session.to_owned(),
        "-n".to_owned(),
        window.to_owned(),
    ];
    if let Some(cwd) = cwd {
        args.push("-c".to_owned());
        args.push(cwd.display().to_string());
    }
    args.push("--".to_owned());
    args.push(command);
    args
}

fn window_args(session: &str, name: &str, cwd: Option<&Path>, command: String) -> Vec<String> {
    let mut args = vec![
        "new-window".to_owned(),
        "-d".to_owned(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{window_id}".to_owned(),
        "-t".to_owned(),
        format!("{session}:"),
        "-n".to_owned(),
        name.to_owned(),
    ];
    if let Some(cwd) = cwd {
        args.push("-c".to_owned());
        args.push(cwd.display().to_string());
    }
    args.push("--".to_owned());
    args.push(command);
    args
}

impl WindowManager for Tmux {
    fn ensure_session(&self, session: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        if !self.session_exists_raw(session)? {
            let argv = vec!["sh".to_owned(), "-c".to_owned(), "exec cat".to_owned()];
            self.create_session(session, "scratch", &argv, None)?;
        }
        self.record_server()
    }

    fn new_window(&self, session: &str, name: &str, spec: &WindowSpec) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        Self::check_cwd(spec.cwd.as_deref())?;
        if self.window_exists(session, name)? {
            return Err(TmuxError::WindowExists {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        }
        let command = window_command(&spec.argv, &spec.env)?;
        let display_command = window_command(&spec.argv, &[])?;
        let args = window_args(session, name, spec.cwd.as_deref(), command);
        let display_args = window_args(session, name, spec.cwd.as_deref(), display_command);
        let id = self.run_checked_display(&display_args, &args)?.trim().to_owned();
        if id.is_empty() {
            return Err(TmuxError::Command {
                args: display_args,
                status: Some(0),
                stderr: "tmux new-window returned no window id".to_owned(),
            });
        }
        if let Err(error) = self.set_option(&id, "@vowp", "1") {
            // A command that exits immediately can take its window with it
            // before the tag command runs. The window is already gone, so
            // there is nothing unsafe left behind; the command itself ran.
            if matches!(
                &error,
                TmuxError::Command { stderr, .. } if stderr.contains("no such window")
            ) {
                return Ok(());
            }
            let _ = self.execute(&["kill-window".to_owned(), "-t".to_owned(), id]);
            return Err(error);
        }
        Ok(())
    }

    fn kill_window(&self, session: &str, name: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        let target = self.window_target(session, name)?;
        self.require_owned_server()?;
        let args = vec![
            "show-options".to_owned(),
            "-w".to_owned(),
            "-v".to_owned(),
            "-t".to_owned(),
            target.clone(),
            "@vowp".to_owned(),
        ];
        let output = self.execute(&args)?;
        if !output.status.success() || output.stdout.trim() != "1" {
            return Err(TmuxError::NotTagged {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        }
        let args = vec!["kill-window".to_owned(), "-t".to_owned(), target];
        self.run_checked(&args).map(|_| ())
    }

    fn list_windows(&self, session: &str) -> Result<Vec<WindowInfo>, TmuxError> {
        require_vowp_session(session)?;
        let windows = self.list_windows_raw(session)?;
        if !windows.is_empty() {
            // `vowp down --kill-windows` lists first and then kills each window;
            // refusing an unrecorded or replaced server here makes the whole
            // cleanup fail loudly instead of silently skipping every kill.
            self.require_owned_server()?;
        }
        Ok(windows.into_iter().map(|window| window.info).collect())
    }

    fn window_exists(&self, session: &str, name: &str) -> Result<bool, TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        Ok(self
            .list_windows_raw(session)?
            .iter()
            .any(|window| window.info.name == name))
    }

    fn ensure_layout(&self, project: &str, spec: &LayoutSpec) -> Result<(), TmuxError> {
        let session = session_name_for_project(project);
        require_vowp_session(&session)?;
        if !self.session_exists_raw(&session)? {
            self.create_session(&session, "board", &spec.board, spec.cwd.as_deref())?;
            self.record_server()?;
        } else {
            self.record_server()?;
            if !self.window_exists(&session, "board")? {
                self.new_window(
                    &session,
                    "board",
                    &WindowSpec {
                        argv: spec.board.clone(),
                        cwd: spec.cwd.clone(),
                        env: Vec::new(),
                    },
                )?;
            }
            if self.window_exists(&session, "scratch")? {
                self.kill_window(&session, "scratch")?;
            }
        }
        let windows = [
            ("supervisor", &spec.supervisor),
            ("planner", &spec.planner),
            ("keeper", &spec.keeper),
            ("inbox", &spec.inbox),
        ];
        for (name, argv) in windows {
            if !self.window_exists(&session, name)? {
                self.new_window(
                    &session,
                    name,
                    &WindowSpec {
                        argv: argv.clone(),
                        cwd: spec.cwd.clone(),
                        env: Vec::new(),
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn current_scope() -> Option<Scope> {
    CURRENT_SCOPE.with(|current| current.borrow().clone())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Deterministic private tmux socket name for one node home.
///
/// The name is `vowp-` plus a 64-bit FNV-1a hash of the home path. It is safe
/// for `tmux -L` and stable across `vowp up`, the detached supervisor and a
/// later `vowp down` running with the same home.
pub fn socket_name_for_home(home: &Path) -> String {
    let canonical = canonical_or_original(home);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("vowp-{hash:016x}")
}
