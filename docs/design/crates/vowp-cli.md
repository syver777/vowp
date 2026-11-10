# crate `vowp-cli` (binary `vowp`)

Thin binary over `vowp-core`: argument parsing (clap derive), rendering (human + `--json`), exit codes, `--node` over ssh, signal handling.
All behaviour of commands is in `cli.md`; this file fixes the **module layout, ownership, shared API and tests**. `anyhow` is allowed only here
(`main.rs`); inner modules return `CliError`. Dependencies: `conventions.md` §3.

## 1. Module layout

The crate is `[[bin]] name = "vowp"`, with a tiny `src/lib.rs` exposing `pub fn run(args: Vec<String>, io: &mut dyn IoStreams) -> i32` so tests call commands in-process
(no subprocess needed except for the few `assert_cmd` smoke tests).

```
crates/vowp-cli/
  Cargo.toml
  src/main.rs         fn main() -> ExitCode { vowp_cli::run(std::env::args().collect(), &mut RealIo) }   (anyhow only here, for panics/IO setup)
  src/lib.rs          pub fn run, IoStreams trait (stdout/stderr/stdin writers+readers; tests use a buffer impl), re-exports
  src/cli.rs          clap derive structs for EVERY command in cli.md (global flags + subcommand enum), parse -> `Command`
  src/ctx.rs          Ctx { paths, deps: vowp_core::Deps, project: ProjectName, json: bool, color: bool }, Ctx::build(&Global) (loads vowp.toml, opens Deps with SystemRunner/SystemGit/Tmux)
  src/output.rs       Table (aligned columns), `Out::{human, json}`, time formatting, `json_error`
  src/errors.rs       CliError + exit-code mapping (cli.md §1.1)
  src/remote.rs       --node over ssh: build_ssh_argv, run_remote
  src/signals.rs      SIGTERM/SIGINT -> CancelToken with signal_hook::flag
  src/cmd/mod.rs      `pub mod` for all of the below and `pub fn dispatch(ctx, Command) -> Result<i32, CliError>`
  src/cmd/init.rs  doctor.rs  job.rs  graph.rs
  src/cmd/up.rs  status.rs  board.rs  attach.rs  plan.rs  window.rs   (`up`,`down`, `status`, `board`, `attach`, `plan`, hidden `__window`)
  src/cmd/session.rs  inbox.rs  cost.rs  node.rs  loops.rs  schedule.rs  maint.rs   (`maint` = reindex, backup, schema, version)
  src/cmd/promote.rs  (`promote`, `rollback`, `releases`)
  src/framework.rs    embeds the templates from the repo `framework/` dir with include_str!/include_dir-free explicit list; `init` writes them
  tests/*.rs          tests for the commands
```
Every command module in `src/cmd/` is real code. A command that cannot run returns an explicit error (exit code 1, message `not implemented in this build`);
no command may ship as a stub in a final build: the acceptance test `no_command_returns_not_implemented` iterates every command in `cli.rs` with `--help`-free dry arguments and fails on `NotImplemented`.

## 2. Shared API (frozen)

```rust
pub trait IoStreams { fn out(&mut self) -> &mut dyn std::io::Write; fn err(&mut self) -> &mut dyn std::io::Write; fn input(&mut self) -> &mut dyn std::io::BufRead; fn is_tty(&self) -> bool; }

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")] Invalid(String), #[error("{0}")] NotFound(String), #[error("{0}")] Conflict(String),
    #[error("{0}")] Guard(String), #[error("{0}")] Limited(String), #[error("remote: {0}")] Remote(String),
    #[error("not implemented: {0}")] NotImplemented(&'static str),
    #[error(transparent)] Core(#[from] vowp_core::CoreError), #[error(transparent)] Store(#[from] vowp_store::StoreError),
    #[error(transparent)] Protocol(#[from] vowp_protocol::ProtocolError), #[error("io: {0}")] Io(#[from] std::io::Error),
}
impl CliError { pub fn kind(&self) -> vowp_protocol::ErrorKind; pub fn exit_code(&self) -> i32; pub fn hint(&self) -> Option<String>; }

pub struct Global { pub json: bool, pub node: Option<String>, pub project: Option<String>, pub home: Option<PathBuf>, pub no_color: bool, pub quiet: bool, pub verbose: u8 }
pub struct Ctx { /* see layout */ }
impl Ctx { pub fn build(g: &Global, io: &dyn IoStreams) -> Result<Ctx, CliError>; pub fn store(&self) -> Result<vowp_store::Store, CliError>; pub fn clock(&self) -> &dyn Clock; }

pub struct Table { /* header + rows */ }
impl Table { pub fn new(headers: &[&str]) -> Self; pub fn row(&mut self, cells: Vec<String>); pub fn render(&self, color: bool) -> String; }
pub fn print_json<T: serde::Serialize>(io: &mut dyn IoStreams, v: &T) -> Result<(), CliError>;       // pretty-printed, trailing newline
pub fn print_error(io: &mut dyn IoStreams, e: &CliError, json: bool);                                   // cli.md §1.1 shapes
pub fn local_time(ts: Timestamp, now: Timestamp) -> String;                                           // "HH:MM" today else "MM-DD HH:MM" (UTC offset from [node] tz_offset_minutes)

pub mod remote {
    pub fn build_ssh_argv(target: &str, args: &[String], interactive: bool) -> Vec<String>;             // pure; quoting via vowp_tmux::quote::shell_quote
    pub fn run_remote(target: &str, args: &[String], interactive: bool) -> Result<i32, CliError>;        // spawns ssh; 255 -> Remote error
}
```

## 3. Rules

1. Every command handler is `fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &XArgs) -> Result<i32, CliError>`; no handler prints directly to `std::io`.
2. `--json` output types are the protocol types (types.md) wherever one exists (`Job`, `Session`, `InboxItem`, …); only summaries (`status`, `doctor`, `cost`) have CLI-local `Serialize` structs, defined next to the command.
3. Commands that mutate job state use `vowp_core::requests`: if a supervisor holds the lock they `push` a request and poll the job row ≤ 5 s; else `apply_direct`.
4. `Ctx::build` never creates `~/.vowp` implicitly except for `init`; other commands fail with `NotFound` ("run `vowp init`").
5. Interactive commands (`chat`, `attach`, `board`) check `is_tty()`; without a TTY they degrade as stated in `cli.md`.
6. Secrets: `doctor` reports env var **names** present/absent only.
7. `vowp --help` text for each command is the first line of its section in `cli.md`.

## 4. Named tests

Listed in `cli.md` §8, grouped by the file table above. Additional: `help_lists_every_v0_1_command` (compares clap's subcommand set with a constant list copied from `cli.md`),
`no_command_returns_not_implemented`, `json_flag_outputs_valid_json_for_every_listing_command`, `exit_codes_follow_error_kind`, `ctx_build_requires_init`, `io_streams_buffer_impl_captures_output`.

## 5. Out of scope
Shell completions (v0.2), TUI board beyond redraw loop, windows (OS) support.
