# crates/vowp-tmux.md — safe tmux window manager

Depends on `std` and `thiserror` only (no `vowp-protocol`: window names are plain strings; core maps job ids to names), keeping the dependency direction of conventions.md §2.
tmux windows are **views**: they run a read-only pretty log follower (`vowp __window job <id>`, which resolves the job's latest session and follows it with the `session tail --follow --pretty` renderer), never a harness TUI and never a control channel.

## 1. Module layout

```
crates/vowp-tmux/
  src/lib.rs        // re-exports, #![forbid(unsafe_code)]
  src/error.rs      // TmuxError
  src/quote.rs      // shell_quote, validate_name
  src/tmux.rs       // Tmux (real implementation) + run helper
  src/manager.rs    // WindowManager trait, WindowInfo, WindowSpec, ensure_layout, window_name_for_job
  src/fake.rs       // pub mod fake: FakeWindows (in-memory WindowManager)
  tests/            // tmux_private_socket.rs, safety_grep.rs, quote.rs
```

## 2. Public API

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo { pub index: u32, pub name: String, pub tagged: bool, pub pane_pid: Option<u32>, pub active: bool }

/// What to run in a new window. `argv` must be non-empty (an interactive shell window is never created).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowSpec { pub argv: Vec<String>, pub cwd: Option<std::path::PathBuf>, pub env: Vec<(String, String)> }

pub trait WindowManager: Send + Sync {
    fn ensure_session(&self, session: &str) -> Result<(), TmuxError>;
    fn new_window(&self, session: &str, name: &str, spec: &WindowSpec) -> Result<(), TmuxError>;   // tags @vowp=1; Err(WindowExists) if present
    fn kill_window(&self, session: &str, name: &str) -> Result<(), TmuxError>;                       // refuses untagged windows
    fn list_windows(&self, session: &str) -> Result<Vec<WindowInfo>, TmuxError>;                     // Ok(vec![]) if the session does not exist
    fn window_exists(&self, session: &str, name: &str) -> Result<bool, TmuxError>;
    /// Creates the fixed windows `board supervisor planner keeper inbox` (§4) in that order, idempotently. Part of the trait so core (which holds `Arc<dyn WindowManager>`) and CLI tests using `FakeWindows` can create the layout.
    fn ensure_layout(&self, project: &str, spec: &LayoutSpec) -> Result<(), TmuxError>;
}

/// Real tmux. `socket = Some("name")` runs every command as `tmux -L name ...` (tests, private servers); `None` uses the default server (CLI only).
#[derive(Debug, Clone)]
pub struct Tmux { /* socket: Option<String>, binary: String */ }

impl Tmux {
    pub fn new(socket: Option<String>) -> Tmux;
    pub fn with_binary(socket: Option<String>, binary: impl Into<String>) -> Tmux;   // tests/CI override
    pub fn has_tmux(&self) -> bool;                                                  // `tmux -V` succeeds
    pub fn session_exists(&self, session: &str) -> Result<bool, TmuxError>;
    pub fn set_window_title(&self, session: &str, name: &str, title: &str) -> Result<(), TmuxError>;   // `rename`-free: sets @vowp_title option
    /// Display only. `tmux capture-pane -p -t <target> -S -<lines>`. Decisions must never depend on it.
    pub fn capture_pane(&self, session: &str, name: &str, lines: u32) -> Result<String, TmuxError>;
    /// Only if `session` starts with "vowp-" and every window in it is tagged (or the session has no windows).
    pub fn kill_session(&self, session: &str) -> Result<(), TmuxError>;
    /// ["tmux", ("-L", sock)?, "attach-session", "-t", session] for the CLI to exec/spawn.
    pub fn attach_argv(&self, session: &str) -> Vec<String>;
    pub fn kill_server_for_tests(&self) -> Result<(), TmuxError>;                    // `tmux kill-server`; Err(Refused) when socket is None
}
impl WindowManager for Tmux {}

pub fn session_name_for_project(project: &str) -> String;     // "vowp-<project>" (project validated by validate_name)
pub fn window_name_for_job(job_id: &str) -> String;           // "job-<id>"

/// Commands for the fixed windows; each is an argv run in that window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutSpec {
    pub board: Vec<String>,        // e.g. ["vowp","__window","board"]
    pub supervisor: Vec<String>,   // e.g. ["vowp","__window","supervisor"]
    pub planner: Vec<String>,
    pub keeper: Vec<String>,
    pub inbox: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
}
pub const FIXED_WINDOWS: [&str; 5] = ["board", "supervisor", "planner", "keeper", "inbox"];

