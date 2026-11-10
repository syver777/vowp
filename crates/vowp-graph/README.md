# vowp-graph

Deterministic job graph, state machine, ownership checks and dispatch planning for
[VOWP](https://github.com/syver777/vowp), a VPS-first supervisor for AI coding agents. This crate is part
of the VOWP workspace.

## What it provides

- The job DAG and dependency planning.
- The job state machine with bounded attempts, hang restarts and explicit effects.
- Ownership-overlap checks for the paths a job declares.
- Dispatch planning: which ready jobs may start, and in what priority order.
- ASCII and Mermaid rendering, plus YAML import and export.

The crate is pure: no I/O, no clock reads and no threads. `vowp-core` loads rows from `vowp-store`, feeds
them to the graph, and applies the effects the graph returns.

## Status

VOWP 0.1.0 is an early release. The graph and state-machine API is used by the shipped `vowp` CLI, but it is
not yet stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-graph)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
