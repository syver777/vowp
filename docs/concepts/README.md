# Concepts

These pages explain how VOWP is put together and why. They describe the v0.2 home model and the
implementation on `main`; every command shown is implemented. The binding specification is in
[`docs/design/v0.2/home-and-surfaces.md`](../design/v0.2/home-and-surfaces.md).

| Page | Read it for |
|---|---|
| [Overview](overview.md) | The vocabulary, the home and control machine, the components and where state lives. |
| [Home and surfaces](home-and-surfaces.md) | One supervisor for many projects, leases, takeover, remote execution, app hosts, orchestrators and MCP. |
| [Capacity](capacity.md) | Measured host and run usage, admission, brakes, targets and the advisor. |
| [Data lifecycle](data-lifecycle.md) | Classes, garbage collection, closed-session compaction, backup, restore and migration. |
| [Graph and loops](graph-and-loops.md) | Jobs, edges, states, gates, L1–L9 and the merge queue. |
| [Sessions and switching](sessions-and-switching.md) | Event logs, capsules, surfaces, executors and handoff. |
| [Providers and failover](providers-and-failover.md) | Profile ladders, shared provider limit rows and cost receipts. |
| [Work → Production](work-to-production.md) | Guarded, reversible promotion. |

If you want to run something rather than understand it, start with the [guides](../guides/README.md).
