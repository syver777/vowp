# vowp-cli

The command-line interface for [VOWP](https://github.com/syver777/vowp), a local-first supervisor for AI
coding agents. This crate is part of the VOWP workspace.

The crate is named `vowp-cli`; the binary it builds and installs is named `vowp`. There is no separate
`vowp` meta-crate: installing this package is how you get the command.

## Install

Once version 0.3.0 is on crates.io:

```sh
cargo install vowp-cli
vowp --version
```

From a checkout of the repository, install from the workspace root instead. The publishable
package manifest lives there so the `framework/` assets embedded with `include_str!` are inside the
package; the source is in `crates/vowp-cli/src`:

```sh
cargo install --path .
vowp --version
```

## What it provides

- The `vowp` binary and its command tree: project and node setup, jobs and graph, sessions, the supervisor,
  inbox, costs, loops, schedules, backups, schema export and Work -> Production promotion.
- A small library (`vowp_cli::run`) so command behaviour is tested in-process.

## Status

VOWP 0.3.0 is an early release. The `vowp` command tree is implemented, but some supervisor loops and
optional features have known gaps; the release notes list them.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [CLI reference](https://github.com/syver777/vowp/blob/main/docs/reference/cli.md)
- [API documentation](https://docs.rs/vowp-cli)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
