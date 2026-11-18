# vowp-agent

The Mac-side local agent for [VOWP](https://github.com/syver777/vowp), a local-first supervisor for AI
coding agents. This crate is part of the VOWP workspace.

## What it provides

- Local agent configuration and the ed25519 node key, with private file permissions.
- The SSH stdio and TLS WebSocket transports for `vowp-protocol` agent frames.
- Surface pairing, local surface state and the authenticated local agent socket.
- The run-assignment executor with its spool and result bundles, and the chat lease loop.
- The opencode SSE bridge, the local orchestrator host and the bounded offline read cache.
- The launchd/systemd-user service files and the worktree path map.

The crate depends on `vowp-protocol` only. It never opens a store or a database and never depends on
`vowp-core`; it dials a home only through the injected transport.

## Status

VOWP 0.4.0 is an early release. The agent client and the local orchestrator are new in 0.4.0 and their
interfaces are not yet stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-agent)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
