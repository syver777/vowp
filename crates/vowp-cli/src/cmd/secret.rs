//! `vowp secret` and the hidden vault helpers.
//!
//! Values are read from a hidden prompt or from stdin, never from argv. The CLI only
//! renders the core vault API; storage, injection and redaction live in `vowp-core`.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9 and §12.

use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use clap::{Args, Subcommand};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle, Signals};
use vowp_core::vault::{self, REQUEST_TTL, SecretRequestServer, Upgrade};
use vowp_protocol::{SecretInfo, SecretName, VaultMode};

use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp secret`.
#[derive(Debug, Clone, Args)]
pub struct SecretArgs {
    /// The secret subcommand.
    #[command(subcommand)]
    pub command: SecretCommand,
}

/// The `vowp secret` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum SecretCommand {
    /// Store a secret through a hidden prompt.
    Set(SecretSetArgs),
    /// List secret names and their last four characters.
    Ls,
    /// Remove a secret.
    Rm(SecretRmArgs),
    /// Move basic-mode secrets to the vault user.
    Upgrade,
    /// Open a one-time paste page for a secret.
    Request(SecretRequestArgs),
}

/// Arguments for `vowp secret set`.
#[derive(Debug, Clone, Args)]
pub struct SecretSetArgs {
    /// Secret name (`^[A-Z][A-Z0-9_]{0,63}$`).
    pub name: String,
}

/// Arguments for `vowp secret rm`.
#[derive(Debug, Clone, Args)]
pub struct SecretRmArgs {
    /// Secret name.
    pub name: String,
    /// Confirm the removal.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp secret request`.
#[derive(Debug, Clone, Args)]
pub struct SecretRequestArgs {
    /// Secret name.
    pub name: String,
}

/// Arguments for the hidden `vowp __vault-exec`.
#[derive(Debug, Clone, Args)]
pub struct VaultExecArgs {
    /// Name of the one variable to inject.
    #[arg(long, value_name = "NAME")]
    pub env: String,
    /// Program and arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true)]
    pub argv: Vec<String>,
}

/// Arguments for the hidden `vowp __vault-admin`.
#[derive(Debug, Clone, Args)]
pub struct VaultAdminArgs {
    /// The admin action.
    #[command(subcommand)]
    pub command: VaultAdminCommand,
}

/// The hidden `__vault-admin` actions.
#[derive(Debug, Clone, Subcommand)]
pub enum VaultAdminCommand {
    /// Check that the helper context and vault directory are usable.
    Probe,
    /// Read one value from stdin and write it as the vault user.
    Set(VaultNameArgs),
    /// Remove one secret as the vault user.
    Rm(VaultNameArgs),
}

/// Arguments carrying only a secret name.
#[derive(Debug, Clone, Args)]
pub struct VaultNameArgs {
    /// Secret name.
    pub name: String,
}

/// Runs `vowp secret`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &SecretArgs) -> Result<i32, CliError> {
    match &args.command {
        SecretCommand::Set(set) => set_secret(ctx, io, set),
        SecretCommand::Ls => list_secrets(ctx, io),
        SecretCommand::Rm(remove) => remove_secret(ctx, io, remove),
        SecretCommand::Upgrade => upgrade(ctx, io),
        SecretCommand::Request(request) => request_secret(ctx, io, request),
    }
}

/// Runs the hidden `vowp __vault-exec`.
pub fn vault_exec(ctx: &Ctx, _io: &mut dyn IoStreams, args: &VaultExecArgs) -> Result<i32, CliError> {
    let name = SecretName::parse(&args.env)?;
    let value =
        vault::read_value_for_exec(ctx.deps(), &name)?.ok_or_else(|| CliError::NotFound(format!("secret {name}")))?;
    let Some(program) = args.argv.first() else {
        return Err(CliError::Invalid("__vault-exec needs a program after --".to_owned()));
    };
    // `exec` replaces this process; only a spawn failure returns.
    let error = Command::new(program)
        .args(&args.argv[1..])
        .env(name.as_str(), value)
        .exec();
    Err(CliError::Io(error))
}

