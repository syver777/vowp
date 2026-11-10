# hello-greeting

## Goal

Create `src/greeting.txt` containing exactly one line:

```text
Hello from VOWP
```

Commit the file on the job branch. The fake adapter writes the file, commits it, and writes
the report; the supervisor runs the job's command gate and the merge queue for real.

## Gates

- `report` — the fake worker's report has `status: done`.
- `owns_respected` — only `src/greeting.txt` may change.
- `grep -qx 'Hello from VOWP' src/greeting.txt` — checks the committed content.

## Done when

The report has `status: done`, the content gate passes, and the job merges into `main`.
