# hello-farewell

## Goal

Create `src/farewell.txt` containing exactly one line:

```text
Goodbye from VOWP
```

This job does not depend on `hello-greeting`; the two run in parallel while the supervisor
has free worker slots.

## Gates

- `report` — the fake worker's report has `status: done`.
- `owns_respected` — only `src/farewell.txt` may change.
- `grep -qx 'Goodbye from VOWP' src/farewell.txt` — checks the committed content.

## Done when

The report has `status: done`, the content gate passes, and the job merges into `main`.