/// Runs the hidden `vowp __vault-admin`.
pub fn vault_admin(ctx: &Ctx, io: &mut dyn IoStreams, args: &VaultAdminArgs) -> Result<i32, CliError> {
    match &args.command {
        VaultAdminCommand::Probe => {
            vault::admin_probe(ctx.deps())?;
            Ok(0)
        }
        VaultAdminCommand::Set(set) => {
            let name = SecretName::parse(&set.name)?;
            let value = crate::read_stdin(io)?;
            if value.is_empty() {
                return Err(CliError::Invalid("secret value must not be empty".to_owned()));
            }
            let info = vault::admin_set(ctx.deps(), &name, &value)?;
            writeln!(io.out(), "saved {} ends in …{}", info.name, info.last4)?;
            Ok(0)
        }
        VaultAdminCommand::Rm(remove) => {
            let name = SecretName::parse(&remove.name)?;
            if vault::admin_remove(ctx.deps(), &name)? {
                writeln!(io.out(), "removed {name}")?;
            } else {
                writeln!(io.out(), "missing {name}")?;
            }
            Ok(0)
        }
    }
}

/// Stores a secret after reading it from a hidden terminal prompt.
fn set_secret(ctx: &Ctx, io: &mut dyn IoStreams, args: &SecretSetArgs) -> Result<i32, CliError> {
    let name = SecretName::parse(&args.name)?;
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Guard(
            "run it in a terminal or use `vowp secret request`".to_owned(),
        ));
    }
    let mut guard = EchoGuard::hide()?;
    write!(io.err(), "value for {name}: ")?;
    io.err().flush()?;
    let mut line = String::new();
    let read = io.input().read_line(&mut line);
    guard.restore()?;
    read?;
    writeln!(io.err())?;
    let value = line.trim_end_matches(&['\r', '\n'][..]).to_owned();
    let info = vault::write_secret(ctx.deps(), &name, &value)?;
    if ctx.json() {
        print_json(io, &info)?;
    } else {
        writeln!(io.out(), "{} saved, ends in …{}", info.name, info.last4)?;
    }
    Ok(0)
}

