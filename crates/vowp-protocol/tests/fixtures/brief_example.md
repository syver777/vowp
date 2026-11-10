---
job: sample-job
owns: ["crates/vowp-graph/**"]
must_not_touch: ["everything else"]
needs_done: [sample-dependency]
gates: [report, "cargo test -p vowp-graph", owns_respected, review]
done_when: "report has status: done and every gate passes"
---
# sample-job

Implement the DAG and the state machine.
