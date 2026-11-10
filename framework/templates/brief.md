---
job: example-job
owns: ["src/example/**"]
must_not_touch: ["everything else"]
needs_done: []
gates: [report, "make test", owns_respected, review]
done_when: "report has status: done and every gate passes"
---

# Goal

One paragraph. State the outcome, why it matters, and what "working" means for the user.

# Read first

- The job's graph node and any design or interface document it names.
- The existing code and tests in the paths you own.
- The project's `AGENTS.md`, plus any nested `AGENTS.md` near those paths.

# Deliverables

- List the files, behavior and documentation this job must produce.
- Name the public interfaces and error behavior that must not change.
- Say what must not be left as a stub, a placeholder or a fake passing test.

# Tests required

- List the exact commands that prove the work, including the project's formatter, linter and test suite.
- Describe at least one test that fails without the change and passes with it.
- State any allowed exceptions to hermetic testing explicitly; otherwise tests use temporary directories and
  fake services, never the real home directory, network or user services.

# Out of scope

- List adjacent work that belongs to another job or another owner.
- Name paths this job must not touch, beyond the default "everything else".

# Done when

- Every gate in the front-matter passes and its evidence is in the report.
- The report exists at the path in the graph node, with real commands, results and commit ids.
- `status: done` is honest: nothing in the deliverables is missing or silently stubbed.
