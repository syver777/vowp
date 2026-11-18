# The agent node

An **agent node** is a Mac or Linux machine that dials the home and runs local surfaces or
offloaded jobs. It is not a second home: the home still owns the database, the session log and
every merge. The node owns its own node key, path map, harness login, worktrees and surface
tokens, and reaches the home over SSH stdio by default or through an owner-managed WSS proxy.

> **Status: current.** Enrollment, the WSS transport, `agent service`, `agent run --service`,
> the separate-user checks, surface rotation, bundle result and `capacity history` are
> implemented in this tree. The security review checklist and the rollback drill are recorded in
> [`../releases/v0.4-hardening.md`](../releases/v0.4-hardening.md); the owner live Mac smoke test is
> pending the owner's run.

## What the node needs

- macOS or Linux, `git`, and the `vowp` binary you intend to run. Build it from this repository
  with `cargo build --release --workspace` and install the binary where the service can find it.
- Its own harness login and its own provider environment. VOWP never copies the owner's login or
  a provider key to a node.
- A state directory: `~/.vowp-agent` by default, or `VOWP_AGENT_HOME`. The directory is 0700;
  its private files are 0600.
- Network access to the home through SSH, or through the HTTPS endpoint that fronts the optional
  WSS listener.

The home sees a node name, a public key, a project path map and capacity. It does not need to
know whether the account is yours or a dedicated one; [Separate user](#separate-user) explains
the supported shape and its limits.

## Enroll the node

Create the node and a one-time code on the home:

```sh
vowp node enroll mac-studio --label mac
```

The command prints the 52-character code once and the matching `vowp agent enroll` line. The code
expires after ten minutes and is stored only as a hash. Then, on the node:

```sh
vowp agent enroll --target <home-ssh-target> --code-file - --project ~/src/vowp
```

`--project` is repeatable and maps one local checkout; the project name comes from its
`vowp.toml`, and the worktree root defaults to `<checkout>/.vowp/worktrees`. The checkout must
already exist. Without `--project`, enrollment reuses the saved map. `--code-file -` reads the
code from stdin; the code is never in argv, `ps` or a log line.

Enrollment writes `~/.vowp-agent/agent.json` (0600) and the node key pair. It then connects and
performs the `AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome` handshake. The node is
ready when `vowp agent status` shows a connection and a key fingerprint:

```sh
vowp agent status
```

Later project changes do not need a re-enrollment:

```sh
vowp agent paths add --project ~/src/another-project
vowp agent paths ls
vowp agent paths rm another-project
```

A running `vowp agent run` reloads the path map on its next heartbeat and sends
`PathMapUpdate`. Absolute roots stay in `agent.json` and in the owner-only home row; they are
never put in a status snapshot or a frame that carries a job.

### Choose the transport

SSH stdio is the default and needs no home listener:

```sh
vowp agent enroll --target <home-ssh-target> --code-file - --project ~/src/vowp
```

The agent runs this forced command on the home:

```text
ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 <target> vowp agent gateway --stdio
```

WSS is the optional alternative for a network that blocks outbound SSH. It carries the same
frames over a TLS reverse proxy that you operate; VOWP itself opens only a loopback listener.
Enroll with the proxy URL instead of an SSH target:

```sh
vowp agent enroll --wss wss://home.example.com/agent/v1 --code-file - --project ~/src/vowp
```

The URL must be exactly `wss://host[:port]/agent/v1`: no `ws://`, query, fragment, user-info or
other path is accepted. TLS uses the system trust store and hostname verification by default. A
self-signed deployment can hard-pin the leaf certificate:

```sh
vowp agent enroll --wss wss://home.example.com/agent/v1 \
  --tls-fingerprint sha256:<64-lowercase-hex> --code-file -
```

A pin mismatch is a refusal (exit 5), never a warning. In system mode the first successful
connection records the leaf fingerprint in `agent.json`; a later certificate that does not match
that recorded value is refused before the first frame and the error names the old and new
fingerprint. After checking the new value through a trusted channel, confirm it with the
owner-confirmation command (see
[Changed certificate fingerprint](#changed-certificate-fingerprint)); a normal certificate renewal
therefore needs that one owner action. Do not edit `agent.json` while the agent is running.

There is no fallback between the transports. A WSS agent whose proxy is down reconnects with
backoff and spools events; it never dials SSH behind the configuration's back. The reverse-proxy
deployment is in [WSS deployment](#wss-deployment).

## Run the agent

In a terminal:

```sh
vowp agent run --foreground
```

`--role` is `surface`, `executor` or `both` (the default, shown in the help as `ROLE`). The
process reconnects with exponential backoff (1, 2, 4 … 60 seconds with jitter) and rewrites
`status.json` on every heartbeat. `Ping` is answered with `Pong`. A revoked node exits 5 and a
superseded connection exits 4.

For a node that must survive a reboot, logout and crash, install the service manager unit. The
agent never runs as root and VOWP never installs a system-wide unit.

```sh
vowp agent service install --print
vowp agent service install --now
vowp agent service status
```

`--print` writes nothing and invokes no manager; it prints the unit text, the unit path and the
manager commands. `install` without `--now` writes exactly one unit file and prints what to run;
`--now` also runs the current user's manager commands. On systemd, `install` prints, but never
runs, `loginctl enable-linger <user>` — run that yourself if the unit must survive logout.
`--force` replaces a file at the unit path that VOWP did not generate; without it the install
refuses (exit 4) rather than overwrite a foreign file.

| Platform | Unit | Path |
|---|---|---|
| macOS | launchd `LaunchAgent` | `~/Library/LaunchAgents/dev.vowp.agent.plist` |
| Linux | systemd user unit | `$XDG_CONFIG_HOME/systemd/user/vowp-agent.service`, else `~/.config/systemd/user/vowp-agent.service` |

The unit is 0600, owned by the installing user, and carries only the binary path,
`agent run --foreground --service`, `VOWP_AGENT_HOME` and the installing shell's `PATH`. It
carries no token, code, key, provider value or project path. The launchd job also uses
`~/.vowp-agent/run` as its restart marker; the agent writes it during `--service` startup and
removes it after `Revoked` or `Superseded`, so a terminal revocation stops the job and a crash
restarts it. The systemd unit uses `RestartPreventExitStatus=4 5` instead.

Stop and remove the unit without deleting the node key or any token:

```sh
vowp agent service uninstall --yes
```

`uninstall` stops the unit first and removes only a file carrying the VOWP marker. A missing or
foreign unit is not removed. `agent service status` prints the unit path, whether the manager has
it loaded, the reported pid and the last exit status; with `--json` it reports the same fields
and `available: false` when no manager is available.

### Drains, crashes and revocation

SIGTERM starts a drain: the agent stops accepting assignments, flushes the pending spool, sends
`SurfaceDetach` for attached surfaces, releases their leases, stops app hosts it started, writes
`connection = stopped` to `status.json` and exits 0 within ten seconds. The manager allows 20
seconds before SIGKILL. A crash is restarted by the manager; the agent's single-instance socket
prevents two copies, and the restart scans the spool and resumes. A revoked or superseded node
writes the terminal status, removes the launchd marker (or exits 4/5 under systemd) and stays
stopped. A home restart only drops the connection; the agent reconnects and sends `Resume`.

## Check the node

```sh
vowp agent status
vowp agent doctor
```

`agent status` prints the connection, node, home, protocol, last heartbeat, pending spool bytes,
cache counts, key fingerprint, the earliest surface-token expiry and, when mapped, the local
path roots. `--json` adds a `surfaces` array:

```json
{
  "connection": "connected",
  "node": "mac-studio",
  "home": "home-1",
  "protocol": "vowp-agent/1",
  "last_heartbeat": "2026-11-16T10:00:00.000Z",
  "last_error": null,
  "pending_spool_bytes": 42,
  "key_fingerprint": "sha256:0123456789abcdef",
  "surface_token_expires_at": "2026-12-16T10:00:00.000Z",
  "surfaces": [
    {
      "surface": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
      "prefix": "AbCdEfGhIjKl",
      "expires_at": "2026-12-16T10:00:00.000Z",
      "state": "paired"
    }
  ],
  "paths": [
    {
      "project": "vowp",
      "local_root": "/Users/me/src/vowp",
      "worktree_root": "/Users/me/src/vowp/.vowp/worktrees",
      "allow": []
    }
  ]
}
```

A surface state is `paired`, `needs_pair` or `revoked`. The prefix is the first 12
scheme-stripped token characters; the token itself is never printed or written to `status.json`.
A token that expires within seven days prints one warning line, and `agent doctor` reports the
same warning without failing.

`agent doctor` is a local warning list, not a gate: `state_dir`, `service`, `surfaces` and
`logins`. It reports a foreign or loose state directory, a missing or not-loaded VOWP unit, a
leftover or missing launchd run marker, a `needs_pair` surface, an expired or soon-expiring token,
and copied login material (`.ssh`, `.claude`, `.env`, `.git-credentials`, `auth.json`) **by name
only**. It does not inspect a foreign unit's marker. It exits 0 when there are no failures
(warnings are allowed) and 1 on a failure. It never reads the contents of a login file. See
[security.md](../security.md#agent-nodes-and-wss) for what these checks do and do not protect.

## Surfaces: pair, rotate, replace

A surface is the local app or chat that talks to a session through the node. Pairing needs the
home and the node; the home prints the code once and the exact Mac command.

On the home:

```sh
vowp surface pair --session <session-id> --node mac-studio --kind opencode
```

The command prints a ten-minute one-time code, the expiry and a line like:

```sh
vowp agent pair --code-file - --surface <surface-id> --session <session-id> --app opencode
```

Run that line on the node and paste the code on stdin. `agent pair` stores the token at
`~/.vowp-agent/surfaces/<surface-id>/token` (0600, in a 0700 directory) and prints the surface
id, token path and expiry; it never prints the token.

```sh
vowp surface ls
vowp surface ls --node mac-studio
```

`surface ls` shows `ID NODE KIND STATE SCOPES PREFIX EXPIRES OVERLAP`. `PREFIX` is the newest
active token's display prefix, never the token or its hash. `OVERLAP` is `-` normally, the old
token's deadline while a rotation overlap is open, and `PENDING <deadline>` while a queued
rotation has not reached the node. A token whose expiry has passed shows `expired` in the
`EXPIRES` cell while the surface's `STATE` stays `attached`; the token's expiry and the surface's
state are separate facts.

### Rotate a token

```sh
vowp surface rotate <surface-id>
```

A rotation mints a fresh 30-day token with the surface's existing scopes and shortens every other
active token to `now + 10 minutes`; a token that already expires sooner keeps its earlier expiry.
The old token verifies during the overlap and is refused after it. The replacement is queued in
`<home>/agent-frames/<node>/<surface-id>.json` and delivered on the node's next authenticated
connection. A second rotation before delivery replaces that file, so two plaintext tokens are
never left on disk. The command prints the surface, node, prefix, new expiry and old-token expiry;
it never prints the token.

The default works for an online node. Two flags cover an offline or planned window:

```sh
vowp surface rotate <surface-id> --wait 30s
vowp surface rotate <surface-id> --grace 1h
```

`--wait` polls the outbox for the given duration. It exits 0 when the frame was drained and exits
7 with `rotation_pending` when the node is still offline; without `--wait` the command keeps the
shipped exit 0 plus a stderr warning. `--grace` sets the old tokens' overlap to `now + grace`
instead of ten minutes, capped at 24 hours; it is refused for a pending or revoked surface. The
replacement token still expires after 30 days, so a node offline past that deadline must re-pair.

A queued rotation survives a home or agent restart. On reconnect the gateway sends it only to
that surface's node. A replacement token that expires before delivery is still delivered; the
home refuses the expired token on first use, and the surface must be re-paired. If the old token
is refused first, `agent status` marks the local surface `needs_pair`, drops its cached reads and
stops retrying; the status line names the surface and the recovery commands, never the token.

### Replace or re-pair a surface

When a surface is lost, or a token died offline past its replacement expiry, use one transaction
to revoke the old surface and create the replacement:

```sh
vowp surface pair --session <session-id> --node mac-studio --kind opencode --replace <old-surface-id>
```

The old surface must belong to the same node and session and must not already be revoked. Its
queued rotation frame is removed and every active token is revoked, so it cannot receive a stale
token. Then run the new `vowp agent pair` line on the node. The longer manual form still works:

```sh
vowp surface revoke <old-surface-id> --yes
vowp surface pair --session <session-id> --node mac-studio --kind opencode
```

`surface revoke` revokes every active token, marks the surface `revoked`, removes any queued
rotation and writes one ledger note with the action, surface id, node, prefix and expiry. It never
prints a token.

## Separate user

Running the agent as a dedicated **standard** account is supported on macOS and Linux. The shape
is one node name and one node key per account; a second account on the same machine is a second
node, not a second role on the same node. The account owns its state directory, project clone,
worktrees, harness login and provider environment. The owner's account keeps the home SSH key,
the home checkout and the home secrets.

Create the node on the home as usual, then enroll and install the service **as that user**. VOWP
never calls `sudo`, changes uid or writes another user's home; the `sudo -iu` below is the
owner's action:

```sh
sudo -iu vowp-agent -- vowp agent enroll --target <home-ssh-target> --code-file -
sudo -iu vowp-agent -- vowp agent service install --now
sudo -iu vowp-agent -- vowp agent status
loginctl enable-linger vowp-agent
```

`loginctl enable-linger` is printed by `agent service install` on Linux and never run with
`sudo` by VOWP. On macOS a dedicated standard user is a `LaunchAgent` in that user's GUI session;
the user must log in once after a reboot, or the owner bootstraps the job in that user's GUI
domain. A root `LaunchDaemon` or a system-wide systemd unit is unsupported.

The separate user is a filesystem and process boundary for the **owner's** files, not a sandbox
for repository code. The harness runs as the agent user and can read that user's own node key and
surface tokens if it tries; a repository that needs a home-only secret is not offloaded. A project
checkout, worktree root or git directory owned by another uid or group- or world-writable is a
refusal; group and world read/execute remain allowed. A state directory owned by another uid or
mode 0755 is a refusal (exit 5). See [the security model](../security.md#separate-user-limits).

## WSS deployment

WSS is optional and off by default. It lets an agent reach the home where outbound SSH is
blocked, at the cost of a public TLS endpoint, DNS and certificate operations, and a proxy you
must harden. It is not a hosted relay and not a second protocol: the proxy terminates TLS, and
the loopback listener behind it serves the same `AgentFrame` JSON and the same node-key
challenge as SSH.

### 1. Configure the home listener

Add the ingress table to the home's `~/.vowp/config.toml`:

```toml
[ingress.wss]
enabled = true
listen = "127.0.0.1:8743"
path = "/agent/v1"
max_connections = 64
max_frame_bytes = 1048576
handshake_timeout_secs = 10
```

`listen` must be a loopback socket address; a non-loopback value is refused before the listener
binds. `path` must be exactly `/agent/v1`. The frame cap is between 1 byte and 16 MiB; the
default is 1 MiB. These are the only settings this table has; unknown keys are rejected, and a
missing table means WSS is off.

### 2. Start the loopback listener

```sh
vowp up
```

When `[ingress.wss].enabled = true`, `vowp up` starts the hidden `vowp agent gateway --listen`
child on the configured address and stops it with the supervisor. A missing or unstartable
gateway warns and leaves the supervisor running, exactly like the optional web window. The
listener binds loopback only and serves one route; it never terminates TLS and never opens a
public port. Do not run the hidden listener yourself in normal operation.

### 3. Put your own TLS proxy in front of it

The proxy owns the public port, the certificate and TLS. Terminate TLS 1.3, forward only
`/agent/v1`, and pass the WebSocket upgrade. A Caddy site can be as small as:

```text
home.example.com {
  tls test@example.com
  @agent path /agent/v1
  handle @agent {
    reverse_proxy 127.0.0.1:8743
  }
  respond 404
}
```

An nginx location follows the same shape:

```text
location = /agent/v1 {
  proxy_pass http://127.0.0.1:8743;
  proxy_http_version 1.1;
  proxy_set_header Upgrade $http_upgrade;
  proxy_set_header Connection "upgrade";
  proxy_set_header Host $host;
}
location / { return 404; }
```

The proxy is a trusted home component: it sees the decrypted frames, including a surface token,
a lease token, prompts and tool output. Treat a compromised proxy as a home compromise, rotate
the affected surface tokens and revoke the node. Keep its access logs free of frame bodies, and
do not put it on shared infrastructure. VOWP never runs ACME, stores a proxy private key or
writes proxy configuration.

### 4. Enroll and verify

```sh
vowp agent enroll --wss wss://home.example.com/agent/v1 --code-file -
vowp agent status
```

The agent offers the `vowp-agent/1` subprotocol on every connection. A missing or different
subprotocol is refused before the first frame; an unknown path is `404`, a plain `GET` without
the upgrade is `426`, and a binary message is refused. The WebSocket carries one text message per
`AgentFrame`; there is no compression and no query-string credential. A second connection for the
same node supersedes the first.

The first system-mode connection records the leaf certificate fingerprint. A later change is
refused until you confirm the new value with the owner command; a pinned deployment
(`tls = "pin"`) refuses any mismatch as a hard error and still needs re-enrollment. See
[Changed certificate fingerprint](#changed-certificate-fingerprint) and
[the WSS trust boundary](../security.md#the-wss-trust-boundary) for the full security property.

### Changed certificate fingerprint

In system mode, a proxy certificate whose fingerprint differs from the value recorded in
`agent.json` is refused before the first frame, and the error names both values. Verify the new
value through a trusted channel first — out of band, not from the same connection and not from a
page served by that host. Then record it as your normal user:

    vowp agent trust --fingerprint sha256:<64-lowercase-hex>

The command is local-only: it reads and rewrites only the private `agent.json` (0600), never
connects to the home, never runs the agent, and never reads or prints `node.key`, a surface token
or another secret. A missing recorded value is an explicit first record, not a silent
trust-on-first-use reset; the same value prints `fingerprint unchanged` and writes nothing. A
`tls = "pin"` transport is a hard pin: the command refuses it (exit 5), and a changed pinned
certificate needs re-enrollment with the new `--tls-fingerprint`.

## Bundle result

A node without push access can still return work. `[offload] result_transport = "bundle"` tells
the home to accept a `base..head` git bundle over the authenticated agent connection instead of
fetching a pushed branch. The node needs no git credential for the result; it still needs its
node key.

```toml
[offload]
enabled = true
default = "any"
result_transport = "bundle"

[offload.projects.vowp]
placement = "any"
```

Then check the effective policy and dispatch:

```sh
vowp offload policy
vowp offload status
```

The assignment carries `result_transport`, so the value wins for the whole run and neither side
falls back. A bundle node is eligible when the project is mapped, the base commit is fetchable
and the path map is valid; the push-remote match is not required. Read access to the base commit
is still required, so a node with no fetch access is not eligible. A push run keeps the existing
remote check.

The agent creates exactly one ref, `refs/heads/job/<id>`, with `base` as its prerequisite, then
sends `BundleStart`, `BundleChunk` messages and `BundleEnd`. The transfer is capped at 256 MiB
per bundle, five minutes and 256 KiB of raw data per chunk; a disconnect resumes at the offset
the home acknowledges. The home hashes every byte, checks the size and sha256, runs `git bundle
verify`, checks the exact one-ref set, and fetches only that ref into a private staging
repository before importing the verified objects. It then runs the same `owns_respected` and
`secret_scan` gates as a push run and hands the job to the merge queue. A dirty worktree returns
only committed objects; uncommitted files are never in the bundle. A run with `base == head`
fails `result_empty`.

The two failure classes are different:

| Detail prefix | Meaning | What remains |
|---|---|---|
| `bundle_failed` | The transfer or file was unusable: over the cap, sha256/size mismatch, a chunk gap, a missing staging file, home disk full. | The staging file is removed; no project ref; the job stays retryable. |
| `bundle_rejected` | The bytes are well-formed but not the assigned result: `git bundle verify` failed, a foreign ref or head, the base is not a prerequisite, the head is not the reported head or not a descendant. | No project ref and no staging file after settle; the job stays retryable. |

A bundle that passes verification but then fails a gate leaves both the inspection
`refs/vowp/jobs/<id>` and the local `job/<id>` branch in place; the branch stays for inspection
and only the staging worktree is removed. It never merges. The durable result record carries
`bundle_bytes` and `bundle_sha256` beside `transport = "bundle"`, `base_commit` and `head_commit`;
it carries no path, token or secret. See
[the bundle verification sequence](../security.md#bundle-verification).

## Capacity history

The home aggregates the previous completed five-minute bucket on each machine tick and keeps a
bounded history in the global database. Raw `host_samples` stay for 48 hours; the rollups stay
for 30 days. A gap is a gap, not a zero.

```sh
vowp capacity history
vowp capacity history --since 7d --bucket 1h
vowp capacity history --json
```

`--since` accepts a duration or date and defaults to 24 hours. `--bucket` is `5m` (the stored
bucket), `1h` or `1d`; the larger widths are read-time aggregations weighted by the raw sample
count, not stored tables. The human table is `AT SAMPLES MEM_MIN/AVG/MAX LOAD1 DISK_MIN SOURCE`;
`source` is `proc`, `fallback` or `mixed`, and a fallback point's memory and load values are not
a measurement. The JSON document is `CapacityHistory`:

```json
{
  "generated_at": "2026-11-16T10:00:00.000Z",
  "since": "2026-11-15T10:00:00.000Z",
  "until": "2026-11-16T10:00:00.000Z",
  "bucket_secs": 300,
  "retention_secs": 2592000,
  "points": [
    {
      "at": "2026-11-16T09:55:00.000Z",
      "samples": 60,
      "mem_available": {"min": 0, "avg": 0, "max": 0},
      "load1_avg": 0.0,
      "disk_free_home_min": 0,
      "source": "proc"
    }
  ]
}
```

The zeroes stand for byte counts; the real document never invents a measurement. `history` is
home-only and opens the store read-only, so it writes no target, advice, ledger or rollup row and
is safe while the supervisor runs. A `--since` before the retained window is clamped to the
oldest retained bucket and the command says so. Cost is not duplicated here: use
`vowp cost --by day`.

## Routine checks

| Check | Command |
|---|---|
| Enrollment and connection | `vowp agent status` |
| State-directory mode and copied logins | `vowp agent doctor` |
| Service unit and manager state | `vowp agent service status` |
| Surface prefix, expiry and pending rotation | `vowp surface ls` |
| Node reachability and mapped projects | `vowp node show <name>` |
| Effective offload and bundle policy | `vowp offload policy` |
| Capacity history and gaps | `vowp capacity history --since 7d --bucket 1h` |

Next: [Control machines and extra machines](multi-node.md) for `--node` and tunnels, or the
[security model](../security.md) for the trust boundaries and the hardening checklist.
