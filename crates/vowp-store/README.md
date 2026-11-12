# vowp-store

SQLite storage, JSONL session logs and content-addressed blobs for
[VOWP](https://github.com/syver777/vowp), a local-first supervisor for AI coding agents. This crate is part
of the VOWP workspace.

## What it provides

- The split SQLite schema (one global database plus one database per project), the v0.1→v0.2
  migration, cross-file reconcile and typed repository access for jobs, runs, sessions, loops,
  nodes, the cost ledger and miscellaneous state.
- The append-only JSONL session log and session file layout under a node's home directory.
- Content-addressed blob storage, `reindex` recovery and consistent backups.
- Path helpers and typed errors for the storage layer.

The crate owns persistence only. It does not decide job-state transitions, run loops or talk to harnesses;
those rules live in `vowp-graph`, `vowp-core` and `vowp-adapters`.

## Status

VOWP 0.3.0 is an early release. The schema is versioned and migrated forward, but the Rust API is not yet
stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-store)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
