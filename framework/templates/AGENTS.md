# AGENTS.md — rules for agents in a VOWP-managed repository

This repository is run by VOWP. A job is one unit of work with a brief, owned paths, gates and a report.
These rules apply to every agent and every harness. A job's brief can add rules; it never removes these.

## The job contract

- Read your brief and the job's graph node before you change anything. If they disagree, stop and report the
  conflict; do not guess.
- Change only the paths listed under `owns`. Read anything; write nothing else. A lock file may be updated
  only by the tool that owns it, never by hand.
- Work autonomously until your report exists. Do not stop after announcing a next step, and do not wait in
  chat for permission that the brief already gives you.
- Verify against real artifacts, not claims: run the gates yourself, read the output, and put the exact
  commands and results in the report. "It should work" is not evidence.
- A refused or risky action is staged, never worked around. Record what was refused, why, and what a human
  must decide.

## Git

- Commit in your own worktree on the job branch. Small, focused commits with a clear subject.
- Never push. The supervisor's merge queue is the only writer of the main branch.
- Never rewrite shared history, force-push, or delete a branch you do not own.
- End every commit message with the trailer `AI-Assisted: <model> via VOWP`.

## Secrets

- Never read, print, log, commit or paste secret values: API keys, tokens, passwords, private keys, `.env`
  files, credentials. Refer to a secret by its **name** only.
- Do not copy secrets into a report, a test fixture, a command line, or an error message.
- If a task seems to need a secret value, stop and ask the owner in the report.

## Tests and evidence

- Tests must be real and hermetic. A test must fail if the code is wrong; no hard-coded passes and no
  `todo!()`/`unimplemented!()` presented as a feature.
- Tests never touch the real home directory, the user's services, the network, or real agent binaries unless
  the brief explicitly allows it. Use temporary directories, private sockets and fake harness scripts.
- Run every gate in the brief, plus the project's formatter, linter and test suite when they exist. Report
  the exact command, exit code and a short result.

## Costs

- Report real costs from the harness's own receipts (usage and cost fields). If no receipt exists, label the
  number as an estimate or `unknown`; never assume a call was free.
- Do not invent budgets or switch to a paid provider that the job's profile does not list.

## Public actions and safety

- Nothing public without the owner's explicit yes: no publishing, deploys, repository visibility changes,
  releases, new accounts or keys.
- No `sudo`, no changes outside the project's worktree, and no touching other users' files or services.
- Prefer the project's existing tools and dependencies. Do not install system packages or add dependencies
  outside the crate or module you own.

## Asking the owner

- A question for the owner goes in the report front-matter under `questions_for_owner`, with the job status
  set to `blocked`. Do not sit idle waiting for a chat reply.
- Only the blocked job waits; keep unrelated work moving. The owner answers through the VOWP inbox.

## Reports

- Write the report at the path named in the brief, using the report template's front-matter.
- State what you changed, what you ran, what passed, what failed, what is left, and the exact commit ids.
- If you could not finish, say so plainly with `status: partial` or `status: blocked`; never mark unfinished
  work `done`.
