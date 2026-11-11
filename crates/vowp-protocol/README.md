# vowp-protocol

Shared data types and traits for [VOWP](https://github.com/syver777/vowp), a local-first supervisor for AI
coding agents. This crate is part of the VOWP workspace and is the foundation the other `vowp-*` crates are
built on.

## What it provides

- The wire and storage types for jobs, events, sessions, capsules, gates, reports, costs, inbox items,
  releases and configuration.
- Adapter and service traits, identifiers, time helpers and the `Clock` abstraction.
- Serde support for the JSON, JSONL and TOML representations used by the supervisor.
- JSON Schema export for the data that crosses a process or node boundary.

The crate contains no I/O of its own; it defines the vocabulary and validation rules that the store, graph,
adapters, core and CLI use.

## Status

VOWP 0.2.0 is an early release. The types in this crate back the shipped `vowp` CLI, but the API is not yet
stable and may change in a minor release while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-protocol)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
