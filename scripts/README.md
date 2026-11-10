# scripts/ — install, release, watchdog and uninstall

Per-user operations for a VOWP node. Nothing here needs root. This index lists
only the product scripts that ship in the public tree.

| File | Purpose |
|---|---|
| `install.sh` | Copies the binary and templates into place, enables the systemd user unit and adds the five-minute cron check. Supports `--dry-run`. |
| `vowp.service` | systemd **user** unit run by `systemctl --user`; `Restart=always`. |
| `vowp-watchdog.sh` | Cron check for `~/.vowp/heartbeat`; restarts the unit when the heartbeat is missing or older than 180 s, falling back to `vowp up --detach`. |
| `vowp-watchdog.cron` | Standalone crontab template for the same five-minute check. |
| `uninstall.sh` | Stops and removes the unit, cron entry and binaries; keeps `~/.vowp` state. Supports `--dry-run` and `--keep-binary`. |
| `tests/run.sh` | Hermetic tests with fake `systemctl`, `crontab`, `loginctl`, `vowp`, `claude`, `opencode` and `date` on `PATH`. |
| `release/package-archive.sh` | Builds one release archive and its per-archive `.sha256` for a target. |
| `release/generate-third-party-licenses.py` | Regenerates `THIRD-PARTY-LICENSES.md` from the locked dependency graph. |
| `release/check-packages.sh` | Packages every crate and runs the publish dry runs; it never uploads. |
| `release/publish-crates.sh` | Publishes the crates in dependency order; dry run by default, `--execute` uploads. |

## Install

From the repository root, after building the binary:

```sh
cargo build --release -p vowp-cli --bin vowp
./scripts/install.sh
```

Preview without changing anything:

```sh
./scripts/install.sh --dry-run
```

The default locations are the ones `vowp doctor` checks:

| Path | Check |
|---|---|
| `~/.local/bin/vowp` | — |
| `~/.local/bin/vowp-watchdog.sh` | `watchdog` |
| `~/.config/systemd/user/vowp.service` | `systemd` |

`install.sh` also installs this crontab block, with your absolute home path:

```cron
# vowp: 5-minute L8 watchdog check (managed by scripts/install.sh)
*/5 * * * * "/home/you/.local/bin/vowp-watchdog.sh" >/dev/null 2>&1
```

Use `--no-service` or `--no-cron` to skip those steps, and `--linger` to run
`loginctl enable-linger` so the supervisor keeps running after you log out.

The watchdog follows `docs/design/loops.md` §10: the L1 tick writes
`~/.vowp/heartbeat` as unix seconds, and the cron check restarts the supervisor
when the file is missing or more than 180 seconds old. Log lines go to
`~/.vowp/logs/watchdog.log`.

## Uninstall

```sh
./scripts/uninstall.sh
```

This removes the unit, cron entry, watchdog and `vowp` binary. It deliberately
leaves `~/.vowp` (database, session logs, inbox copies and backups) in place;
delete that directory yourself if you want to discard the node state.

## Tests

```sh
sh scripts/tests/run.sh
```

The test temporarily sets `HOME` and `XDG_*`, puts fake `systemctl`,
`crontab`, `loginctl`, `vowp`, `claude`, `opencode` and `date` scripts on
`PATH`, and checks dry-run behaviour, installed paths, cron idempotence,
watchdog restart/fallback/boundaries/malformed input, uninstall, and that
`vowp doctor` probes only the fake harnesses and reports the `systemd` and
`watchdog` checks as `ok`. Set `VOWP_BIN=/path/to/vowp` to run the doctor
check from a specific build. `shellcheck` runs when it is installed.
