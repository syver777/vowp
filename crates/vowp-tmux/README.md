# vowp-tmux

Safe tmux window management for [VOWP](https://github.com/syver777/vowp) job sessions. This crate is part of
the VOWP workspace.

## What it provides

- Creation and lookup of VOWP's fixed windows (board, supervisor, planner, keeper, inbox) and per-job
  windows, always in the `vowp-` session namespace.
- A narrow command surface: VOWP attaches viewers and follows logs; it never respawns processes or sends
  keystrokes through tmux.
- Per-home socket names and shell quoting helpers.
- An in-memory fake window manager for tests.

The crate refuses to kill windows it did not create. It is the only crate in the workspace that runs the
`tmux` binary.

## Status

VOWP 0.2.0 is an early release. The window-manager API is used by the shipped `vowp` CLI, but it is not yet
stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-tmux)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
