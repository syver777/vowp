# hello-vowp — a three-job VOWP demo

This example runs a complete VOWP graph in about a minute, without a model, provider
account, or real harness. It uses the generic `cli` adapter with a tiny fake worker script;
the supervisor, sessions, gates, git worktrees, dependency edges and merge queue are the
real ones. The script itself makes no network calls; a first `cargo run` build may fetch
crates as usual.

The graph has three jobs:

```
hello-greeting  ─┐
                 ├─> hello-message
hello-farewell  ─┘
```

Each job writes one file under `src/`, passes a `report` gate, an `owns_respected` gate and
a content-checking command gate, and is merged into `main` by the merge queue.

## Prerequisites

- Rust (the toolchain from the repository's `rust-toolchain.toml`) and `git`.
- `tmux` is **not** needed: the script starts the supervisor with `--no-tmux`. The
  supervisor may still attempt to recreate a window layout; without a tmux server that
  attempt warns and is harmless. The script points `tmux` at a private socket directory
  under the temporary work directory and kills that server at the end, so it never
  contacts the caller's tmux server.

## Run it

From the repository root:

```sh
FAKE=1 examples/hello-vowp/run.sh
```

`run.sh`:

1. creates a temporary directory with a bare `origin` and a demo checkout;
2. creates a temporary VOWP home and runs `vowp init` there;
3. adds the three jobs with `vowp job add`;
4. starts `vowp up --detach --no-tmux` with a fake `cli` adapter;
5. polls until `hello-greeting`, `hello-farewell` and `hello-message` are all `done`;
6. runs `vowp down` and checks the merged files in the checkout and on `origin/main`.

Job worktrees live under `<project>/.vowp/worktrees`: `run.sh` fills the
`worktree_root` placeholder in `vowp.toml` with that absolute path, inside the
checkout, so the default cwd allow-list admits them without a `[node].cwd_allow`
entry.

It exits 0 only when all three jobs reached `done` and the merged content is correct. It
never touches `~/.vowp`, the real tmux server, or a real `claude`/`opencode` binary. A
temporary directory created by the script is removed on success.

Useful variables:

| Variable | Meaning |
|---|---|
| `VOWP=/path/to/vowp` | use a specific `vowp` binary (otherwise an installed/built binary, or `cargo run`) |
| `TIMEOUT=300` | seconds to wait for the jobs (default 180) |
| `KEEP=1` | keep a script-created temporary directory for inspection (ignored when `WORK` is set) |
| `WORK=/some/dir` | use an explicit work directory; it must be empty (or not exist), is never deleted, and keeps its contents for inspection |

`WORK` may be relative; `run.sh` creates it and then resolves it to an absolute path before
any git or VOWP command. Spaces in the path are supported.

Example with a kept work directory:

```sh
KEEP=1 FAKE=1 examples/hello-vowp/run.sh
```

The script prints the work directory, VOWP home and project paths. To inspect the result,
run from inside the printed project checkout, using the same binary you ran the demo with
(`$VOWP`, an installed `vowp`, or `target/debug/vowp` from this repository):

```sh
cd /tmp/hello-vowp.XXXX/hello-vowp
/path/to/vowp --home ../home --project . status
/path/to/vowp --home ../home --project . board --once
```

Run from the checkout because the generic `cli` adapter resolves its mapping file relative
to the process working directory when `vowp.toml` has no `[project].path`.

## Test `run.sh`

From the repository root:

```sh
bash examples/hello-vowp/tests/test_run.sh
```

The test runs the example three times in temporary directories: with the default work
directory, with a relative `WORK=relwork`, and with a `WORK` path containing a space. Each
run uses `FAKE=1`, a private `HOME` and a private tmux socket, and asserts that all three
jobs reach `done`. Set `KEEP=1` to keep the test root, or `TIMEOUT=300` to give a slow first
build more time.

## Files

| Path | Purpose |
|---|---|
| `run.sh` | the runnable acceptance script |
| `vowp.toml` | fake-adapter project config copied into the temporary checkout |
| `mapping.toml` | generic `cli` adapter mapping for the fake worker |
| `fake-worker.sh` | the fake harness: writes the job file, commits it, writes the report |
| `briefs/*.md` | brief bodies for the three jobs |
| `project/` | the initial demo repository contents |
| `tests/test_run.sh` | default, relative and spaced `WORK` scenarios |

The fake worker is deliberately small: it reads the report path from the L2 prompt, writes
the file named for the job, commits it, and emits a report with `status: done`. The gates
and merge queue below it are not faked.

## Why fake adapters?

This example is for checking the wiring and watching the lifecycle without spending model
tokens or configuring a harness. For a real run, replace the `[profiles.*]` ladders and the
`[harnesses.fake-worker]` entry with a real `claude-code`, `opencode`, or `cli` mapping and
keep the same graph and gates.
