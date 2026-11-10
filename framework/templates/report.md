---
job: example-job
status: partial            # done | partial | blocked
branch: job/example-job
commits: [<commit-sha>]
tests: { command: "<test command>", passed: 0, failed: 0 }
files_changed: [<path>]
open_items: []
questions_for_owner: []
---

# Summary

What was asked, what was built, and whether it works. Keep this short enough to read first.

# What changed

- Files and behavior changed, with the reason for each.
- Public interfaces, migrations or configuration that a reviewer must notice.
- Anything intentionally left out and why.

# Verification

- Exact commands run, exit codes, and the relevant output or counts.
- Gates that passed and gates that were skipped, with the reason for each skip.
- Evidence from real artifacts: test output, diffs, logs or built files, not claims.

# Costs

- Real cost from receipts, by model or provider, when the harness reports it.
- Otherwise `unknown` or an estimate labelled as an estimate. Never assume a call was free.

# Open items

- Known limitations, follow-up work, or risks that remain.
- Use `[]` when there are none.

# Questions for the owner

- One question per line, each with the decision you need and the default you will take if unanswered.
- When a question blocks the job, set `status: blocked` and list it in `questions_for_owner` above too.
