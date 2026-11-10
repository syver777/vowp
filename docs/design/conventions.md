# conventions.md — Rust conventions, dependencies, tests, naming

Binding for every contribution. It adds the details a contributor needs to the repository's contribution rules.

## 1. Language and tooling

- Stable Rust, edition 2024, toolchain from `rust-toolchain.toml`. No nightly features, **no `unsafe`**
  (`#![forbid(unsafe_code)]` in every crate's `lib.rs`/`main.rs`).
- Before every commit: `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`.
- Plain threads and blocking I/O (`std::thread`, `std::sync::mpsc`, `std::process`). **No `tokio` in v0.1** (no crate needs it:
  every harness is a child process with a stdout pipe; one reader thread per pipe). `axum`/`tokio` arrive with the v0.2 daemon.
- Libraries return typed errors (`thiserror`); `anyhow` only in the `vowp` binary (`vowp-cli`).
- No `unwrap()`/`expect()` in library code outside `#[cfg(test)]`, except where the invariant is stated in a comment on
  that line (`// invariant: regex literal is valid`). Prefer `?`.
- Public items have doc comments (`#![warn(missing_docs)]` in libs; CI clippy runs with `-D warnings`).
- Modules small: aim for < 400 lines per file; split by concept. Names plain: `Store`, `SessionLog`, `Supervisor`; no `Manager`/`Helper`/`Util`.
- Time always goes through `vowp_protocol::Clock`; never call `Timestamp::now()` inside loop/gate/schedule logic (only in `SystemClock`).
- Ids always from `vowp_protocol::ids`; never hand-format ULIDs.

## 2. Workspace layout and dependency direction

```
vowp-protocol  ←  vowp-store, vowp-graph, vowp-tmux, vowp-adapters
                         ↖     ↑          ↑          ↗
                           vowp-core   (store, graph, tmux, adapters, protocol)
                                ↑
                           vowp-cli   (core + all)
```

`vowp-graph` depends only on `vowp-protocol`; `vowp-tmux` on nothing (plain `std` + `thiserror`). `vowp-store` depends on
`vowp-protocol` only. `vowp-adapters` on `vowp-protocol` only. Cycles are forbidden.

## 3. Dependencies per crate (add with `cargo add -p <crate> <dep>`; features as listed)

| Crate | Normal deps | Dev deps |
|---|---|---|
| `vowp-protocol` | `serde` (derive), `serde_json`, `thiserror`, `ulid`, `time` (formatting, parsing, macros), `schemars` (derive, preserve_order off), `toml`, `serde_yaml_ng` (front-matter YAML), `regex` (gate pattern validation), `globset` (validate owns globs) | `proptest`, `pretty_assertions` |
| `vowp-store` | `vowp-protocol`, `rusqlite` (features `bundled`), `serde_json`, `thiserror`, `sha2`, `hex` | `tempfile`, `proptest` |
| `vowp-graph` | `vowp-protocol`, `globset`, `thiserror`, `serde_yaml_ng` (graph export) | `proptest` |
| `vowp-tmux` | `thiserror` | `tempfile` |
| `vowp-adapters` | `vowp-protocol`, `serde`, `serde_json`, `thiserror`, `toml` (cli mapping), `regex`; resolve binaries by scanning `PATH` yourself (≈10 lines), no `which` crate | `tempfile`, `pretty_assertions` |
| `vowp-core` | `vowp-protocol`, `vowp-store`, `vowp-graph`, `vowp-tmux`, `vowp-adapters`, `serde_json`, `thiserror`, `cron`, `globset`, `sha2`, `regex`, `time` | `tempfile`, `proptest` |
| `vowp-cli` | all workspace crates, `clap` (derive), `anyhow`, `serde_json`, `time`, `signal-hook` (safe signal flags for SIGTERM/SIGINT) | `tempfile`, `assert_cmd` (optional) |

Rules: versions are whatever `cargo add` resolves (real and current); never write a version by hand. Prefer deps already in
the workspace; if a needed dep is missing from `[workspace.dependencies]`, add it to your own crate only.
`serde_yaml_ng` is the maintained fork of the deprecated `serde_yaml`; if `cargo add` cannot resolve it, use `serde_norway`
and record that in the report (same API surface; only `from_str`/`to_string` are used).

## 4. Errors and logging

- One `thiserror` enum per crate: `ProtocolError`, `StoreError`, `GraphError`, `TmuxError`, `AdapterError`, `CoreError`
  (and a `CliError` mapping to exit codes in `vowp-cli`). Each has `fn kind(&self) -> ErrorKind` (protocol vocabulary).
- Errors carry context (path, id, command) in fields, not pre-formatted strings, so `--json` output can include them.
- Libraries never print. Logging goes through the tiny `Log` trait in `vowp-protocol` (`fn log(&self, level: Level, target: &str, msg: &str)`)
  injected into `Supervisor`; the CLI's implementation writes `~/.vowp/logs/supervisor.log` lines
  `2026-11-08T19:25:47.123Z INFO l1 job graph-engine: running -> gating`. Never log prompts, env values or secret-looking strings.
- Panics are bugs: loops run each tick under `std::panic::catch_unwind` in `vowp-core` only, logging and counting `fail_streak`.

## 5. Tests are real and hermetic

- Every crate's test names are listed in its `crates/<crate>.md`; implement **those names** (extra tests welcome).
- Never touch the real HOME, `~/.vowp`, the network, the user's tmux server, or real `claude`/`opencode`. Use:
  - `tempfile::tempdir()` for every filesystem path; pass the root explicitly (`NodePaths::new(tmp.path())`), never read `$HOME` in a library.
  - **private tmux socket**: `tmux -L vowp-test-<ulid>`; the test guard kills that server on drop. Skip (not fail) with a
    printed notice only if `tmux` is not installed — the build container has it, so CI must not skip.
  - **fake harness scripts**: `crates/vowp-adapters/tests/fakes/*.sh` (POSIX sh) printing recorded fixture lines; see `adapters.md` §9.
  - **real git** in temp repos (bare `origin` + clone) for merge-queue/owns/pushed tests; `git` is present in the build container.
  - recorded fixtures under `crates/<crate>/tests/fixtures/` (JSON/JSONL files, committed, sanitized: no secrets, no real paths beyond `/home/user/project`).
- A test must fail if the code is wrong: assert on concrete values, not just `is_ok()`. No `assert!(true)`, no `#[ignore]` to hide failures,
  no `todo!()`/`unimplemented!()`.
- Time-dependent logic takes a `Clock`; tests use `FakeClock`. Sleeping tests are banned except where a real child process is needed
  (then ≤ 2 s, with a generous timeout, and polled not slept).
- Property tests (`proptest`) where the spec says so (event seq, graph, glob overlap, cron next-run). Cases ≤ 256, deterministic seeds off by default.
- Test file layout: unit tests in `#[cfg(test)] mod tests` next to the code; integration tests in `crates/<crate>/tests/*.rs`.

## 6. Naming

| Thing | Convention | Examples |
|---|---|---|
| Crates | `vowp-<name>`; Rust ident `vowp_<name>` | `vowp_store` |
| Types | `UpperCamel`; enums singular | `JobState`, `EdgeKind` |
| Fns / modules | `snake_case`; getters without `get_` | `store.job(&id)` |
| DB tables / columns | `snake_case`, plural tables | `gate_results.gate_index` |
| Enum JSON | `snake_case` strings; event types dotted (`tool.call`); executor strings `harness:model@effort` | |
| Constructors | `new` (infallible), `try_new`/`parse` (fallible), `open` (I/O) | |
| Store getters | `x(&id) -> Result<Option<X>>` (absent = `None`), `require_x(&id) -> Result<X>` (absent = `NotFound`), `list_xs(filter)` | |
| Ids in paths | job id as given; sessions by ULID | `sessions/01KA…/log.jsonl` |
| Worktrees | `<worktree_root>/<job-id>`, branch `job/<job-id>` | |
| tmux | session `vowp-<project>`; windows `board supervisor planner keeper inbox job-<id>` | |

## 7. Documentation rules for code

Doc comments describe behaviour and invariants, not restate the name. Each module starts with `//!` explaining its job in ≤ 5 lines and
pointing to the spec section (`//! Spec: docs/design/crates/vowp-store.md §3`). Do not document features that do not exist; mark planned ones `// v0.2`.
