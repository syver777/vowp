# Concepts

These pages explain how VOWP is put together and why. They describe the v0.1 design and the
implementation on `main`; every command shown is implemented. The binding specification is in
[`docs/design/`](../design/README.md).

| Page | Read it for |
|---|---|
| [Overview](overview.md) | The vocabulary, the components and where state lives. |
| [Graph and loops](graph-and-loops.md) | Jobs, edges, states, gates, L1–L9 and the merge queue. |
| [Sessions and switching](sessions-and-switching.md) | Event logs, capsules, executors and handoff. |
| [Providers and failover](providers-and-failover.md) | Profile ladders, limits and cost receipts. |
| [Work → Production](work-to-production.md) | Guarded, reversible promotion. |

If you want to run something rather than understand it, start with the [guides](../guides/README.md).
