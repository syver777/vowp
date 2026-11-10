# hello-message

## Goal

Create `src/message.txt` with two lines, copied from the files the other two jobs produced:

```text
Hello from VOWP
Goodbye from VOWP
```

This job waits for both `hello-greeting` and `hello-farewell`, so it also demonstrates a
dependency edge in the graph.

## Gates

- `report` — the fake worker's report has `status: done`.
- `owns_respected` — only `src/message.txt` may change.
- `grep -qx 'Hello from VOWP' src/message.txt && grep -qx 'Goodbye from VOWP' src/message.txt`
  — checks both lines of the committed content.

## Done when

The report has `status: done`, the content gate passes, and the job merges into `main`.