pub mod quote {
    /// POSIX single-quote escaping: wraps in '...' and rewrites each ' as '\''. The empty string becomes ''.
    pub fn shell_quote(s: &str) -> String;
    /// `[A-Za-z0-9_.-]+`, 1..=64 chars, must not start with '-' ; Err(InvalidName) otherwise.
    pub fn validate_name(kind: &'static str, s: &str) -> Result<(), crate::TmuxError>;
}

pub mod fake {
    /// In-memory WindowManager for core tests: records sessions/windows/specs, honours the same rules (names validated, tag always set, kill requires tag,
    /// `vowp-` prefix required). `FakeWindows::untagged_window(session,name)` lets tests simulate a foreign window.
    #[derive(Default, Clone)] pub struct FakeWindows { /* Arc<Mutex<..>> */ }
    impl FakeWindows {
        pub fn new() -> Self;
        pub fn untagged_window(&self, session: &str, name: &str);
        pub fn specs(&self, session: &str) -> Vec<(String, super::WindowSpec)>;   // creation order
        pub fn killed(&self) -> Vec<(String, String)>;
    }
    impl super::WindowManager for FakeWindows {}
}
```

## 3. Behaviour rules and invariants

1. **No respawn, no keystrokes — ever.** The crate exposes no `respawn-pane`, `respawn-window` or `send-keys` capability and never builds such a command line. Recycling a window means `kill_window` then `new_window`. (Design summary §5.1.) A test greps the source (§8).
2. **Only `vowp-` sessions.** Every operation taking a `session` first checks `session.starts_with("vowp-")` and `validate_name`; otherwise `Err(TmuxError::ForeignSession)`. `list_windows`/`window_exists` included. Other tmux sessions are invisible and untouchable.
3. **Tagging.** `new_window` always runs, immediately after creation, `tmux set-option -w -t <session>:<name> @vowp 1` (and creates the window with `-P -F '#{window_id}'` so the tag targets the returned window id, not a name, closing the race with name reuse). Untagged windows are never killed: `kill_window` queries `show-options -w -v @vowp`; absent/≠`1` ⇒ `Err(NotTagged)`.
4. **`kill_session`** refuses a session containing any untagged window (`Err(NotTagged)`); an empty/non-existent session is `Ok`.
5. **Command passing without injection.** Names (`session`, `name`, option values) are validated by `validate_name` (`[A-Za-z0-9_.-]+`, no leading `-`); they are passed as separate `std::process::Command` args, never through a shell. tmux's `new-window` takes the window command as **one shell-command string**; `argv` is therefore turned into `argv.iter().map(shell_quote).collect::<Vec<_>>().join(" ")` (each element single-quoted, `'` → `'\''`), and `env` entries become leading `env 'K=V' …` words (`K` validated `[A-Za-z_][A-Za-z0-9_]*`). Nothing else is ever interpolated. `argv` empty ⇒ `Err(EmptyCommand)`. `cwd` is passed with `-c <path>` (must exist, else `Err(InvalidCwd)`).
6. **Window creation** uses `tmux new-window -d -P -F '#{window_id}' -t <session>: -n <name> -c <cwd> '<command>'` so it never steals focus. `new_window` on an existing name ⇒ `Err(WindowExists)`.
7. **`ensure_session`**: `tmux has-session -t =<name>`; if absent, `tmux new-session -d -s <name> -n scratch -- <sleep-free placeholder argv ["sh","-c","exec cat"]>` and tag the window. `scratch` exists only so the session survives; `ensure_layout` removes it after creating `board` (it is tagged, so the kill is allowed). When the session does not exist at all, `ensure_layout` instead calls `new-session -d -s <name> -n board <board cmd>` directly (no scratch).
8. **`ensure_layout(project, spec)`**: creates session `vowp-<project>` if missing with window `board` at index 0, then, in order, `supervisor`, `planner`, `keeper`, `inbox` — each only if missing (idempotent), each tagged. Job windows (`job-<id>`) are created later by `new_window`. `board` is the first window (lowest index); if the user's tmux `base-index` is 1 the order still holds and only the numbers shift.
9. **`capture_pane`** returns text only; callers must treat it as display (principle: logs drive decisions). `lines` clamped to 1..=2000.
10. **Errors from tmux** (`no server running`, `can't find window`) are normalised: `list_windows`/`window_exists` on a missing session return empty/false; others return `TmuxError::Command { args, status, stderr }` with `args` omitting env values.
11. All commands run with `Command::new(binary)` and `-L <socket>` first when a socket is set; a 10 s timeout is enforced by a watchdog thread (`wait_timeout` implemented with `try_wait` polling at 20 ms); timeout ⇒ `Err(Timeout)` and the child is killed.
12. `kill_server_for_tests` exists only to give tests a Drop guard; it returns `Err(Refused)` when `socket == None` so it can never kill the user's server.

## 4. Window naming

| Window | Name | Created by |
|---|---|---|
| 0 | `board` | `ensure_layout` |
| 1 | `supervisor` | `ensure_layout` |
| 2 | `planner` | `ensure_layout` |
| 3 | `keeper` | `ensure_layout` |
| — | `job-<job-id>` | core at dispatch (`window_name_for_job`) ; verifier windows are `job-<verify-job-id>` |
| last | `inbox` | `ensure_layout` (created with the others; later job windows are appended after it) |

Job ids are validated slugs (`[a-z0-9-]`), so `job-<id>` always passes `validate_name`; ids > 59 chars are rejected by `window_name_for_job` returning `Err(InvalidName)` in the manager call.

## 5. Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux binary not found or not runnable")]                                   NotInstalled,
    #[error("invalid {kind} name {value:?} (allowed: [A-Za-z0-9_.-], not starting with '-')")] InvalidName { kind: &'static str, value: String },
    #[error("session {0:?} is not a vowp- session")]                                    ForeignSession(String),
    #[error("window {session}:{name} is not tagged @vowp=1; refusing")]                 NotTagged { session: String, name: String },
    #[error("window {session}:{name} already exists")]                                  WindowExists { session: String, name: String },
    #[error("window {session}:{name} not found")]                                       WindowNotFound { session: String, name: String },
    #[error("empty command")]                                                           EmptyCommand,
    #[error("working directory does not exist: {0}")]                                   InvalidCwd(String),
    #[error("refused: {0}")]                                                            Refused(&'static str),
    #[error("tmux timed out after {0:?}")]                                              Timeout(std::time::Duration),
    #[error("tmux {args:?} failed with {status:?}: {stderr}")]                          Command { args: Vec<String>, status: Option<i32>, stderr: String },
    #[error("io: {0}")]                                                                 Io(#[from] std::io::Error),
}
```
(`ErrorKind` mapping is done by `vowp-core`, since this crate does not depend on `vowp-protocol`.)

## 6. Test harness

```rust
/// tests/common/mod.rs
struct PrivateTmux { tmux: Tmux, socket: String }       // socket = format!("vowp-test-{}", random 8 hex from std::time nanos ^ pid)
impl PrivateTmux { fn new() -> Option<Self> /* None + eprintln!("SKIP: tmux not installed") when !has_tmux() */ }
impl Drop for PrivateTmux { fn drop(&mut self) { let _ = self.tmux.kill_server_for_tests(); } }
```
Tests never use the default server, never `TMUX` env of the user (the test sets `TMUX` unset for child processes with `Command::env_remove("TMUX")`). Window commands in tests are `["sleep","30"]` or `["sh","-c","echo hello; sleep 30"]` and are cleaned by the guard.

## 7. Named tests

Unit (`quote.rs`, no tmux)
- `shell_quote_plain_and_empty` — `a b` → `'a b'`, `` → `''`.
- `shell_quote_embedded_single_quote` — `it's` → `'it'\''s'`; `$(rm -rf /)`/`;`/backticks/newlines round-trip through `sh -c 'printf %s <quoted>'` and print the original string exactly (this test spawns `sh`, no tmux).
- `validate_name_rules` — accepts `job-graph.engine_1`, rejects empty, `-x`, `a b`, `a/b`, `a:b`, `a;b`, 65 chars.
- `window_name_for_job_prefix` / `session_name_for_project_prefix`.
- `command_string_builds_quoted_argv_with_env` — internal fn producing the tmux command string for `["echo","a b","c'd"]` + `env K=V` yields the exact expected string.

