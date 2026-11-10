---
job: sample-job
status: done
branch: job/sample-job
commits: [abc1234, def5678]
tests:
  command: cargo test -p vowp-graph
  passed: 31
  failed: 0
files_changed:
  - crates/vowp-graph/src/lib.rs
open_items: []
questions_for_owner: []
---

# sample-job — report

`vowp-graph` is implemented and green.

## What was built

The DAG and the state machine.
