# vowp-adapters

Harness adapters for [VOWP](https://github.com/syver777/vowp), a local-first supervisor for AI coding agents.
This crate is part of the VOWP workspace.

## What it provides

- A Claude Code adapter over `claude -p` stream-JSON output, including limit and error classification.
- An opencode adapter over `opencode run --format json`.
- A generic CLI adapter configured by a TOML mapping file.
- A shared process runner, streaming parser and secret redaction for adapter output.
- Test helpers behind the `testing` feature.

Adapters drive the harnesses' documented headless/JSON interfaces. VOWP does not fork or spoof a harness,
does not scrape a TUI, and does not read or forward harness login tokens.

## Status

VOWP 0.3.0 is an early release. The adapter interfaces are used by the shipped `vowp` CLI, but they are not
yet stable while VOWP is in 0.x.

## Links

- [VOWP README](https://github.com/syver777/vowp/blob/main/README.md)
- [API documentation](https://docs.rs/vowp-adapters)
- [Design notes](https://github.com/syver777/vowp/tree/main/docs/design)

## License

MIT. See the [VOWP license](https://github.com/syver777/vowp/blob/main/LICENSE).