Private-socket (`tests/tmux_private_socket.rs`; skip-with-notice only if tmux is missing)
- `ensure_session_creates_and_is_idempotent` — session exists; second call OK.
- `ensure_layout_creates_fixed_windows_in_order` — names in order `board,supervisor,planner,keeper,inbox`; board at the lowest index; all `tagged`; second call changes nothing.
- `new_window_tags_and_runs_command` — window appears, `tagged=true`, `pane_pid` is Some; `capture_pane` eventually contains `hello` (polled ≤ 2 s).
- `new_window_rejects_duplicate_and_empty_argv` — `WindowExists`, `EmptyCommand`.
- `new_window_passes_special_characters_verbatim` — argv `["sh","-c","printf '%s' \"$1\" > OUT","x","a b'c;$(x)"]` with a temp cwd; file `OUT` contains exactly `a b'c;$(x)`.
- `kill_window_kills_only_the_named_window` — two job windows; kill one; the other survives (`window_exists`) and its process is alive.
- `kill_window_refuses_untagged_window` — an untagged window is created with a **raw** `tmux -L <sock> new-window` call in the same session; `kill_window` returns `Err(NotTagged)` and the window still exists.
- `kill_window_refuses_foreign_session` — a session named `other` (created raw) yields `ForeignSession`; the session is untouched.
- `kill_session_refuses_when_untagged_window_present` / `kill_session_kills_tagged_only_session`.
- `list_windows_missing_session_is_empty`.
- `window_name_reuse_after_kill` — kill then `new_window` with the same name works and the new window is tagged (recycle path; proves no respawn is needed).
- `kill_server_for_tests_refuses_default_socket` — `Tmux::new(None).kill_server_for_tests()` → `Refused` **without spawning tmux** (use `with_binary(None, "/nonexistent")` so a bug would fail loudly with `NotInstalled` instead).
- `attach_argv_includes_socket` — exact vector with and without socket.

Safety (`tests/safety_grep.rs`)
- `source_has_no_respawn_or_send_keys` — reads every file under `src/` (via `env!("CARGO_MANIFEST_DIR")`), and for each line that is **not** a doc/line comment (after trimming, not starting with `//`) asserts it contains none of the substrings `respawn-pane`, `respawn-window`, `send-keys`, `send_keys`, `respawn_pane`. The test file itself is under `tests/` and therefore not scanned. (Comments may mention them to explain the ban.)

Fake (`src/fake.rs` unit tests)
- `fake_enforces_same_rules` — `ForeignSession`, `NotTagged` for `untagged_window`, `WindowExists`, `InvalidName`, tag always set, `killed()` records.

## 8. Out of scope (v0.2)
Layout presets, pane splits, mouse/status-line theming, web terminal windows, remote tmux over ssh inside this crate (the CLI runs `ssh -t host tmux attach …` itself).
