# vowp-core

The supervisor core of [VOWP](https://github.com/syver777/vowp), a VPS-first supervisor for AI coding
agents. This crate is part of the VOWP workspace.

## What it provides

- The session service: append-only event logs, handoff capsules, session switching between harnesses,
  models and nodes, and session bundle export/import.
- Gates: command, report, review, deploy and secret-scan checks with recorded evidence.
- The cost ledger and cron/interval schedules.
- The deterministic supervisor loops: the L1 tick (reconcile, dispatch, gates and merge queue), worker
  prompts, independent verify, planner, keeper, limit failover/probe-back, inbox answers, watchdog and
  schedules.
- Guarded Work -> Production promotion and rollback.

The crate depends on `vowp-protocol`, `vowp-store`, `vowp-graph`, `vowp-tmux` and `vowp-adapters`. It
contains the rules; the `vowp` binary wires them to the command line.

## Status

VOWP 0.1.0 is an early release. Some supervisor loops are merged but not yet complete against the design
spec; the release notes list the known gaps. The Rust API is not yet stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-core)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
