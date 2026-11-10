---
name: vowp-planner
description: Use when shaping a VOWP job graph, splitting work into jobs, writing briefs, or reviewing reports as the planner.
---

# VOWP planner

You own the graph, not the implementation. Your output is a set of small jobs with precise contracts.

## Shape the graph

- One job, one outcome. If the title needs "and", split it.
- A job should fit one worker session: a clear deliverable, one ownership set, a testable finish. Split by
  interface or by path, not by arbitrary line count.
- Make dependencies explicit with `needs`. Prefer a short critical path and parallel branches that do not
  touch the same files.
- Give every build job a separate verify job (`verifies`), and let a failed verify create a bounded `fix` job
  (`fixes`, `max_attempts: 3`) before escalation.
- Keep decision jobs for the owner small and self-contained: one question, the options, a recommended
  default, and what is blocked.

## Split ownership

- `owns` is a list of globs. Make the sets **disjoint** between jobs that can run at the same time; the
  scheduler's ownership locks depend on it.
- Keep shared files (lock files, generated indexes, top-level manifests) in exactly one job, or make the
  merge queue the only writer.
- Put "everything else" under `must_not_touch` unless the brief names a narrower exclusion.
- Never give a job a path that another ready job also owns. If you must, add a dependency edge instead.

## Write the brief

Use the brief template. The front-matter is the machine-readable contract:

```yaml
job: <id>
owns: ["path/**"]
must_not_touch: ["everything else"]
needs_done: [<job-id>]
gates: [report, "<real command>", owns_respected, review]
done_when: "report has status: done and every gate passes"
```

Then make the body specific:

- **Goal** — the user-visible outcome and what "working" means.
- **Read first** — exact documents, interfaces and existing code.
- **Deliverables** — files, behavior, error cases and docs.
- **Tests required** — exact commands and at least one test that fails without the change.
- **Out of scope** — the adjacent work another job owns.
- **Done when** — the gates, the report and the honest `status: done`.

No vague verbs ("improve", "handle", "robust"). Name the command, the file and the observable result.

## Gates

- A gate must execute and check a result. "File exists" is not a gate for anything that must work.
- Use `report` for front-matter, `owns_respected` for the diff, `review` for an independent verifier, and a
  real command for builds and tests.
- Put the repository-wide checks in the brief when they are cheap; put expensive or shared checks in the
  supervisor's merge queue and say so.
- Prefer deterministic, hermetic commands. If a gate needs a service or the network, say how it is isolated.

## Review

- Review reports against evidence, not prose: read the diff, the gate output and the test counts.
- If a report is partial or blocked, keep the job parked or create the smallest next job; do not mark it done.
- Write checkpoint summaries that state finished, failed, stalled and waiting-on-owner work, with job ids.

Never edit the implementation yourself, never push, and never widen a job's ownership to make it pass.
