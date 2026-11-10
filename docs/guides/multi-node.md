# Multiple nodes

A VOWP project is homed on one node. That node runs the supervisor, owns the database and the job
worktrees, and decides job state. Other nodes are reachable over SSH; in v0.1 there is no cross-node
sync, replication or shared database.

The recommended production layout is **VPS project home + local control**. The VPS owns the repository,
database, worktrees and workers; your computer runs the orchestrator/communicator session and the
`vowp --node vps ...` CLI. Several projects can be homed on one VPS; each has its own supervisor and
slot budget, and the VPS's CPU/RAM plus provider limits are the shared ceiling. v0.1 has no global
cross-project scheduler, so size each project's `max_workers`/`max_verifiers` to fit the total.

> **Status: current.** `vowp init`, `vowp doctor`, the `--node` flag, node registration
> (`vowp node add/ls/rm`) and cross-node session commands are implemented.

## What a node is

| Node | Role |
|---|---|
| `local` | Your laptop. Can run the CLI and, if a project is homed here, the supervisor. |
| `vps` | A server. Runs the same `vowp` binary; can host a project or be a switch target for a profile. |
| any named node | A row in `nodes` with an SSH target and capabilities. |

The supervisor and the project graph live on one node. The `--node <name>` flag runs a command on that
node and forwards stdout, stderr and the exit code. It is SSH, not an API: no port is opened and no token
is sent.

## Prepare the remote node

1. Install the same VOWP binary on the remote and make sure `vowp` is on `PATH` for a non-interactive
   shell.
2. Make SSH key authentication work in batch mode:
   `ssh -o BatchMode=yes <target> vowp --version` must print a version without a prompt.
3. Install `git` and, if the remote will run jobs with windows, `tmux`.
4. Install and sign in the harnesses there exactly as on the local node. VOWP stores no credentials and
   does not copy a login between nodes.

The remote user needs access to the project checkout and worktree root. A version mismatch is not
blocked, but commands and schema versions are only guaranteed within the same release; upgrade both
sides together.

Remote commands run in the remote's own VOWP home and SSH working directory. For project-scoped
commands such as `status`, `job` and `session`, the remote needs its own initialised project
(`vowp init` in a checkout, or `--project` pointing at one); the laptop's database only supplies the
SSH target.

## Register a node

```sh
vowp node add vps vps.example.com
vowp node ls
vowp node rm vps
```

`node add` records the name and SSH target, then tests
`ssh -o BatchMode=yes <target> vowp --version`. The result is stored in `capabilities`; a failure is a
warning unless you pass `--strict`, in which case the command fails. `node ls` prints
`NAME TARGET LOCAL LAST_SEEN VERSION`.

The local node is registered by `vowp init`; you normally do not add it by hand.

## Run a command remotely

```sh
# --node is current; these run on the registered vps
vowp --node vps job ls
vowp --node vps status
vowp --node vps session ls
vowp --node vps session tail <id> --follow
```

`--node` is global: it may appear before or after the subcommand, and it is removed from the arguments
sent to the remote. The CLI builds one safely quoted remote command and runs:

```
ssh -o BatchMode=yes -o ConnectTimeout=10 [-t] <target> vowp <args…>
```

`-t` is added only for interactive commands (`attach`, `session chat`, `board` without `--once`). Exit
255 from SSH becomes exit 7 (`remote`), including a missing remote `vowp`. If the target is the local
node, the flag is ignored and the command runs in-process.

The cross-node session commands keep the bundle as the unit of movement:

- `vowp session import <file> --node vps` streams the bundle over SSH stdin; the remote never needs the
  file path.
- `vowp session export <id> --node vps` writes the bundle locally from the remote's stdout.
- A cross-node `session switch --to … --node vps` exports locally, imports on the target, runs the switch
  there, and records `from_node`/`to_node` on both sides.

## Run sessions on another node

A profile may set `node = "vps"`, which makes that role's sessions run on the VPS while the supervisor
stays on the project's home node. The project checkout must be available on that node; v0.1 does not sync
files between nodes, so provision the checkout yourself. This is how a laptop can plan on Claude Code
locally and run workers on a server.

For a one-off move, prefer a session switch or export/import rather than moving a running job: a job does
not change node during failover in v0.1.

## Example: laptop and VPS

Replace `vps.example.com` with the SSH target you prepared above. `vowp node add` records the target
and probes it, so a successful probe is the quickest sign that the `--node vps` commands will work.

```sh
# on the laptop, where the project is homed
vowp init --name my-project
vowp doctor

vowp node add vps vps.example.com
vowp node ls
vowp up

# watch the local project and run a command on the VPS
vowp status
vowp attach
vowp --node vps job ls
```

`vowp --node vps job ls` reads the VPS's own VOWP home and project checkout, not the laptop's graph;
the VPS needs the initialised checkout described above (or pass `--project` with its path there).

If `node add` warns that the probe failed, fix SSH or the remote `PATH` first. Without `--strict`, the
warning is non-fatal and the node row is still written, but `--node` will fail until SSH to that target
works. With `--strict`, the command fails before writing the row.

If the project itself is homed on the VPS, run `vowp up` there and use `vowp --node vps status` from the
laptop. The laptop still needs its own initialized VOWP home with the `vps` node registered — for example
`vowp init` in a small local project and then `vowp node add vps vps.example.com` — because `--node`
looks the node up in the local database before it opens SSH. It does not need the VPS project's database
or session store.

## Security notes

- Remote access is SSH with the keys you already use. VOWP does not open a port, run a daemon or store a
  token in v0.1.
- `BatchMode=yes` means a missing key fails instead of prompting. Fix the key; do not disable the check
  to "make it work".
- Arguments are single-quote escaped as one remote command string; no argument is interpolated
  unquoted. A command with spaces, quotes or `$` is passed literally.
- A remote node sees the same project checkout and can run harnesses; treat its account and disk with
  the same care as the local one.

## Troubleshooting a remote command

| Symptom | Check |
|---|---|
| Exit 7, `remote` | `ssh -o BatchMode=yes <target> vowp --version` from the same user and shell. |
| `vowp: command not found` | The remote non-interactive `PATH`; install to a standard location or set it in the SSH environment. |
| Version mismatch | Upgrade both binaries. A newer database is refused rather than guessed at. |
| Interactive command hangs | `-t` is only added for interactive commands; a non-TTY `chat` reads until EOF by design. |
| Host key prompt | Add the host key to `known_hosts` first; batch mode must not prompt. |

Next: [troubleshooting](../troubleshooting.md) covers the local and remote failure modes together.
