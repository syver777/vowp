---
name: vowp-worker
description: Use when you are the worker on a VOWP job, from reading the brief through committing the change and writing the report.
---

# VOWP worker

You own exactly one job. The brief is the contract; the graph node is the index.

## Start

1. Read the job's brief, its graph node, and the project `AGENTS.md`.
2. Read the code and tests in the paths you own before editing.
3. Run the existing tests once to learn the baseline.
4. Restate the goal and the gates in your first working note.

## Execute

- Change only the paths under `owns`. If the work needs another path, stop and report the missing ownership;
  do not widen the job yourself.
- Make the smallest coherent change that satisfies the brief. Keep unrelated refactors out.
- Add or update tests with the change. A test must fail when the behavior is wrong.
- Run the project's formatter, linter and test suite, then every gate in the brief. Read the output; do not
  trust exit codes alone when a command can fail silently.
- Keep tests hermetic: temporary directories, a private socket, recorded fixtures and fake harness scripts.
  Never use the real home directory, `~/.vowp`, the user's tmux server, the network, or a real agent binary
  unless the brief explicitly allows it.

## Commit

- Commit small and focused on the job branch. Do not amend or rewrite shared history.
- Never push. The supervisor's merge queue is the only writer of the main branch.
- End every commit message with `AI-Assisted: <model> via VOWP`.

## Report

Write the report at the path in the graph node before you stop. Use the report template front-matter and
include:

- `status`: `done`, `partial` or `blocked` — honestly.
- `branch`, `commits`, `files_changed`, and the exact test commands with real counts.
- What changed, what you verified, what you could not verify, and why.
- Real cost from receipts, or `unknown` / an estimate labelled as such.
- Any question for the owner, with `status: blocked` and the question repeated under `questions_for_owner`.

Do not stop after announcing a next step. Continue until the report exists, every gate has been run, and
the result is recorded. If a refused or risky action blocks the job, stage it and report it; never work
around it.
