# Contributing to VOWP

VOWP is an early-release, MIT-licensed Rust workspace. Contributions are welcome: bug reports,
documentation fixes, tests and focused code changes. This page covers setup, the checks CI runs and how
to get a change reviewed.

## Set up

1. Install [Rust with rustup](https://rustup.rs/) if you do not have it. The workspace pins its
   toolchain in [`rust-toolchain.toml`](rust-toolchain.toml); `rustup show` prints the active one.
2. Install `git` and `tmux` with your operating system's package manager. The build, the merge queue
   and the tests use git; the supervisor and its tests use tmux.
3. Optionally install a harness such as [Claude Code](https://code.claude.com/docs/en/overview) or
   [opencode](https://opencode.ai/docs/), signed in with its own tooling, if you want to run the
   supervisor end to end.
4. Fork the repository on GitHub, clone your fork, and create a topic branch for your change.

Check the basics:

```sh
git --version
cargo --version
rustc --version
rustup show
```

VOWP does not need a provider key of its own and never reads a harness login. If a harness needs an
environment variable, set it in your shell exactly as you already do for that harness; `vowp doctor`
reports only whether the **name** is present.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo run -p vowp-cli --bin vowp -- --version
```

The last command prints `vowp <version>`, with `<version>` taken from the workspace `Cargo.toml`.

## Checks before a pull request

These are the checks [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on Linux and macOS:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

If you change documentation, also run the offline documentation checks:

```sh
bash docs/tests/run.sh
```

Tests must be real and hermetic: no network, no real home directory or `~/.vowp`, no user tmux server
and no real harness binaries. A test that starts tmux must use a private socket (`tmux -L <unique>`)
**and** a private `TMUX_TMPDIR`, created before the first tmux command. Never use the default tmux
server.

## Propose a change

1. Open an issue first for a bug report or a design change, so the approach can be discussed before you
   spend time on it. Small fixes can go straight to a pull request.
2. Keep the pull request scoped to one problem: read anything, but change only the files the change
   needs.
3. Include a test that fails without the change and passes with it. Update the documentation and
   `--help` text when behaviour changes.
4. Make sure the checks above pass. A red build is not ready for review.
5. A maintainer reviews the change and merges it after the checks pass. Do not push to `main`
   directly; push follow-up commits to your topic branch.
6. Do not report a vulnerability in a public issue or pull request. Follow [`SECURITY.md`](SECURITY.md).

Keep commits focused, with a `crate: what changed` subject and a body that explains why when the reason
is not obvious. If a commit is AI-assisted, disclose it with `AI-Assisted: <model> via VOWP`.

## Code of conduct

All project spaces follow [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). Report unacceptable behavior
privately to the repository owner through GitHub.
