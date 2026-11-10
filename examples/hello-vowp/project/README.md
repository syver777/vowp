# hello-vowp demo project

This is the target repository for the `examples/hello-vowp` walkthrough. The demo jobs add
three files under `src/`:

- `src/greeting.txt`
- `src/farewell.txt`
- `src/message.txt` (depends on the other two)

`run.sh` in the parent directory creates a temporary checkout and a local bare `origin`,
plus a temporary VOWP home; it does not modify this directory.
