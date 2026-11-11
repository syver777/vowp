# Control machines and extra nodes

VOWP 0.2 has one **home** that owns the truth. Every other machine is either a **control machine**
that talks to it or an **executor node** that runs work the home schedules. No control machine opens
a database; no second machine is authoritative.

> **Status: current.** `vowp control`, `vowp tunnel`, `vowp fleet`, `vowp node`, the `--node` redirect
> and cross-node session export/import/switch are implemented. Remote execution uses the home's
> session service; the same session is never written by two machines.

## What each machine is

| Machine | Role | Holds |
|---|---|---|
| **home** | The VPS. Runs the one node supervisor, owns the global and project databases, session logs, leases and snapshot. | All truth. |
| **control machine** | A laptop or desktop. Reads the snapshot, opens tunnels, runs the app or `vowp chat`, sends writes to the home. | `~/.vowp-control/`: config, snapshot cache, spool, surface state and tunnel records. |
| **executor node** | A registered machine that can run a turn. It can also be surface-only (`node add <name> -`) because it dials the home; the home never dials it. | Its own working copy and harness login. |

The phone is a control surface with no installation: an SSH app plus the owner's key, running
`vowp chat` on the home. See [Using your phone](phone.md).

## Prepare SSH

Every route is plain SSH with key authentication and no password prompt:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 <home-ssh-target> true
ssh -o BatchMode=yes -o ConnectTimeout=10 <executor-ssh-target> vowp --version
```

Keep the host keys in `known_hosts`. If a key is missing, fix the key; do not disable batch mode or
put a password in a script. The home needs outbound access to the git remote and to the harness's
provider; a control machine only needs SSH to the home.

## Make a computer the control machine

On the laptop or desktop:

```sh
vowp control init --name home <home-ssh-target>
vowp control status
vowp fleet status
```

`control init` probes the home, writes `~/.vowp-control/control.json` (0600), creates the spool, the
surface state and the tunnel table, and records the home in `nodes.json`. It refuses to overwrite an
existing configuration without `--force`; a failed probe is a warning unless `--strict` is passed.
The setup path does the same thing:

```sh
vowp setup control --apply
```

`setup control` reuses the target from `vowp setup home` (or takes `--target`) and installs no
supervisor. It is the recommended path because it also records the setup state and fetches the first
snapshot.

On a control machine:

- project, fleet, capacity, job, inbox, secret and session commands default to the home; the CLI
  sends them over SSH and returns the home's exit code;
- `vowp status` and `vowp fleet status` try the home over SSH first; only when SSH fails do they print
  the cached snapshot with an `as of` time. A successful `fleet status` refreshes that cache;
- `fleet status --refresh` requires SSH; `fleet status --cached` never opens SSH;
- writes (`project add/pause`, `inbox answer`, `job add/pause/retry`, `capacity set`, ...) always go
  to the home; they never succeed against the cache;
- `vowp up`, `vowp down` and `vowp board` refuse with exit 5 because they need local truth;
- a path given to `project add` is a path **on the home**, not on the laptop.

`vowp control status` prints the config and the cache metadata without SSH. The cache is disposable;
the home remains the only source.

## Open a tunnel

The opencode app host and the one-time secret page bind loopback on the home. A control machine
reaches them through a managed SSH forward. Claude Remote Control does not use a tunnel: it connects
outbound to claude.ai from the home and opens no local port.

```sh
vowp tunnel open opencode --remote-port <port>
vowp tunnel open web --remote-port <port> --local-port <port>
vowp tunnel ls
vowp tunnel close opencode
```

`tunnel open` starts `ssh -N ... -L 127.0.0.1:<local>:<host>:<remote>` in its own process group,
records the pid in `~/.vowp-control/tunnels.json` (0600) and prints the local port when it chose one.
Ports below 1024 need `--allow-privileged`. A duplicate name is a conflict (exit 4). `tunnel ls`
removes records whose process died. There is no `close --all`: close what you opened.

A tunnel is opened from the machine that will use it. On the home, `vowp tunnel` exits 5. Ports and
pids are local to the control machine and never written to the snapshot or the session log.

## Register executor nodes

The home's registry is the `nodes` table. On the home:

```sh
vowp node add <node-name> <executor-ssh-target>
vowp node add laptop -            # surface-only: the laptop dials the home
vowp node ls
vowp node rm <node-name>
```

`node add` probes `ssh -o BatchMode=yes <target> vowp --version` and stores the result in the node's
capabilities. A failed probe is a warning unless `--strict` is passed; a surface-only node is
registered without a probe because the home will never dial it. `node rm` refuses the local node.

On a control machine, `node add/ls/rm` edit the control machine's `nodes.json` mirror rather than a
home database. Add the home once with `control init` or `setup control`; add other nodes only when
you need to address them from the laptop.

## Use `--node`

`--node` is the v0.1 remote-command flag and still works. It re-executes the command over SSH on the
target node:

```sh
vowp --node <node-name> status
vowp --node <node-name> job ls
vowp --node <node-name> session tail <id> --follow
vowp --node <node-name> doctor
```

The target needs its own `vowp` binary and its own VOWP installation for project-scoped commands; a
version mismatch is not blocked, but upgrade both sides together. Interactive commands
(`attach`, `chat`, `session chat`, `board` without `--once`) get a TTY. SSH exit 255 becomes exit 7
(`remote`). Arguments are single-quote escaped into one remote command; a shell alias or wrapper on
the remote that rewrites arguments is a bug, not something to work around.

On a control machine, `--node home` addresses the configured home directly; other names come from
`nodes.json`. `--node` is not a second database and does not make the remote authoritative: the home
still owns the session logs and the supervisor.

## Move a session between nodes

The same session normally stays on the home while you change the surface. If you need a copy on a
different machine, the v0.1 bundle path still works:

```sh
vowp session export <id> --out planner.vowp-bundle.json
vowp session import planner.vowp-bundle.json --node <node-name>
vowp session switch <id> --to <executor> --node <node-name>
```

`export` writes a `vowp-bundle-v1` file with a capsule, the last events and a checksum. `import`
validates it and creates a **child** session; the origin is never modified. A cross-node switch
exports, imports and switches on the target, recording the node transition. In v0.2 this is for
copying or archiving; the normal multi-device path is one home with several surfaces.

## Remote execution and SSH drops

A session has one writer: the home. A surface sends turns through the home's session service, and the
home appends events with a gapless `seq`. If the SSH connection drops during a turn, the surface keeps
its unacknowledged events in a local spool and replays them into the home log when it reconnects.
Ingest is idempotent by event id, so a replay cannot duplicate an event. This is what lets a laptop or
phone lose connectivity without losing the conversation.

The practical rules:

- The home stays up; do not run a second supervisor to "keep the session alive".
- On reconnect, the surface replays its spool into the home log and continues from the last
  acknowledged sequence. The chat view prints only events after the current turn's `grant.tail_seq`;
  use `vowp session show` or `vowp session tail` to read the replayed events from the home log.
- Do not hand-edit `log.jsonl`, a capsule or a spool file to repair a gap; report the session id and
  the first missing sequence instead.
- A surface that does not hold the lease can read in follow mode but every write is refused with the
  takeover notice.

## Example: laptop and home

```sh
# once, on the laptop
vowp control init --name home <home-ssh-target>
vowp setup control --apply

