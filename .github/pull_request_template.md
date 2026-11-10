## Summary

<!-- What does this change, and why? Keep it short and concrete. -->

## Related issue

<!-- Link the issue this fixes, for example "Fixes #123". Write "none" if there is no issue. -->

## What changed

<!-- The important behavior changes, and anything a reviewer should look at first. -->

## Checklist

- [ ] Tests added or updated for the change, or a short explanation of why none are needed
- [ ] Documentation updated where behavior changed (`README.md`, `docs/`, `--help` text)
- [ ] `cargo fmt --all` run
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] No secrets, tokens, `.env` contents, private hostnames or personal paths in the diff, logs or examples
- [ ] Every test or script that touches tmux uses a unique private socket (`tmux -L <unique>`) **and** a private `TMUX_TMPDIR` created before the first tmux command; nothing uses `default`
- [ ] Commits are focused, and AI-assisted commits end with the `AI-Assisted: <model> via VOWP` trailer
- [ ] I have read [`CONTRIBUTING.md`](https://github.com/syver777/vowp/blob/main/CONTRIBUTING.md) and agree to the [`CODE_OF_CONDUCT.md`](https://github.com/syver777/vowp/blob/main/CODE_OF_CONDUCT.md)

## Notes for the reviewer

<!-- Known limits, follow-ups, or how you verified the change beyond the test suite. -->