/// Lists secret names, last four characters and update times.
fn list_secrets(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let infos = vault::list_secrets(ctx.deps())?;
    if ctx.json() {
        print_json(io, &infos)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut table = Table::new(&["NAME", "LAST4", "UPDATED"]);
    for info in &infos {
        table.row(vec![
            info.name.to_string(),
            info.last4.clone(),
            local_time_at(info.updated_at, now, ctx.tz_offset_minutes()),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Removes one secret after the explicit `--yes` guard.
fn remove_secret(ctx: &Ctx, io: &mut dyn IoStreams, args: &SecretRmArgs) -> Result<i32, CliError> {
    if !args.yes {
        return Err(CliError::Guard(format!("removing secret {} requires --yes", args.name)));
    }
    let name = SecretName::parse(&args.name)?;
    if !vault::remove_secret(ctx.deps(), &name)? {
        return Err(CliError::NotFound(format!("secret {name}")));
    }
    if ctx.json() {
        print_json(io, &serde_json::json!({"name": name.to_string(), "removed": true}))?;
    } else {
        writeln!(io.out(), "{name} removed")?;
    }
    Ok(0)
}

/// Prints or completes the basic-to-vault upgrade.
fn upgrade(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    match vault::upgrade(ctx.deps())? {
        Upgrade::AlreadyVault => {
            if ctx.json() {
                print_json(io, &serde_json::json!({"mode": VaultMode::VaultUser}))?;
            } else {
                writeln!(io.out(), "secrets: vault user")?;
            }
            Ok(0)
        }
        Upgrade::Switched => {
            if ctx.json() {
                print_json(io, &serde_json::json!({"mode": VaultMode::VaultUser}))?;
            } else {
                writeln!(io.out(), "secrets: vault user (switched from basic mode)")?;
            }
            Ok(0)
        }
        Upgrade::NeedsCommand(command) => Err(CliError::Guard(format!(
            "run this command, then re-run `vowp secret upgrade`:\n{command}"
        ))),
        Upgrade::NoSudo(reason) => Err(CliError::Guard(reason)),
    }
}

/// Starts the one-time paste page and waits for the owner's value.
fn request_secret(ctx: &Ctx, io: &mut dyn IoStreams, args: &SecretRequestArgs) -> Result<i32, CliError> {
    let name = SecretName::parse(&args.name)?;
    let server = SecretRequestServer::bind(ctx.deps(), &name, ctx.clock().now())?;
    let request = server.request().clone();
    if ctx.json() {
        print_json(io, &request)?;
    } else {
        writeln!(io.out(), "open {}", request.url)?;
        writeln!(
            io.out(),
            "expires {} (single use, 10 minutes)",
            request.expires_at.as_rfc3339()
        )?;
    }
    io.out().flush()?;
    let info: SecretInfo = server.wait(REQUEST_TTL)?;
    if !ctx.json() {
        writeln!(io.out(), "{} saved, ends in …{}", info.name, info.last4)?;
    }
    Ok(0)
}

/// The action a signal takes after echo is restored; production exits the process.
type ExitAction = Arc<dyn Fn(i32) + Send + Sync>;

/// Runs one `stty` mode change; tests replace it with an in-process action.
type EchoAction = Arc<dyn Fn(bool) -> Result<(), String> + Send + Sync>;

/// Restores terminal echo on every return path, including SIGINT and SIGTERM.
///
/// The signal watcher records the signal before it takes the terminal lock, runs `stty echo`
/// from its own thread (safe outside a signal handler), and then exits with the conventional
/// `128 + signal` status. A hide that is already running sees the pending signal as soon as
/// its `stty -echo` child returns and restores echo itself before exiting, so the watcher
/// cannot restore first and let that child finish afterwards. A normal return restores echo
/// before the watcher is removed, so a late signal cannot leave it off.
struct EchoGuard {
    active: bool,
    terminal: Arc<EchoTerminal>,
    watcher: Option<SignalRestore>,
}

impl EchoGuard {
    /// Disables echo when the process has a terminal on stdin.
    ///
    /// A missing or failing `stty` is a hard error: a hidden prompt must never read a value
    /// while the terminal still echoes it.
    fn hide() -> Result<Self, CliError> {
        Self::hide_with(Path::new("stty"), |code| std::process::exit(code))
    }

    /// Builds a guard with an explicit `stty` program and exit action (used by tests).
    fn hide_with(stty: &Path, exit: impl Fn(i32) + Send + Sync + 'static) -> Result<Self, CliError> {
        Self::hide_with_terminal(Arc::new(EchoTerminal::new(stty)), exit)
    }

    /// Installs the watcher and hides echo through `terminal`.
    ///
    /// Tests construct `terminal` with an in-process echo action so the hide can be held at
    /// the `stty -echo` point without a real signal or a shell script.
    fn hide_with_terminal(
        terminal: Arc<EchoTerminal>,
        exit: impl Fn(i32) + Send + Sync + 'static,
    ) -> Result<Self, CliError> {
        let exit: ExitAction = Arc::new(exit);
        let watcher = SignalRestore::install(Arc::clone(&terminal), Arc::clone(&exit)).map_err(|error| {
            CliError::Guard(format!("could not install the terminal echo restore handler: {error}"))
        })?;
        if let Err(error) = terminal.set(false) {
            drop(watcher);
            return Err(CliError::Guard(format!("could not hide terminal echo: {error}")));
        }
        if let Some(signal) = terminal.take_signal() {
            let status = 128 + signal;
            if let Err(error) = terminal.restore_then(|| exit(status)) {
                eprintln!("vowp: {error}; terminal echo may still be disabled");
                exit(1);
            }
            return Err(CliError::Guard(format!(
                "terminal echo was restored after signal {signal}"
            )));
        }
        Ok(Self {
            active: true,
            terminal,
            watcher: Some(watcher),
        })
    }

    /// Restores echo and stops the signal watcher; a second call is a no-op.
    fn restore(&mut self) -> Result<(), CliError> {
        if !self.active {
            return Ok(());
        }
        self.terminal
            .set(true)
            .map_err(|error| CliError::Guard(format!("could not restore terminal echo: {error}")))?;
        self.active = false;
        drop(self.watcher.take());
        Ok(())
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("vowp: {error}");
        }
    }
}

/// One terminal's echo state, serialised between the prompt and the signal watcher.
struct EchoTerminal {
    apply: EchoAction,
    lock: Mutex<()>,
    signalled: AtomicI32,
}

impl EchoTerminal {
    /// Names the `stty` program this terminal is changed through.
    fn new(stty: &Path) -> Self {
        let stty = stty.to_path_buf();
        Self::with_action(move |enable| set_echo(&stty, enable))
    }

    /// Builds a terminal around an explicit echo action.
    fn with_action(apply: impl Fn(bool) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self {
            apply: Arc::new(apply),
            lock: Mutex::new(()),
            signalled: AtomicI32::new(0),
        }
    }

    /// Runs the echo action with one mode while holding the shared lock.
    fn set(&self, enable: bool) -> Result<(), String> {
        let _guard = self.lock();
        self.set_locked(enable)
    }

    /// Runs the echo action while the caller already holds the terminal lock.
    fn set_locked(&self, enable: bool) -> Result<(), String> {
        (self.apply)(enable)
    }

    /// Takes the terminal lock, recovering from a poisoned lock.
    ///
    /// The signal watcher keeps this guard until it exits the process, so any hide that has
    /// not started yet blocks instead of running after the restore.
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a signal for a hide that may still be waiting for its `stty -echo` child.
    fn note_signal(&self, signal: i32) {
        self.signalled.store(signal, Ordering::SeqCst);
    }

    /// Consumes the signal recorded while echo was being hidden, if any.
    fn take_signal(&self) -> Option<i32> {
        match self.signalled.swap(0, Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// Restores echo and runs `after` before releasing the terminal lock.
    ///
    /// A signalled hide exits through this method, so the watcher cannot run a second restore
    /// between the restore and the exit.
    fn restore_then(&self, after: impl FnOnce()) -> Result<(), String> {
        let _guard = self.lock();
        self.set_locked(true)?;
        after();
        Ok(())
    }
}

/// Runs `stty` with one echo mode, describing a spawn or exit failure.
fn set_echo(stty: &Path, enable: bool) -> Result<(), String> {
    let mode = if enable { "echo" } else { "-echo" };
    let output = Command::new(stty)
        .arg(mode)
        .stdin(Stdio::inherit())
        .output()
        .map_err(|error| format!("could not run `{} {mode}`: {error}", stty.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(format!(
        "`{} {mode}` exited {}{}",
        stty.display(),
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    ))
}

/// Watches SIGINT and SIGTERM while the hidden prompt is open.
struct SignalRestore {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl SignalRestore {
    /// Restores echo for SIGINT/SIGTERM; `exit` runs after the restore.
    ///
    /// The signal is recorded before the terminal lock is taken so an in-flight hide can
    /// restore echo as soon as its child returns. The lock is then deliberately held until
    /// `exit` ends the process, which orders this restore after any such child and keeps a
    /// hide that has not started yet from running afterwards.
    fn install(terminal: Arc<EchoTerminal>, exit: ExitAction) -> std::io::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        let handle = signals.handle();
        let thread = thread::Builder::new()
            .name("vowp-echo-restore".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    terminal.note_signal(signal);
                    let _guard = terminal.lock();
                    match terminal.set_locked(true) {
                        Ok(()) => exit(128 + signal),
                        Err(error) => {
                            eprintln!("vowp: {error}; terminal echo may still be disabled");
                            exit(1);
                        }
                    }
                }
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

impl Drop for SignalRestore {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;

    /// Appends one mode line to a test log.
    fn append(log: &Path, line: &str) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|error| format!("open {}: {error}", log.display()))?;
        writeln!(file, "{line}").map_err(|error| format!("write {}: {error}", log.display()))
    }

    /// Builds an in-process fake `stty` that records each mode.
    ///
    /// `fail` makes `-echo` return an error. `release` makes `-echo` first record `start` and
    /// then wait for that file, so a test can deliver a signal while the hide is in flight.
    fn logging_terminal(log: &Path, fail: bool, release: Option<PathBuf>) -> EchoTerminal {
        let log = log.to_path_buf();
        EchoTerminal::with_action(move |enable| {
            if let Some(release) = release.as_deref() {
                if !enable {
                    append(&log, "start")?;
                    wait_for_file(release)?;
                }
            }
            append(&log, if enable { "echo" } else { "-echo" })?;
            if fail && !enable {
                Err("fake stty failed".to_owned())
            } else {
                Ok(())
            }
        })
    }

    /// Waits until `path` exists, failing after five seconds.
    fn wait_for_file(path: &Path) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for {}", path.display()));
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Runs the ignored child that hides echo, raises `signal`, and must exit through the watcher.
    fn run_signal_child(log: &Path, signal: i32) -> std::process::Output {
        Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "cmd::secret::tests::echo_guard_signal_child", "--ignored"])
            .env("VOWP_TEST_ECHO_LOG", log)
            .env("VOWP_TEST_ECHO_SIGNAL", signal.to_string())
            .output()
            .expect("run the signal child")
    }

    /// Returns the modes the fake `stty` recorded.
    fn logged(log: &Path) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn echo_guard_restores_echo_when_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("echo.log");
        let terminal = Arc::new(logging_terminal(&log, false, None));

        {
            let _guard = EchoGuard::hide_with_terminal(terminal, |_| {}).expect("hide echo");
            assert_eq!(logged(&log), vec!["-echo"], "echo must be off while the prompt is open");
        }

        assert_eq!(logged(&log), vec!["-echo", "echo"], "dropping the guard restores echo");
    }

    #[test]
    fn echo_guard_refuses_when_stty_cannot_disable_echo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("echo.log");
        let terminal = Arc::new(logging_terminal(&log, true, None));

        let error = match EchoGuard::hide_with_terminal(terminal, |_| {}) {
            Ok(_) => panic!("a failing stty must be a hard error"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("could not hide terminal echo"), "{error}");
        assert_eq!(logged(&log), vec!["-echo"], "a failed hide does not run a restore");
    }

    #[test]
    #[ignore = "child process for the echo guard signal tests"]
    fn echo_guard_signal_child() {
        let log = PathBuf::from(std::env::var("VOWP_TEST_ECHO_LOG").expect("parent passes an echo log path"));
        let signal = std::env::var("VOWP_TEST_ECHO_SIGNAL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(SIGINT);
        let terminal = Arc::new(logging_terminal(&log, false, None));
        let _guard = EchoGuard::hide_with_terminal(terminal, |code| std::process::exit(code)).expect("hide echo");
        signal_hook::low_level::raise(signal).expect("raise signal");
        thread::sleep(Duration::from_secs(5));
        panic!("signal {signal} did not restore echo and exit");
    }

    #[test]
    fn echo_guard_restores_echo_on_sigint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("echo.log");

        let output = run_signal_child(&log, SIGINT);

        assert_eq!(
            output.status.code(),
            Some(130),
            "the child must exit with 128 + SIGINT; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            logged(&log),
            vec!["-echo", "echo"],
            "the SIGINT path must restore echo before exiting"
        );
    }

    #[test]
    fn echo_guard_restores_echo_on_sigterm() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("echo.log");

        let output = run_signal_child(&log, SIGTERM);

        assert_eq!(
            output.status.code(),
            Some(143),
            "the child must exit with 128 + SIGTERM; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            logged(&log),
            vec!["-echo", "echo"],
            "the SIGTERM path must restore echo before exiting"
        );
    }

    #[test]
    fn echo_guard_restores_echo_when_a_signal_arrives_during_hide() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let terminal = Arc::new(EchoTerminal::with_action({
            let calls = Arc::clone(&calls);
            let started_tx = Arc::clone(&started_tx);
            let release_rx = Arc::clone(&release_rx);
            move |enable| {
                calls.lock().expect("record echo call").push(enable);
                if !enable {
                    let started = started_tx
                        .lock()
                        .expect("start sender")
                        .take()
                        .expect("the hide starts once");
                    started
                        .send(())
                        .map_err(|_| "test dropped the start receiver".to_owned())?;
                    let release = release_rx
                        .lock()
                        .expect("release receiver")
                        .take()
                        .expect("the hide is released once");
                    release
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .map_err(|_| "test did not release the hide".to_owned())?;
                }
                Ok(())
            }
        }));
        let status = Arc::new(AtomicI32::new(0));
        let recorded = Arc::clone(&status);
        let signaler = {
            let terminal = Arc::clone(&terminal);
            thread::spawn(move || {
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the hide must start");
                terminal.note_signal(SIGINT);
                release_tx.send(()).expect("release the hide");
            })
        };

        let result = EchoGuard::hide_with_terminal(terminal, move |code| {
            recorded.store(code, Ordering::SeqCst);
        });

        signaler.join().expect("signal thread");
        assert!(result.is_err(), "a signalled hide must not return a live guard");
        assert_eq!(
            status.load(Ordering::SeqCst),
            128 + SIGINT,
            "the interrupted hide must exit with 128 + SIGINT"
        );
        assert_eq!(
            *calls.lock().expect("echo calls"),
            vec![false, true],
            "the hide must finish before the restore runs"
        );
    }

    #[test]
    #[ignore = "child process for echo_guard_waits_for_hide_before_restoring"]
    fn echo_guard_signal_during_hide_child() {
        let log = PathBuf::from(std::env::var("VOWP_TEST_ECHO_LOG").expect("parent passes an echo log path"));
        let release = PathBuf::from(std::env::var("VOWP_TEST_ECHO_RELEASE").expect("parent passes a release path"));
        let raiser = {
            let log = log.clone();
            let release = release.clone();
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !logged(&log).iter().any(|line| line == "start") {
                    assert!(Instant::now() < deadline, "timed out waiting for the hide to start");
                    thread::sleep(Duration::from_millis(5));
                }
                signal_hook::low_level::raise(SIGINT).expect("raise SIGINT");
                fs::write(&release, b"go").expect("release the hide");
            })
        };
        let terminal = Arc::new(logging_terminal(&log, false, Some(release)));
        let _guard = EchoGuard::hide_with_terminal(terminal, |code| std::process::exit(code)).expect("hide echo");
        let _ = raiser.join();
        thread::sleep(Duration::from_secs(5));
        panic!("the SIGINT path did not end the process");
    }

    #[test]
    fn echo_guard_waits_for_hide_before_restoring() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("echo.log");
        let release = tmp.path().join("release");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "cmd::secret::tests::echo_guard_signal_during_hide_child",
                "--ignored",
            ])
            .env("VOWP_TEST_ECHO_LOG", &log)
            .env("VOWP_TEST_ECHO_RELEASE", &release)
            .output()
            .expect("run the hide-race child");

        assert_eq!(
            output.status.code(),
            Some(130),
            "the child must exit with 128 + SIGINT; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            logged(&log),
            vec!["start", "-echo", "echo"],
            "the signal restore must wait for the in-flight stty -echo"
        );
    }
}