# every day
vowp fleet status
vowp fleet queue
vowp chat <session-id>          # routed to the home over SSH
```

```sh
# once, on the home
vowp project add /srv/projects/alpha
vowp up
```

The laptop has no supervisor, no project database and no worktrees. If it is lost, the home is
unchanged.

## Troubleshooting a remote command

| Symptom | Check |
|---|---|
| Exit 7, `remote` | `ssh -o BatchMode=yes <target> vowp --version` from the same user and shell. |
| `vowp: command not found` remotely | The remote non-interactive `PATH`; install to `~/.local/bin` or use the absolute path. |
| `this is a control machine` | The command needs local truth (`up`, `down`, `board`) or an app host. Run it on the home. |
| `no control configuration` | Run `vowp control init --name home <home-ssh-target>` on this machine. |
| `fleet status` says `stale` | The home is unreachable. Check SSH; the cached answer is still labelled `as of`. |
| A write says it went to the cache | It did not. Writes always route to the home; fix SSH or the control config. |
| Host key prompt | Add the host key to `known_hosts`; batch mode must not prompt. |
| Version mismatch | Upgrade both binaries; a newer database is refused rather than guessed at. |
| Tunnel dies immediately | The remote port is not listening on the home, or `ExitOnForwardFailure` refused it. Check the app host and the port. |
| An argument was mangled | VOWP single-quote escapes the whole remote command; check for a remote shell wrapper. |

Next: [Sessions and surfaces](sessions.md) for leases and takeover, or
[Operating a home](operating.md) for capacity, backups and migration.
