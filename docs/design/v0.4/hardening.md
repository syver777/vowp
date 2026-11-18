# v0.4 — phase 4 hardening: optional WSS, a service, rotation, bundles, a separate user and metrics

Status: **implemented in this tree and shipped in 0.4.0; the phase-exit evidence is recorded in
[`docs/releases/v0.4-hardening.md`](../../releases/v0.4-hardening.md).** The four library slices
and the CLI surface (`4b`–`4f`) are merged on `main`, and `4g` documents the shipped behaviour.
Token rotation's mechanics shipped earlier in `v4-local-4a-surface-rotate`; §4.1 describes them.
The security review (§8.2), the owner live Mac smoke test (§8.3) and the rollback drill (§8.4)
results are recorded on the evidence page; the owner smoke test **passed on 2026-11-18**. No
separate tag or date is promised by this design page; the release notes are
[`docs/release/0.4.0.md`](../../release/0.4.0.md).

This page began as the plan for the phase and keeps the design-time wording and markers in the
sections below. Where a §2–§7 sentence calls a command or flag *(planned)*, the name is the design
target; the shipped form is the one in [`../../reference/cli.md`](../../reference/cli.md) and
[`../../guides/agent-node.md`](../../guides/agent-node.md). The known deviations from the plan
are listed in the build-status section above, and every command shown in a guide exists in this
tree or is marked *(planned)* there.

The accepted [`local-orchestrator.md`](local-orchestrator.md) names the phase-4 scope in §14.1 —
"WSS ingress, launchd, token rotation UX, bundle result, separate user, metrics, docs" — but only
WSS has a sketch (§8.6) and rotation a partial contract (§4.3). A build job cannot be written from
that text without guessing which layer changes, what the security property is, or which test
proves it. This page makes each item implementable: the goal, the wire/file/CLI change, the
security property, the failure mode, the named hermetic tests, and an ordered set of slices whose
concurrently runnable `owns` paths do not overlap.

The home stays the only writer, one log per session stays the rule, no credential is copied to a
node, and no VOWP process opens a public listener by default.

Command markers: a command followed by *(planned)* is part of the original design target. The
build slices in §9 are now merged; the shipped command tree, not this marker, is authoritative.
`vowp surface rotate` shipped in `v4-local-4a-surface-rotate` and §4.1 describes its behavior.

## Build status and known deviations

The shipped tree contains:

- WSS ingress: `[ingress.wss]`, `WssTransport`, the loopback `WssListener`, the hidden
  `vowp agent gateway --listen` and the `vowp up` child start.
- The launchd/systemd-user service: `agent service install|uninstall|status`, the `--service` run
  mode, the `run` marker and the drain.
- Rotation UX: `--wait`, `--grace`, the envelope with `queued_at`/`overlap_until`, the
  prefix/overlap columns and the `needs_pair` state.
- Bundle result: the five frames, the assignment `result_transport`, the node-side bundle and the
  home staging/verification path.
- The separate-user checks (`isolation.rs`), `agent doctor` and the documented ownership rules.
- Owner confirmation for a changed system-mode WSS leaf fingerprint: `vowp agent trust`.
- Metrics history: global schema v7 `capacity_rollups`, the rollup writer/pruner and
  `vowp capacity history`.

Known deviations from the text below:

- `agent service status` has no command-specific `--json`; the global `--json` flag prints the
  documented document.
- `CapacityHistory` also carries `until` and `retention_secs`, and `vowp capacity history` has no
  command-specific `--json` (the global flag applies). The document is described in
  [`../types.md`](../types.md) §16.1 and [`crates/vowp-core.md`](../crates/vowp-core.md) §10.
- The gateway does not discard a queued rotation whose replacement token expired before delivery.
  It delivers the frame; the home refuses the expired token on first use and the owner re-pairs.
  `--wait` still stops at the delivery deadline and reports `rotation_pending`.
- The rollback drill and the owner live Mac smoke test are recorded in
  [`../../releases/v0.4-hardening.md`](../../releases/v0.4-hardening.md). The owner ran the smoke
  test on 2026-11-18; it **passed**, and the record is on that page.

## 1. Scope, invariants and vocabulary

### 1.1 What phase 4 adds

Every item below is planned for phase 4; §4.1 describes the one rotation slice that already ships on
`main`. The default column is the behavior when the owner does nothing.

| Item | One-line outcome | Default |
|---|---|---|
| WSS ingress | An agent reaches the same framed protocol through a TLS reverse proxy where SSH is blocked. | off; SSH stays the transport |
| Service install | `vowp agent service install` writes a launchd `LaunchAgent` or a systemd user unit and the agent survives a reboot or crash. | off; the owner installs it |
| Token rotation UX | Rotation is visible, auditable and recoverable, including for a node that was offline. | rotation is opt-in |
| Bundle result | A node without push access returns `base..head` as a verified git bundle over the agent channel. | `result_transport = "push"` |
| Separate user | A dedicated standard account is a supported way to run the agent; the rules are explicit. | the owner's account |
| Metrics history | A bounded local history for capacity and cost feeds a read-only CLI/JSON surface. | 30 days of 5-minute rollups |
| Docs and release gates | Security review, owner live Mac smoke test and rollback drill are named phase-exit activities. | release only after the gates pass |

### 1.2 Invariants that still bind

1. **One home.** The home remains the only writer of the global DB, project DBs and session logs.
   A WSS proxy, a service unit, a bundle or a separate user does not add a second writer.
2. **One log.** An offloaded run appends to the same `sessions/<id>/log.jsonl` through
   `IngestSession`. A bundle result returns git objects, not a second session log.
3. **No credential copying.** A node uses its own git credentials, provider login and node key.
   A bundle result needs no git credential on the node; it still needs the node key.
4. **No new public listener by default.** The home's processes bind loopback or a Unix socket. A
   WSS deployment adds a proxy the owner configures; VOWP itself binds `127.0.0.1`.
5. **No silent fallback.** A WSS agent that cannot connect does not dial SSH behind the config's
   back. A bundle run does not fall back to push, and a push run does not fall back to a bundle.
6. **Least privilege.** A surface token, a lease token and the node key keep the scopes in §4.1 of
   the accepted note. New features do not widen them.
7. **Redaction and paths.** Exact-value redaction, `[REDACTED:<NAME>]`, `PathRef` and
   `<external>` are unchanged. New audit rows and status fields carry names, prefixes and
   timestamps, never values or absolute local paths.

### 1.3 Vocabulary

The terms are the accepted note's: **home**, **node**, **agent**, **gateway**, **surface**,
**lease**, **path map**, **path ref**, **run fence**, **pending spool**, **offload**, **home-only
secret**. Two additions are used below:

- **ingress** — the owner-deployed TLS endpoint (a reverse proxy) plus the loopback WebSocket
  listener the gateway serves. It is not a hosted relay.
- **rollup** — one aggregated metrics row for a fixed time bucket, computed from raw host samples.

## 2. WSS ingress (optional)

### 2.1 Goal and shape

Let an agent reach the home from a network that blocks outbound SSH, without changing the frame
protocol, the node-key challenge, the lease rules or the run fences. The home opens no public port
by itself; the owner deploys a reverse proxy and tells VOWP to listen on loopback behind it.

```text
Mac                                          VPS home
┌──────────────────────────────┐             ┌───────────────────────────────────────┐
│ vowp agent (wss transport)   │   TLS 1.3   │ reverse proxy (owner-managed, 443)    │
│   WssTransport               │ ──────────► │   /agent/v1  → 127.0.0.1:<port>       │
│   AgentFrame JSON per message│             │        │                              │
└──────────────────────────────┘             │        ▼                              │
                                             │ vowp agent gateway --listen           │
                                             │   (loopback WebSocket listener)       │
                                             │        │                              │
                                             │        ▼                              │
                                             │ vowp_core::agent::gateway::serve      │
                                             │        │                              │
                                             │        ▼                              │
                                             │ supervisor / store (home only)        │
                                             └───────────────────────────────────────┘
```

The proxy is not a VOWP process and is not configured by `vowp up`. The VOWP listener binds only a
loopback address. Each accepted WebSocket becomes one `gateway::serve` call, so enrollment, the
challenge-response, heartbeats, surface pairing, dispatch, lease frames, orchestrator commands and
fencing are the same code paths as SSH stdio.

### 2.2 Wire and file contract

**Home configuration.** A new `[ingress.wss]` table in the node config (default off). `NodeFile` in
`crates/vowp-protocol/src/config.rs` gains an `ingress: IngressConfig` field; the struct is
`#[serde(deny_unknown_fields)]`, so the field must exist before a `config.toml` with this table
parses:

```toml
[ingress.wss]
enabled = false
listen = "127.0.0.1:8743"   # must be a loopback address
path = "/agent/v1"
max_connections = 64
max_frame_bytes = 1048576
handshake_timeout_secs = 10
```

`listen` is parsed as a socket address. A non-loopback address is refused at startup with a clear
error (exit 5) and the process does not bind. `path` is a single literal route; there are no
wildcards and no other routes. The port is a local implementation detail of the deployment; the
proxy owns the public port.

**Home command.** A hidden home command `vowp agent gateway --listen <addr>` *(planned)* starts the
loopback listener and serves one gateway connection per WebSocket. `vowp up` starts this child only
when `[ingress.wss].enabled = true`; it locates the binary like the daemon does
(`VOWP_GATEWAY_BIN` for tests, then the sibling binary, then `PATH`) and stops it on shutdown. A
missing or unstartable listener warns and leaves the supervisor running, exactly like the web
window. `vowp agent gateway --stdio` stays the SSH forced command.

**Agent configuration.** `TransportConfig` gains a second variant:

```toml
[agent.transport]
kind = "wss"
url = "wss://<home-host>/agent/v1"
tls = "system"              # system | pin
# fingerprint = "sha256:<hex>"   # required when tls = "pin"
```

`vowp agent enroll --target <ssh-target> --code-file - --wss <url> [--tls-fingerprint <sha256:…>]`
*(planned)* writes that variant; `--target` becomes optional when `--wss` is given, and editing
`agent.json` by hand is the fallback. The node key and the one-time code are the same; no new
credential is introduced.

**Frames.** One WebSocket **text message** carries exactly one JSON `AgentFrame`, with no trailing
newline. The `Sec-WebSocket-Protocol` offer is `vowp-agent/1`; the server selects it, and a missing
or different subprotocol is refused before the first frame. Binary messages are refused. A message
over `max_frame_bytes` closes with WebSocket close code `1009`. Compression is not offered. The
server may reassemble a fragmented message up to the cap, but never treats one fragment as a frame.
The gateway's line reader is adapted to message boundaries; no frame type, field or sequence rule
changes.

**Handshake and fencing.** Unchanged from §8.2 of the accepted note: `AgentHello` →
`HomeChallenge` → `AgentAuth` → `HomeWelcome`; one connection per node; a second connection
supersedes the first; `Ping`/`Pong` every 10 seconds; no pong for 30 seconds closes; the agent
reconnects with the same jittered backoff; the home checks `(run_id, epoch, connection_id)` on every
late frame. A replay of a challenge nonce fails exactly as it does over SSH.

### 2.3 TLS and the certificate source

- The proxy terminates TLS. The minimum is TLS 1.3; the proxy presents a certificate issued by a CA
  in the system trust store, or one its own ACME client obtains (for example Caddy's automatic
  certificates, or nginx with a certificate the owner already manages). VOWP never runs ACME, never
  stores a private key and never writes proxy configuration.
- The default agent mode is `tls = "system"`: the system trust store plus hostname verification.
  Hostname verification fails before the challenge; the `home_id` in `HomeChallenge` then binds the
  application identity, so a certificate for the wrong home is refused even when the host resolves.
- At the first WSS connection the agent records the leaf/SPKI fingerprint in `agent.json` as an
  informational value. A later connection that presents a different fingerprint prints a warning and
  is refused until the owner confirms it with
  `vowp agent trust --fingerprint <sha256:…>` or re-enrolls. The confirmation is local-only: it
  loads `agent.json`, accepts only a `sha256:<64 lowercase hex>` value, and rewrites the private
  0600 file without connecting to the home, running the agent or reading `node.key` or a surface
  token. A missing recorded value is an explicit first record, not a silent trust reset; the same
  value prints `fingerprint unchanged` and does not rewrite the file. There is no silent
  trust-on-first-use reset. A normal certificate renewal therefore needs one owner confirmation;
  that is deliberate.
- `tls = "pin"` with `--tls-fingerprint` is the self-signed deployment: the fingerprint is a hard
  pin, and a mismatch is a refusal (exit 5), never a warning. `vowp agent trust` refuses a pin
  transport (exit 5), so a changed pinned certificate needs re-enrollment with the new
  `--tls-fingerprint`. Pin mode is not the default.
- mTLS is optional defense in depth: a proxy can require a client certificate in addition to the
  node-key challenge. It is not the primary authentication and is not required.

### 2.4 Security property, limits and why it is optional

**Property.** A WSS connection authenticates the same node key, carries the same frames, and gets
the same authorization decisions as an SSH connection. The VOWP listener has only the `/agent/v1`
route, and the proxy must expose only that route; there is no daemon, admin, health, directory or
file route on it. The VOWP listener is loopback-only.

**The proxy is trusted.** Because the proxy terminates TLS, it reads the decrypted frames: it can
see a `SurfaceIssued` token, a lease token in a grant, prompts and tool output, exactly as the
gateway process can. It cannot forge a node signature, replay a nonce or mint a surface token
without the node key, but a compromised proxy can read and replay what it sees on an authenticated
connection. Treat the proxy as part of the home, run it on the same host with the same trust, keep
its access logs free of frame bodies, and do not put it on shared infrastructure. mTLS only
authenticates the Mac to the proxy; the node-key challenge remains the application authentication.
This is the honest cost of a TLS terminator, and it is why WSS is optional and off by default.

**Limits.** No compression (avoids the compression side-channel class), no HTTP/2 push, no
WebSocket extensions other than the subprotocol, no query-string credential, no cookie, no header
trust. The one-connection-per-node rule and a handshake rate limit apply; an unknown path is `404`;
a non-upgrade `GET /agent/v1` is `426`; a wrong subprotocol is `400`. Proxy access logs carry the
node name only if the proxy adds it; VOWP logs no token, code, signature or path map.

**Why optional.** SSH stdio already satisfies the MVP: no new public listener, the owner's key-only
sshd, a forced command and the same frames. WSS buys reachability on networks that block SSH, at the
cost of a public TLS endpoint, DNS and certificate operations, a proxy to harden, new rate limits
and a larger attack surface. It is an explicit deployment choice, not a second protocol and not a
hosted relay. Tailscale or WireGuard remain deployment options under either transport, not VOWP
identity.

### 2.5 Failure modes

| Failure | Agent | Home | Owner sees |
|---|---|---|---|
| Proxy down or DNS failure | reconnect with backoff; spool events; no SSH fallback | listener idle | `wss: connection refused` in `agent status` |
| Certificate expired or hostname mismatch | refuse before `AgentHello`; exit 5 on enroll, retry with backoff when running | — | a clear TLS error naming the host and the reason, never a token |
| Fingerprint changed | refuse; require `agent trust` (system) or re-enroll (pin) | — | a warning naming the old and new fingerprint |
| Unknown path or subprotocol | clear HTTP/WebSocket error | `404`/`400`, no frame processed | `wss: 404 /agent/v1` |
| Frame over the cap | close; reconnect | close `1009`, no partial frame | `frame too large` |
| Second connection for one node | old connection gets `Superseded` | old connection closed | one active agent |
| Proxy restart | reconnect; same node key; `Resume` | per-run and per-surface resume result | a short offline window |
| Non-loopback `listen` | — | startup refused, exit 5 | a config error naming the address |
| Home restart | reconnect; `Resume` | same as SSH | home restarting |

A compromised proxy is a home compromise, not a node compromise: it can read frames and tokens in
flight. Suspect one, rotate the affected surface tokens and revoke the node; the listener has no
other route and no admin path to leak.

### 2.6 Files, tests and non-goals

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** the `[ingress.wss]` table on `NodeFile` in `crates/vowp-protocol/src/config.rs`; a blocking `WssTransport` in `crates/vowp-agent/src/transport.rs` with a new `crates/vowp-agent/src/wss.rs` message/line adapter; `TransportConfig::Wss` in `crates/vowp-agent/src/config.rs`; transport selection in `crates/vowp-agent/src/client.rs` and `crates/vowp-agent/src/lib.rs`; the hidden listener in `crates/vowp-cli/src/cmd/agent.rs`; `vowp up` child start in `crates/vowp-cli/src/cmd/up.rs`; the flags in `crates/vowp-cli/src/cli.rs`; `tungstenite` with `rustls-tls-native-roots` added with `cargo add -p vowp-agent`; the deployment guide is a docs-slice file. Adding `ingress` to the `#[serde(deny_unknown_fields)]` `NodeFile` breaks the full literal at `crates/vowp-cli/src/setup/commands.rs`; 4b updates that literal with `ingress: IngressConfig::default()`, and it is the only CLI source file 4b may touch, only for that literal (4b's only CLI test edit is the `AgentStatus` literal update named in §9.1). `crates/vowp-cli/tests/core.rs` uses `..NodeFile::default()` and needs no change. **Files:** `crates/vowp-protocol/src/config.rs`, `crates/vowp-protocol/tests/config.rs`, `crates/vowp-cli/src/setup/commands.rs`, `crates/vowp-agent/src/transport.rs`, `crates/vowp-agent/src/wss.rs` *(new)*, `crates/vowp-agent/src/config.rs`, `crates/vowp-agent/src/client.rs`, `crates/vowp-agent/src/lib.rs`, `crates/vowp-agent/Cargo.toml`, `crates/vowp-cli/src/cmd/agent.rs`, `crates/vowp-cli/src/cmd/up.rs`, `crates/vowp-cli/src/cli.rs`, `crates/vowp-agent/tests/transport.rs` *(new)*, `crates/vowp-cli/tests/agent.rs`. | `node_file_accepts_the_ingress_wss_table` (new), `wss_carries_the_same_frames_as_stdio` (new; a local TLS proxy on `127.0.0.1:0`), `wss_rejects_a_replayed_nonce_and_a_bad_signature` (new; mirrors `agent_auth_rejects_a_bad_signature_and_a_replayed_nonce`), `wss_refuses_a_changed_fingerprint_without_confirmation` (new), `wss_listener_refuses_a_non_loopback_bind` (new), `wss_route_refuses_every_path_but_agent_v1` (new). | A hosted relay; automatic certificate issuance inside VOWP; multi-user accounts; mTLS as a requirement; HTTP/3/QUIC; Windows; a public bind by a VOWP process. |

The WSS library portion belongs to `v4-local-4b-node-runtime`, including
`crates/vowp-protocol/src/config.rs` and `crates/vowp-protocol/tests/config.rs` for the
`[ingress.wss]` field and `node_file_accepts_the_ingress_wss_table`; the CLI wiring and the proxy
guide belong to `v4-local-4f-cli-surface` and `v4-local-4g-hardening-docs` (see §9).

## 3. Service install (launchd and systemd-user)

### 3.1 Goal and commands

Make the agent survive a reboot, a login and a crash without a terminal, and make stopping it a
service operation rather than a `kill`. The agent never runs as root and VOWP never installs a
system-wide unit.

```sh
vowp agent service install [--print] [--force] [--now]   # (planned)
vowp agent service uninstall [--yes]                     # (planned)
vowp agent service status [--json]                       # (planned)
```

`install` writes exactly one unit file, prints the path and the manager commands, and does nothing
else unless `--now` is passed. `--print` writes nothing, invokes no manager and prints the unit text
and the commands; it is the test seam and a dry run. `--force` replaces a file with the same path
that VOWP did not generate. `uninstall` stops the unit first, then removes only a file that carries
the VOWP marker; it keeps `~/.vowp-agent` and every token. `--now` runs the manager commands for the
current user without `sudo`. `status` prints the unit path, whether it is loaded, the manager's
reported pid and the last exit status; it reads the manager (or `--json` reports `unavailable`) and
never prints a secret.

Platform selection is compile-time: `#[cfg(target_os = "macos")]` uses launchd;
`#[cfg(target_os = "linux")]` uses systemd user units; any other target exits 5 with
"agent service is supported on Linux and macOS only". A missing `HOME` exits 2.

### 3.2 Generated launchd job

The file is `~/Library/LaunchAgents/dev.vowp.agent.plist`, mode 0600, with this shape (paths are
placeholders; the installer writes the resolved values):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- generated by vowp agent service install -->
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.vowp.agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>&lt;vowp-binary&gt;</string>
    <string>agent</string><string>run</string>
    <string>--foreground</string><string>--service</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>VOWP_AGENT_HOME</key><string>&lt;agent-state-dir&gt;</string>
    <key>PATH</key><string>&lt;path-from-the-installing-shell&gt;</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>PathState</key>
    <dict><key>&lt;agent-state-dir&gt;/run</key><true/></dict>
  </dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>20</integer>
  <key>Umask</key><integer>63</integer>
  <key>StandardOutPath</key><string>&lt;agent-state-dir&gt;/logs/agent.log</string>
  <key>StandardErrorPath</key><string>&lt;agent-state-dir&gt;/logs/agent.log</string>
</dict>
</plist>
```

The `run` marker is the restart contract. The launchd installer creates `<state_dir>/run` (0600)
whenever it writes the plist, before `--now` bootstraps the job, and `vowp agent run --service`
leaves it in place until it removes it after `Revoked` or `Superseded`; a crash before the first
connection therefore still restarts. launchd keeps the job alive while the path exists, so a crash
restarts it and a terminal revocation lets it stop. Removing the marker is the only way the job
ends itself; a `launchctl bootout` from `uninstall` is the explicit stop. The systemd unit does not
use the marker; its `RestartPreventExitStatus` handles the terminal exits.

The install commands are `launchctl bootstrap gui/<uid> <plist>`, `launchctl enable
gui/<uid>/dev.vowp.agent` and, with `--now`, `launchctl kickstart -k gui/<uid>/dev.vowp.agent`. A
non-GUI login bootstrap is not attempted. The plist is owned by the user and mode 0600; launchd
accepts a user-owned plist in the user's `LaunchAgents`.

### 3.3 Generated systemd user unit

The file is `$XDG_CONFIG_HOME/systemd/user/vowp-agent.service` when `XDG_CONFIG_HOME` is an
absolute path, else `~/.config/systemd/user/vowp-agent.service`, mode 0600:

```ini
# generated by vowp agent service install
[Unit]
Description=VOWP local agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=<vowp-binary> agent run --foreground --service
Environment=VOWP_AGENT_HOME=<agent-state-dir>
Environment=PATH=<path-from-the-installing-shell>
Restart=on-failure
RestartPreventExitStatus=4 5
RestartSec=5
KillMode=mixed
TimeoutStopSec=20
Umask=0077

[Install]
WantedBy=default.target
```

`install --now` runs `systemctl --user daemon-reload`, then `enable --now vowp-agent.service`.
`uninstall` runs `disable --now`, then `daemon-reload`, then removes the file. `install` prints, but
never runs, `loginctl enable-linger <user>` so a user unit survives logout; enabling linger needs
the owner's own privilege and is never wrapped in `sudo` by VOWP.

### 3.4 User, XDG directories and ownership

- The unit runs as the user that ran `install`. There is no `User=` in a user unit and no attempt
  to install for another account. A dedicated account installs its own unit while logged in as that
  account; see §6.
- The state directory is `~/.vowp-agent` unless `VOWP_AGENT_HOME` names another path. The unit
  writes that value explicitly so a login-shell profile cannot silently move the state. The
  directory is created 0700 and every file is 0600; the service refuses to run when the state
  directory is owned by another uid or mode is group/world accessible.
- The unit captures the invoking shell's `PATH` (for example a Homebrew prefix) so the harness and
  `git` stay visible under the service manager; `PATH` is a list of directories, not a secret. The
  unit carries no secret: no token, no code, no key, no provider value, no absolute project path.
  The node key and surface tokens are read from the state directory by the agent.
- `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` of the user are not
  rewritten. A managed local opencode orchestrator already gets a private XDG root under
  `~/.vowp-agent/orchestrators/<name>/`; the unit does not change the user's global app state.

### 3.5 Restart and drain

- **Stop / logout / shutdown.** SIGTERM starts a drain: the agent stops accepting new run
  assignments, flushes the pending spool, sends `SurfaceDetach` for attached surfaces, releases
  their leases, stops any app host it started, writes `status.json` with `connection = stopped`, and
  exits 0 within 10 seconds. `ExitTimeOut`/`TimeoutStopSec` is 20 seconds, then the manager sends
  SIGKILL. A killed agent leaves its spool and worktrees for inspection; the home fences its runs
  after the grace period.
- **Crash.** The service manager restarts the agent after `ThrottleInterval`/`RestartSec`. The
  agent's single-instance socket bind prevents two copies; a restart scans the spool and resumes; it
  never restarts a harness that crashed (the accepted note's rule).
- **`Revoked`/`Superseded`.** The agent writes the terminal status, removes the launchd `run`
  marker (or exits 4/5, which `RestartPreventExitStatus` excludes under systemd) and the service
  stays stopped. A re-enrollment starts it again with `agent service install --now` or
  `launchctl kickstart`/`systemctl --user start`.
- **Home restart.** The connection drops; the agent reconnects with backoff and `Resume`. The
  service is not restarted, and no run is restarted.
- **Home brake/capacity.** The agent keeps running; only dispatch changes. The service does not
  react to capacity.

### 3.6 Failure modes

| Failure | Behavior |
|---|---|
| Unit path already holds a foreign file | refuse with exit 4 unless `--force`; never overwrite silently |
| `launchctl`/`systemctl` missing | `--print` still works; `--now` exits 7 with the command it could not run |
| `--now` as a non-owner | the manager fails; the unit file stays for the owner to load |
| Plist/unit parse error after an edit | `--force` restores the generated text; the old file is not backed up |
| Log directory unwritable | install fails before writing the unit; the state dir is checked first |
| Revoked node under launchd | `run` marker removed; job stops; status says `revoked` |
| Revoked node under systemd | exit 5; `RestartPreventExitStatus=4 5` stops the restart loop |
| Crash loop | manager throttles; the home sees `offline` and waits; a crash does not move a job |
| Uninstall while a run is active | the service drain stops the run, the home fences after grace; the worktree remains |
| Separate user's unit installed by the owner | refused: the command runs as the current user, not as another account |

### 3.7 Files, tests and non-goals

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** a `service` module that renders, validates and (with `--now`) applies one launchd plist or one systemd user unit; the `--service` run mode with the `run` marker and the drain; the ownership/root checks; `uninstall` with the generated marker; the CLI flags and help. **Files:** `crates/vowp-agent/src/service.rs` *(new)*, `crates/vowp-agent/src/isolation.rs` *(new)*, `crates/vowp-agent/src/client.rs`, `crates/vowp-agent/src/lib.rs`, `crates/vowp-agent/src/config.rs`, `crates/vowp-cli/src/cli.rs`, `crates/vowp-cli/src/cmd/agent.rs`, `crates/vowp-agent/tests/service.rs` *(new)*, `crates/vowp-cli/tests/agent.rs`. | `agent_service_install_writes_the_launchd_plist_under_a_temp_home` (new), `agent_service_install_writes_the_systemd_user_unit_under_a_temp_xdg_config` (new), `agent_service_install_never_calls_the_manager_without_now` (new), `agent_service_uninstall_removes_only_its_own_unit` (new), `agent_service_drain_flushes_the_spool_and_releases_the_lease` (new), `agent_service_refuses_a_foreign_owner_or_loose_state_dir` (new), `agent_refuses_to_run_as_root` (the §14.3 name; the root check is a pure predicate tested with uid 0). | A root `LaunchDaemon`; a system-wide systemd unit; Windows service support; auto-update; installing for another user; managing the user's shell profile; `loginctl enable-linger` execution. |

## 4. Token rotation UX (after `v4-local-4a-surface-rotate`)

### 4.1 What the shipped slice does

`v4-local-4a-surface-rotate` is the shipped slice for this behavior (merged on `main`). It is the
base for this section; the remaining subsections are the new work. Re-read the code before editing
it:
`crates/vowp-core/src/agent/pair.rs` (`rotate_surface_token`, `SURFACE_TOKEN_OVERLAP`),
`crates/vowp-store/src/repo_surfaces.rs` (token rows), `crates/vowp-core/src/agent/gateway.rs`
(frame drain), `crates/vowp-agent/src/client.rs` (live and pairing-time application) and
`crates/vowp-cli/src/cmd/surface.rs`.

The shipped semantics:

- A rotation issues a fresh 30-day token with the surface's existing scopes. Every other active
  token is shortened to `now + 10 minutes`; a token that already expires sooner keeps its earlier
  expiry. The old token verifies during the overlap and is refused after it.
- Only the home's `surface_tokens` rows are hashed: each row keeps a token hash, the 12-character
  display prefix and the scopes, never the token. The plaintext replacement token is persisted at
  rest in two places: the queued `SurfaceIssued` frame file
  `agent-frames/<node>/<request>.json`, which is plaintext JSON holding the full token until the
  gateway delivers it and is removed after delivery; and the agent's `surfaces/<id>/token`, which
  is also plaintext after it is applied. Both files are 0600 in 0700 directories. `vowp surface
  rotate` prints only the prefix; the token is never printed, logged, put in a URL or included in
  the command's JSON output.
- The replacement `SurfaceIssued` is queued in a durable per-node `agent-frames` outbox (new in that
  slice) and drained by the gateway on the node's authenticated connection. The file is 0600 in a
  0700 directory and removed after a successful write; a queued frame survives a home or agent
  restart. A second rotation for the same surface replaces the queued frame rather than leaving two
  plaintext tokens on disk.
- `vowp surface rotate <id>` prints the surface, node, prefix, new expiry and old-token expiry. It
  warns on stderr when the node is not online, still exits 0, and never prints the token. A pending
  surface is exit 4, an unknown one exit 3, a revoked one exit 5.
- The agent applies a live or pairing-time `SurfaceIssued` through a private temporary file and a
  rename, refreshes the existing session binding, drops the surface's cached reads, updates the
  expiry in `status.json`, and deletes the token when `SurfaceRevoked` arrives. A queued rotation
  that arrives before a pairing answer is applied without losing the pairing answer.
- Named tests already in that slice:
  `surface_token_rotation_keeps_the_old_token_for_the_overlap`,
  `surface_rotate_refuses_pending_and_revoked`,
  `surface_rotation_never_extends_a_shorter_expiry`,
  `surface_rotate_queues_for_an_offline_node_and_warns`,
  `surface_rotate_never_prints_the_token`,
  `surface_rotate_uses_the_existing_exit_codes`,
  `agent_applies_a_rotated_token_from_the_live_connection`,
  `agent_revoked_surface_deletes_its_token`.

The remaining work is the UX and the offline edge, not the token mechanics.

### 4.2 Rotation while the node is offline

The queued frame is durable, but the overlap starts when the home **queues** the rotation. A node
that reconnects after the overlap has no valid token until it receives the queued frame; that is
acceptable because it could not write while offline. The real gaps are the owner's visibility and
the case where the node does not reconnect for longer than the replacement token's 30-day life.

The design adds:

- `vowp surface rotate <id> --wait <duration>` *(planned)*: queue the rotation, then poll the outbox
  until the frame is drained or the duration expires. Exit 0 when delivered; exit 7 with
  `rotation_pending` when the node is still offline. Without `--wait`, the command keeps the
  shipped behavior (exit 0 and a warning).
- `vowp surface rotate <id> --grace <duration>` *(planned)*: shorten the old tokens to
  `now + grace` instead of `now + 10 minutes`, capped at 24 hours. The default stays 10 minutes; a
  longer grace is an explicit owner trade-off for a planned offline window. `--grace` is refused
  when the surface is not attached or the node is revoked.
- A queued rotation carries `queued_at`, the replacement's `expires_at` and the old tokens'
  `overlap_until`. The carrier is the `crates/vowp-core/src/agent/spool.rs` envelope, not
  `AgentFrame::SurfaceIssued`: `QueuedAgentFrame` gains `queued_at` and `overlap_until`, and the
  file becomes `{"queued_at": …, "overlap_until": …, "frame": {…}}`; the frame and its schema do
  not change. A bare-frame file written by `4a` reads as an envelope with both fields absent.
  `surface ls` shows `PENDING` while the frame is undelivered and the delivery deadline; `agent
  status` shows the local expiry. When the replacement token expires before delivery, the gateway
  discards the frame and marks the surface `expired`; the owner must re-pair.
- The gateway never sends a frame for another node. A revoked surface removes its queued
  `SurfaceIssued` before `SurfaceRevoked` is sent; rotation after revocation is refused, so a
  revoked surface cannot be resurrected from the outbox.

The recommended default is the security-first 10-minute overlap. A weekend-offline Mac loses its
surface token after the overlap, receives the replacement on reconnect, and resumes without a
re-pair; only a node offline past the replacement's own expiry needs the re-pair path in §4.5.

### 4.3 Prefix and expiry display

- `vowp surface ls` gains a `PREFIX` column for the newest active token and an `OVERLAP` marker when
  an old token is still valid during a rotation; the existing `EXPIRES` column keeps the newest
  token's expiry. The human output never shows the token or its hash. `--json` gains `prefix` and
  `overlap_until`; `expires_at` stays. A surface with no active token keeps `-` in both.
- `vowp surface ls --node <name>` and `vowp surface ls` keep their current filters. A surface whose
  only token has expired shows an `expired` marker in the `EXPIRES`/`OVERLAP` output while its
  `STATE` remains `attached`; the token's expiry and the surface's state are separate facts.
- `vowp agent status` keeps `surface_token_expires_at` (the earliest expiry) for compatibility and
  adds a `surfaces` array *(planned)*: `{surface, prefix, expires_at, state}` where `state` is
  `paired`, `needs_pair` or `revoked`. The prefix is computed locally from the stored token's first
  12 scheme-stripped characters; the token itself is never printed, and `status.json` keeps no
  token.
- A token expiring within 7 days prints one `warning:` line on `vowp agent status` and
  `vowp surface ls`, and `vowp agent doctor` *(planned)* reports it as a warning, not a failure. The
  threshold is a constant, not a config value in this slice.

### 4.4 The ledger/audit row

Rotation today writes no ledger row. The design adds one `node_ledger` row of kind `note` for each
`surface rotate` and `surface revoke`, plus the token-issuing pairing path (the home's
`SurfaceIssued`, not the pending `surface pair`). All three go through
`vowp_core::ledger::Ledger::record` so the existing exact-value redactor applies. The rotate/revoke
rows are written in `pair.rs`, whose functions gain a `&Deps` argument (or take a `&Ledger` built
from one). `v4-local-4c-rotation-ux` owns the two call sites and the in-file test call in
`crates/vowp-cli/src/cmd/surface.rs` for that signature update only; `v4-local-4f-cli-surface` owns
the rest of the file and the flags. That keeps 4c's workspace test gate green without an unowned
call site. The pairing-issue row is written at the gateway call site in `gateway.rs` (4d):
`issue_surface_token` keeps its `&Store` argument, and the gateway builds the entry from the
returned `IssuedSurface`, because `Ledger` is constructed from `Deps` and that call site owns them.
The summary is `surface <id> rotated` (or `revoked`/`paired`); the detail is:

```json
{"action":"rotate","surface":"<id>","node":"<name>","prefix":"<12 chars>","expires_at":"<rfc3339>","old_expires_at":"<rfc3339>"}
```

No token, no hash, no absolute path. `LedgerKind` gains no variant: the database `CHECK` constraint
and every existing match stay unchanged, and a `note` row with `detail.action` is enough to filter.
A duplicate command writes a second row; the ledger is append-only and is not deduplicated.

### 4.5 The agent's re-pair path

When the home refuses a surface token (expired, revoked, or a hash mismatch), the agent must stop
using it and must not retry it forever.

- The agent marks the local surface `needs_pair` in `status.json`, removes the expiry from the
  active set, drops the read cache for that surface, releases the lease if it holds one, and stops
  the app host it started (or leaves an attached app read-only). It does not delete the token file;
  the file is dead but keeping it makes the failure inspectable.
- The status line and `last_error` name the surface and the reason, never the token. The message
  names the exact recovery commands: `vowp surface revoke <id>`, then
  `vowp surface pair --session <id> --node <name> --kind <kind>`, then `vowp agent pair
  --code-file - --surface <new-id> --session <id>`.
- `vowp surface pair --replace <surface-id>` *(planned)* is the one-command form: it revokes the old
  surface and creates the new pending surface in one transaction, so the owner does not have to run
  two commands. A `--replace` target must belong to the same node and session and must not be
  already revoked; the old surface's queued frames and cached reads are removed.
- A pending spool for a surface whose token died is discarded with a visible notice, following the
  existing takeover rule; it is never appended with a dead token. A run's spool is separate and is
  not discarded by a surface re-pair.

### 4.6 Failure modes

| Failure | Home | Agent | Owner sees |
|---|---|---|---|
| Node offline at rotate | queued; old token expires after the grace | receives on next connection; no write while offline | `PENDING` in `surface ls`; warning |
| Node offline past the replacement expiry | frame discarded; surface `expired` | `needs_pair` on next refusal | re-pair instructions |
| Agent disk full during token write | rotation already shortened the old token | atomic write fails; old token remains on disk, dead | `needs_pair`; free space and re-pair |
| Frame write fails mid-drain | frame stays queued | next connection retries | `PENDING` |
| Rotate a pending surface | exit 4, no token minted | — | clear error |
| Rotate a revoked surface | exit 5, no token minted | — | clear error |
| Rotation after a revoke | refused; queued issued frame removed | `SurfaceRevoked` deletes the token | surface revoked |
| Second rotation before delivery | replaces the queued frame | receives the newest token only | one pending frame |
| Token refused on a chat/MCP write | — | `needs_pair`; no retry loop | re-pair instructions |

### 4.7 Files, tests and non-slice

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** the offline/wait/grace semantics; the `spool.rs` envelope that carries `queued_at`/`overlap_until` (the frame and its schema are unchanged); the prefix/expiry display; the `surfaces` status array; the `node_ledger` rows for rotate and revoke (the pairing-issue row is 4d's `gateway.rs`); the `needs_pair` state and `--replace` pairing. **Files:** `crates/vowp-core/src/agent/pair.rs`, `crates/vowp-core/src/agent/spool.rs` (created by `v4-local-4a-surface-rotate`), `crates/vowp-core/src/agent/mod.rs`, `crates/vowp-store/src/repo_surfaces.rs`, `crates/vowp-agent/src/surface_state.rs` *(new)*, `crates/vowp-agent/src/status.rs`, `crates/vowp-agent/src/client.rs` (the refusal path only), `crates/vowp-cli/src/cmd/surface.rs`, `crates/vowp-core/tests/agent.rs`, `crates/vowp-agent/tests/surface_pair.rs`, `crates/vowp-cli/tests/agent.rs`. | `surface_token_rotation_keeps_the_old_token_for_the_overlap` (§14.3; already in the slice), `surface_rotate_queues_for_an_offline_node_and_warns` (already), `surface_rotate_wait_returns_when_the_frame_is_drained` (new), `surface_rotate_grace_extends_the_old_token_but_never_past_the_cap` (new), `surface_ls_prints_the_prefix_and_expiry_but_never_the_token` (new), `surface_rotate_writes_one_ledger_note_without_the_token` (new), `agent_marks_a_refused_surface_needs_pair_and_stops_retrying` (new), `surface_pair_replace_revokes_the_old_surface_and_queues_no_stale_frame` (new). | Changing scopes on rotation; multiple replacement tokens at once; exporting or importing a token; a token longer than 30 days; rotating a node key; a browser UI. |

The carrier for `queued_at`/`overlap_until` is the `QueuedAgentFrame` envelope in
`crates/vowp-core/src/agent/spool.rs`, not `AgentFrame::SurfaceIssued`; the frame and its schema stay
unchanged. `v4-local-4c-rotation-ux` owns the two ledger-aware call sites and the in-file test call
in `crates/vowp-cli/src/cmd/surface.rs`; the flags and columns remain
`v4-local-4f-cli-surface`'s.

## 5. Bundle result

### 5.1 Goal

Let a node return a run's commits without push access. Today `result_transport = "push"` requires
the node's local `origin` to match the project remote and to accept a push; a read-only node is
ineligible. A git-bundle result returns `base..head` over the authenticated agent connection, so
the node needs no git credential and the home still verifies, gates and merges. It is the phase-4
option promised in §6.4 of the accepted note; `ResultTransport::Bundle` already exists as a config
value and is refused by validation.

### 5.2 Wire contract

- `[offload] result_transport = "bundle"` becomes legal; `push` stays the default and the two are
  mutually exclusive per assignment. `AssignmentPayload` in `crates/vowp-protocol/src/offload.rs`
  gains `result_transport: ResultTransport` with `#[serde(default)]` = `push`, so a newer home and
  an older agent still agree on the default.
- A node is eligible for bundle when the project is mapped, the base commit is fetchable and the
  path map is valid; the push-remote match is **not** required and the node needs no push access.
  Read access to the base commit is still required: a node with no fetch access is not eligible. A
  push run keeps the existing remote check. The node still never pushes, tags, merges or deploys.
- New frames, all carrying `run_id` and `epoch`:

| Frame | Direction | Payload |
|---|---|---|
| `BundleStart` | agent → home | `run_id`, `epoch`, `base`, `head`, `branch` (`job/<id>`), `bytes`, `sha256` |
| `BundleAccept` | home → agent | `run_id`, `epoch`, `offset` (bytes already stored) |
| `BundleChunk` | agent → home | `run_id`, `epoch`, `offset`, `data` (hex; at most 256 KiB raw per chunk) |
| `BundleEnd` | agent → home | `run_id`, `epoch`, `bytes`, `sha256` |
| `BundleAbort` | both | `run_id`, `epoch`, `reason` |

- `BundleChunk.data` is lowercase hex, not base64, so no new dependency is added and a 256 KiB
  chunk stays well inside the 1 MiB frame cap. A chunk's `offset` must equal the stored length; a
  gap, overlap or oversized chunk aborts the bundle. The home hashes every byte as it writes.
- The bundle contains exactly one ref, `refs/heads/job/<id>`, with `base` as its prerequisite. The
  agent creates it with `git bundle create <file> <base>..job/<id>` in its job worktree; no tags,
  no extra refs, no `--all`. The worktree may be dirty, but only committed objects are in the
  bundle; uncommitted files are not returned. The home refuses a bundle that `git bundle
  list-heads` shows more than the one expected ref.
- Caps: 256 MiB per bundle, a 5-minute transfer deadline, 256 KiB chunks and a per-run staging
  file. A `BundleStart` with the same `(run_id, epoch, sha256, bytes)` resumes from the returned
  offset; a higher epoch supersedes; a lower epoch is refused. A disconnect keeps the partial file
  for the grace period; a fenced run removes it.

### 5.3 Home apply and verification

1. The home writes the bundle to `<home>/tmp/agent-bundles/<run>/<random>.bundle`, 0600 in a 0700
   directory, while hashing. A size over the cap or a sha mismatch removes the file and fails the
   run with `bundle_failed`.
2. `git bundle verify` runs in the project repository, which must already have the base commit.
   `git bundle list-heads` must name `refs/heads/job/<id>` at the reported `head`; any other ref,
   tag or head is `bundle_rejected`.
3. The home fetches only `refs/heads/job/<id>:refs/vowp/jobs/<id>` from the bundle file into a
   private staging bare repository that already has the base commit (initialized from the home
   repository or given the base from it), never into the main checkout's working tree. It checks
   that the fetched head equals the reported head and descends from the assigned base. A mismatch
   is `bundle_rejected`; nothing is merged and nothing enters the project repository. After a pass,
   the verified head object exists only in the staging repository.
4. Only after step 3 passes, the home transfers that verified head into the project repository's
   object database before the job-branch ref update: `git fetch --no-tags <staging-repo>
   +refs/vowp/jobs/<id>:refs/vowp/jobs/<id>` in the project checkout. This local fetch copies the
   head's objects and creates the inspection ref only; it does not touch the working tree or any
   branch. It is the object transfer that makes `<head>` resolvable in the project repository;
   without it the existing `git branch -f` would fail on an unknown object. The existing
   `crates/vowp-core/src/agent/result.rs` path then points the local `job/<id>` branch at the
   verified head through the existing `git_ref_update` helper (`git branch -f job/<id> <head>`,
   which now resolves because the object is present), creates its staging worktree, runs the
   authoritative `owns_respected` and `secret_scan` gates on `base..head`, and hands the job to the
   normal merge queue. The staging file and staging repo are removed after the result is settled.
   A bundle that fails steps 1–3 leaves no ref in the project repository and no staging file after
   settle; it stays retryable. Once step 4 imports the inspection ref, a gate failure leaves that
   ref for inspection and does not merge; the job stays retryable.
5. The run is `finishing` from `BundleEnd` until the gates settle. `TurnEnd` still carries the
   terminal summary; if it arrives before the bundle is applied, the home records it but does not
   finish the run until the bundle result is settled. A summary without a complete bundle is a
   `bundle_failed` run, not a successful one.

The same frame path works over SSH stdio and WSS; the bundle is a payload of the authenticated
agent channel, not a new listener or a public upload.

### 5.4 What an offloaded run returns beyond the pushed branch

- **The git bundle** is the returned work: one ref, `base..head`, verified before it touches the
  project repository. It is not a session bundle and is never appended to a session event.
- **No session bundle.** The run's events already arrived live through `IngestSession` and the
  home owns the one log. Returning a `vowp-bundle-v1` session bundle would either fork the log or
  duplicate it; a missing event is a spool gap, not something a second session copy may fill. The
  existing bundle export/import stays a cross-node session move, not a run result.
- **The terminal summary** is unchanged: `TurnEnd` carries exit code, outcome, native session id,
  cost, limit, final text, stop reason and agent-side usage. The home writes the run row, cost rows
  and ledger row as it does for a push run.
- **The durable result record** in the per-run `kv` entry gains `bundle_bytes` and `bundle_sha256`
  beside `transport = "bundle"`, `base_commit` and `head_commit`. The record is path-free and
  contains no token or secret.
- **Redaction** runs on the summary and result text exactly as today; the bundle bytes are binary
  git objects and are never logged, mirrored or stored in a session event. The `secret_scan` gate
  is the backstop for a secret committed into the bundle, as it is for a push.

### 5.5 Failure cases

| Failure | Result |
|---|---|
| Node has no push access, `result_transport = "bundle"` | eligible; no remote match required |
| `base` missing on the home or not a prerequisite of the bundle | `bundle_rejected`; no project ref, no merge; staging removed after settle; job retryable |
| `head` not the reported head, not a descendant of base | `bundle_rejected`; no project ref; staging removed after settle; job retryable |
| Bundle has a second ref, a tag or a different branch | `bundle_rejected`; no project ref; file removed |
| sha256/size mismatch | `bundle_failed`; file removed; job retryable |
| Over the 256 MiB cap | `bundle_failed` before the file is staged; no partial ref |
| Disconnect mid-transfer | partial file kept for the grace; resume from `BundleAccept.offset`; grace expiry removes it |
| Same run, higher epoch | old partial removed; new transfer accepted |
| Same run, lower epoch | refused; no bytes written |
| Dirty worktree or no committed change | the bundle returns commits only; uncommitted files are not returned and the run fails `result_empty` when `base == head` |
| Gate failure | run fails the gate, no merge; the imported `refs/vowp/jobs/<id>` stays for inspection; job retryable |
| Home disk full | `bundle_failed`; staging file removed; job retryable |
| `result_transport` changed mid-run | the assignment's value wins; no fallback in either direction |

The split is the one rejection contract. A failure in steps 1–3 (size/sha, `git bundle verify`, the
one-ref check, head/base ancestry) leaves no project ref and no staging file after settle. Only a
bundle that passes the step-4 import and then fails a gate leaves the inspection
`refs/vowp/jobs/<id>`; it never merges. §5.3 and §8.2 state the same split.

### 5.6 Security property

The bundle is untrusted bytes. The home verifies the hash and size, `git bundle verify`, the exact
head and ref set, and the base ancestry **before** it fetches into a private staging repo; after
the staging check and the verified-ref import, it runs the same gates. A bundle cannot add a ref
outside the job, a tag, a hook or a worktree file; it cannot execute code during verification. The
channel is the already-authenticated SSH or WSS connection; there is no new public upload route and
no credential on the node. A bundle-mode node that later gains push access does not silently
switch; the owner changes the policy.

### 5.7 Files, tests and non-slice

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** the five bundle frames; `ResultTransport::Bundle` validation; the assignment field on `AssignmentPayload` in `offload.rs`; the `BundleAccept`/`BundleAbort` arms in the agent serve loop in `client.rs`, ahead of the wildcard; the agent-side bundle creation, chunking, resume and abort; the home staging, verification and result hand-off; the bundle fields in the result record; bundle eligibility in the policy. **Files:** `crates/vowp-protocol/src/agent.rs`, `crates/vowp-protocol/src/offload.rs`, `crates/vowp-protocol/src/config.rs`, `crates/vowp-protocol/tests/agent.rs`, `crates/vowp-protocol/tests/offload.rs`, `crates/vowp-protocol/tests/config.rs`, `crates/vowp-store/tests/offload.rs`, `crates/vowp-core/src/agent/gateway.rs`, `crates/vowp-core/src/agent/result.rs`, `crates/vowp-core/src/agent/dispatch.rs`, `crates/vowp-core/src/offload/policy.rs`, `crates/vowp-core/tests/offload_result.rs`, `crates/vowp-core/tests/offload_e2e.rs`, `crates/vowp-agent/src/client.rs`, `crates/vowp-agent/src/executor.rs`, `crates/vowp-agent/src/worktree.rs`, `crates/vowp-agent/tests/executor.rs`, `crates/vowp-cli/tests/agent.rs`, `crates/vowp-cli/tests/offload.rs`. | `offloaded_run_returns_a_bundle_without_push_access` (new), `bundle_result_is_verified_by_size_sha256_and_bundle_verify` (new), `bundle_result_rejects_a_foreign_ref_or_head` (new), `bundle_result_rejects_a_base_that_is_not_a_prerequisite` (new), `bundle_result_resumes_after_a_disconnect` (new), `bundle_result_over_the_cap_removes_the_staging_file` (new), `bundle_result_manifest_and_run_row_carry_no_absolute_path_or_secret` (new), `offload_refuses_a_job_that_needs_a_home_only_secret` (§14.3; unchanged), `offloaded_run_pushes_the_job_branch_and_the_home_verifies_the_head` (§14.3; the push path must stay green). | A push fallback; a non-git transport; Git LFS, submodules or tags; signed bundles; partial-file resume across a home restart beyond the grace; a node-side merge; a session bundle as a run result; bundle mutation after `BundleEnd`. |

`AssignmentPayload` is defined in `crates/vowp-protocol/src/offload.rs`, so 4d owns that file, its
protocol/store test literals, and the `AssignmentPayload` literals in
`crates/vowp-cli/tests/agent.rs` and `crates/vowp-cli/tests/offload.rs`; 4f keeps the rest of those
CLI tests. `agent.rs` carries the five new frames and `config.rs` only the bundle config value. The
`BundleAccept`/`BundleAbort` arms go into the `client.rs` serve-loop `match frame` ahead of its
`_ => {}` wildcard, so a home-to-agent bundle answer is never silently ignored; 4b owns the same
file for transport selection and 4d re-opens it after 4b merges.

## 6. Separate user

### 6.1 Supported shape

Running the agent as a dedicated **standard** user is supported on both platforms. The shape is:

- one node name and one node key per account; a second account on the same machine is a second
  node, not a second role on the same node;
- the agent's state directory, project clone, worktrees, harness login and provider environment
  belong to that user;
- the owner's account keeps the home SSH key, the home checkout and the home secrets; none of them
  is shared with the agent user;
- the agent is enrolled with `vowp agent enroll` run **as that user** (for example through
  `sudo -iu <agent-user> -- vowp agent enroll …`), and the service is installed **as that user**;
  VOWP itself never calls `sudo`, never changes uid and never writes another user's home.

The home cannot tell whether the account is the owner's or a dedicated one: it sees a node name, a
node key, a path map and heartbeat capacity. That is the intended boundary. The dedicated account
is a filesystem and process boundary for the owner's files; it is not a sandbox for repository code
(§6.5).

### 6.2 File ownership rules

- `<state_dir>` is 0700, owned by the agent uid. `agent.json`, `node.key`, `node.pub`,
  `status.json`, `agent.sock`, the `surfaces/<id>/token` files, the spool and the cache are 0600
  (the socket 0600); directories below the state root are 0700. The existing helpers already set
  these modes; the service and run paths must **check** them, not only create them.
- A file or directory owned by another uid, or a state directory with group/world permission, is a
  refusal (exit 5) with a message that names the path and the expected owner/mode. The check reads
  `std::os::unix::fs::MetadataExt`; it never follows a symlink out of the state root.
- The project clone and every worktree are owned by the agent uid. The agent never writes into the
  owner's checkout. Git's `safe.directory` is not a fix for a shared checkout; a shared working
  tree is unsupported (§6.5).
- A shared read-only project mirror may be group-readable for the agent to fetch, but the agent's
  clone and worktrees are private. A project checkout, worktree root or git directory that is owned
  by another uid or is group- or world-writable is refused with `WorktreeError::UnsafeCheckout`,
  before a run is assigned or started. The check runs when `PathMap::new` builds the map and again
  at run start in `WorktreeManager::create`, before it inspects, fetches or creates anything; the
  git directory is checked there after `common_dir()` resolves it. Named tests:
  `path_map_refuses_a_group_writable_checkout`,
  `path_map_refuses_a_world_writable_worktree_root`,
  `path_map_accepts_a_group_readable_private_checkout`,
  `worktree_create_rechecks_the_checkout_before_creating` and
  `worktree_create_refuses_a_group_writable_git_dir` in
  [`crates/vowp-agent/tests/worktree.rs`](../../../crates/vowp-agent/tests/worktree.rs).
- Provider and git credentials for the agent user live in that user's environment or Keychain and
  are never copied from the owner. The home sends names, never values, exactly as today.

### 6.3 Harness logins and secrets

The harness runs as the agent user. It can read that user's `node.key` and surface tokens if it
tries; the separate user protects the **owner's** `~/.ssh`, provider login and files, not the
agent's own secrets from a harness it runs. State that honestly. The supported rule is: the agent
user has its own harness login, its own provider key and its own git credential; a repo that needs
a home-only secret is not offloaded. `vowp agent doctor` *(planned, CLI slice)* checks by name that
the state directory contains no copied `~/.ssh`, `~/.claude`, opencode auth file or `.env`, and
reports presence by name only.

### 6.4 macOS and Linux notes

- **macOS.** A dedicated *standard* user is a `LaunchAgent` in that user's GUI session. The user
  must log in once after a reboot, or the owner bootstraps the job in that user's GUI domain. A root
  `LaunchDaemon` in the system LaunchDaemons directory is explicitly unsupported: it runs as root,
  needs a system-wide install and contradicts the no-root rule. FileVault and a separate volume are
  the owner's responsibility; VOWP does not encrypt the state.
- **Linux.** `systemd --user` for the dedicated user, plus `loginctl enable-linger <user>` so the
  unit survives logout. VOWP prints the linger command and does not run it with `sudo`. A
  system-wide unit is unsupported for the same reason.
- **Service names.** The user unit is `vowp-agent.service`; the launchd label is `dev.vowp.agent`.
  Two agent accounts on one machine each have their own user session and their own unit file, so
  the names can be identical; two state directories for one account are unsupported.

### 6.5 Explicitly unsupported

- A sandbox or a security boundary for untrusted repository code. The harness runs as the agent
  user and can read everything that user can read, including the node key and surface tokens.
- Per-project users, a uid per job, or two agents writing one checkout.
- Running as root, a system-wide daemon, or a `sudo` helper for the agent.
- Sharing one `~/.vowp-agent` between users or between machines.
- Multi-user VOWP accounts, roles or per-user quotas. The home still has one owner.
- Windows.
- Protecting the owner's files from a malicious harness that runs as the **owner**; only a
  separate user or VM changes that, and VOWP does not claim a VM.

### 6.6 Files, tests and non-slice

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** the ownership/mode/root checks used by `agent run` and `agent service install`; the unit text for a user account; the documented `sudo -iu` enrollment and linger commands; an `agent doctor` check that reports copied logins by name. **Files:** `crates/vowp-agent/src/isolation.rs` *(new)*, `crates/vowp-agent/src/service.rs` *(new)*, `crates/vowp-agent/src/client.rs`, `crates/vowp-agent/tests/service.rs` *(new)*, `crates/vowp-cli/src/cmd/agent.rs`, `crates/vowp-cli/tests/agent.rs`, `docs/guides/agent-node.md` *(new, docs slice)*. | `agent_refuses_to_run_as_root` (§14.3; pure uid predicate), `agent_state_dir_refuses_a_foreign_owner_or_loose_mode` (new), `agent_service_install_writes_a_unit_for_the_current_user_only` (new), `agent_doctor_reports_a_copied_login_by_name_only` (new). | A sandbox; a VM; per-project users; root/system-wide installs; multi-user accounts; Windows; encrypted state. |

## 7. Metrics history

### 7.1 Goal and scope

The home already measures capacity every machine tick and keeps cost in the ledger, but the only
capacity read is "latest sample": `vowp capacity report --since` filters which latest sample it
accepts, not a history. The open follow-up is a bounded local history so the owner can see memory,
load, disk and spend over days or weeks without adding Prometheus or a hosted service. This slice
adds the home's capacity history and reuses the existing cost ledger; it does not add per-node
history, alerts, budgets or a public endpoint.

### 7.2 Storage and retention

The global schema goes to version 7 with one additive table:

```sql
CREATE TABLE capacity_rollups (
  bucket                  TEXT NOT NULL,      -- RFC 3339 UTC bucket start
  bucket_secs             INTEGER NOT NULL,   -- 300 (5 minutes)
  samples                 INTEGER NOT NULL,
  mem_available_min_bytes INTEGER NOT NULL,
  mem_available_avg_bytes INTEGER NOT NULL,
  mem_available_max_bytes INTEGER NOT NULL,
  load1_avg               REAL NOT NULL,
  disk_free_home_min_bytes INTEGER NOT NULL,
  source                  TEXT NOT NULL CHECK (source IN ('proc','fallback','mixed')),
  PRIMARY KEY (bucket, bucket_secs)
) STRICT;
CREATE INDEX capacity_rollups_bucket ON capacity_rollups(bucket);
```

- The v6-to-v7 migration lives in `crates/vowp-store/src/migrate.rs`, next to the v5-to-v6 path:
  `migrate_v06` copies the global file to `backups/pre-0.7-<ms>.db`, creates `capacity_rollups` and
  its index in one immediate transaction, checks the existing table counts, integrity and foreign
  keys, and commits `user_version = 7`; any failure rolls back and leaves the v6 file readable.
  `migrate_v06_if_needed` is called from `Store::open_global_unbound` in
  `crates/vowp-store/src/db.rs`, in the same non-deferred sequence as
  `migrate_v05_if_needed`; `Store::open` and `Store::open_global` reach it through that shared
  path. `Store::open_writable` remains the no-create/no-ensure/no-migrate path and does not call
  it.
- The rollup writer runs on the machine tick after the host sample. It aggregates the **previous
  completed** 5-minute bucket from `host_samples`, then writes one row with
  `INSERT … ON CONFLICT(bucket, bucket_secs) DO UPDATE`, so a repeated tick, a restarted
  supervisor or a deferred migration cannot duplicate or lose a bucket. `samples` is the number of
  raw rows in the bucket; a bucket with no samples is skipped, not written as zero.
- `source` is `proc` when every raw sample in the bucket is `proc`, `fallback` when every sample is
  `fallback`, and `mixed` otherwise. It is the only per-bucket provenance field; the raw rows keep
  their own source.
- Retention: raw `host_samples` are kept for **48 hours** and `capacity_rollups` for **30 days**.
  The current pruner deletes rows older than 24 hours when `interval_secs < 60`, keeps
  `interval_secs >= 60` rows until 30 days, and deletes everything older than 30 days
  (`prune_host_samples`); the 5-second sampler writes only `interval_secs = 5` rows, so the 30-day
  arm currently has nothing to keep. The new pruner deletes raw rows older than 48 hours and
  rollups older than 30 days, and never deletes the bucket that contains `now`. The row budget is
  about **34,560** raw rows (`48 h × 3,600 / 5 s`) and 8,640 rollups. At the current row size that
  is roughly 5.5 MiB of raw rows plus 1.2 MiB of rollups (about 7 MiB together with the indexes),
  so the history is bounded by construction.
- Rollups live in the global DB because capacity is node-wide; cost stays in the project and node
  ledgers. No project DB schema changes; the project version stays 2.
- Missing `/proc` (macOS or a minimal container) still produces `fallback` samples; the rollup
  records their zeroed memory/load values and their real disk free, and the read surface labels the
  bucket `fallback` rather than pretending it measured. A gap (supervisor stopped) is a gap, not a
  zero.

### 7.3 Read-only surface

```sh
vowp capacity history [--since <duration|date>] [--bucket 5m|1h|1d] [--json]   # (planned)
```

- `--since` defaults to `24h`; `--bucket` defaults to `5m`. `1h` and `1d` are read-time
  aggregations of the 5-minute rows, weighted by their `samples` count; they are not additional
  stored tables. A read-time bucket's `mem_available.min`/`max` are the extrema of its 5-minute
  points, and `source` is `mixed` when its points disagree.
- The JSON document is `CapacityHistory` (exported from `vowp_protocol::capacity` and registered
  by a hand-written `schemas.insert("CapacityHistory", …)` in `schema::all_schemas` in
  `crates/vowp-protocol/src/schema.rs`; 4e owns that file and `crates/vowp-protocol/tests/schema.rs`):

```json
{
  "generated_at": "<rfc3339>",
  "since": "<rfc3339>",
  "bucket_secs": 300,
  "points": [
    {
      "at": "<rfc3339>",
      "samples": 5,
      "mem_available": {"min": 0, "avg": 0, "max": 0},
      "load1_avg": 0.0,
      "disk_free_home_min": 0,
      "source": "proc"
    }
  ]
}
```

The illustrative zeroes stand for byte counts; the real document never invents a measurement for a
missing bucket. The human output is one table row per point: `AT SAMPLES MEM_MIN/AVG/MAX LOAD1
DISK_MIN SOURCE`. No path, token or secret appears.
- The command is home-only like `vowp capacity report` and opens the home with
  `Store::open_read_only`; it writes no target, advice, ledger or rollup row. It is safe to run
  while the supervisor runs. A missing table on an unmigrated home is the ordinary "migrate first"
  error, not a panic.
- The daemon does not gain a route in this slice. The v0.3 daemon is a per-project read API on
  loopback; a node-wide capacity route is a later API version and would need its own schema export.
- Cost is not duplicated. `vowp cost --by day` already aggregates the ledger by day, and the
  history command points at it in its human footer. A combined capacity-and-cost chart is a UI
  concern, not a new store.

### 7.4 Failure modes

| Failure | Behavior |
|---|---|
| Supervisor stopped for a day | a gap in points, not a zero; the range still resolves |
| Clock jump | buckets are keyed by UTC start; a jump creates a new bucket, never rewrites an old one |
| Repeated tick / restart | the `ON CONFLICT` update is idempotent; `samples` is recomputed from raw rows |
| Raw samples pruned before the rollup | the bucket is skipped; the next tick cannot reconstruct it, and the gap is visible |
| `--since` before the retention window | clamped to the oldest retained bucket; a note says so |
| Unknown `--bucket` | exit 2 with the accepted values |
| Unmigrated home | the ordinary store version error; no partial table |
| `fallback` sample source | the point is labelled `fallback`; memory/load zeroes are not a measurement |
| Read while a migration runs | the live-supervisor deferral from the live-upgrade design applies; no lock is taken |

### 7.5 Files, tests and non-slice

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** global schema v7 `capacity_rollups` with the v6-to-v7 migration and backup in `migrate.rs`, called from `Store::open_global_unbound` in `db.rs`; the rollup writer and pruner; the `CapacityHistory` type and schema registration; `vowp capacity history` human/JSON output. **Files:** `crates/vowp-store/src/schema/global.sql`, `crates/vowp-store/src/db.rs`, `crates/vowp-store/src/migrate.rs`, `crates/vowp-store/src/repo_capacity.rs`, `crates/vowp-store/tests/migrate.rs`, `crates/vowp-store/tests/metrics.rs` *(new)*, `crates/vowp-core/src/capacity/sampler.rs`, `crates/vowp-core/src/capacity/history.rs` *(new)*, `crates/vowp-core/src/capacity/mod.rs`, `crates/vowp-core/tests/capacity_measure.rs`, `crates/vowp-protocol/src/capacity.rs`, `crates/vowp-protocol/src/schema.rs`, `crates/vowp-protocol/tests/schema.rs`, `crates/vowp-cli/src/cli.rs`, `crates/vowp-cli/src/cmd/capacity.rs`, `crates/vowp-cli/tests/capacity.rs`. | `capacity_rollups_migrates_v6_to_v7_with_a_backup` (new), `capacity_history_buckets_raw_samples_into_five_minute_points` (new), `capacity_history_is_idempotent_across_a_repeated_tick` (new), `capacity_history_keeps_thirty_days_and_prunes_the_rest` (new), `capacity_history_clamps_since_to_the_retained_window` (new), `capacity_history_json_matches_the_exported_schema` (new), `capacity_history_is_read_only_and_writes_no_ledger_row` (new), `sampler_prunes_old_rows` (existing; keep green with the new 48-hour window), `sampler_degrades_on_non_linux` (existing; keep green with the fallback rollup). | Alerts and notifications; per-node history; Prometheus/OpenTelemetry export; configurable retention; cost budgets; a daemon route; long-term archive; a chart UI. |

## 8. Docs and release gates

The evidence record is [`../../releases/v0.4-hardening.md`](../../releases/v0.4-hardening.md): it
carries the §8.2 checklist with code and test links, the §8.4 drill commands and results, and the
§8.3 owner record. The owner live Mac smoke test passed on 2026-11-18.

### 8.1 Documentation set

The phase is not complete with code only. The docs slice owns:

- a new `docs/guides/agent-node.md` (install, enroll, service, separate user, rotation, WSS
  deployment, bundle, metrics) and links from `docs/guides/README.md`, `docs/guides/multi-node.md`
  and `docs/guides/install.md`;
- `docs/security.md` for the WSS trust boundary, the service files, the separate-user limits and
  the bundle verification; the hardening checklist gains the corresponding rows;
- `docs/design/crates/vowp-agent.md`, `vowp-core.md`, `vowp-store.md`, `vowp-cli.md`,
  `docs/design/cli.md`, `docs/design/types.md` (the new frames and `CapacityHistory`),
  `docs/design/schema/global.sql` and the generated `docs/reference/` pages;
- this page's status, the [`docs/design/README.md`](../README.md) status and the
  [`docs/roadmap.md`](../../roadmap.md) phase-4 row, with no date.

Every command in the guide must exist in the tree at that point or be marked *(planned)*. The
reference pages are regenerated, not hand-edited.

### 8.2 Security review

The phase exit requires a security review with evidence in the report:

1. WSS: only `/agent/v1`, only loopback, only the `vowp-agent/1` subprotocol; the node signature,
   nonce and connection id are the same; a changed certificate fingerprint requires an explicit
   owner action; no token, code or key in a URL, header, log or error. The review states that the
   TLS-terminating proxy is a trusted home component that can read frames and tokens, and that its
   logs carry no frame bodies.
2. Service: the unit contains no secret; the files are user-owned 0600; the agent refuses root and
   a foreign/loose state directory; the drain does not drop a spool.
3. Rotation: the home `surface_tokens` rows hold only a hash, prefix and scopes; the two plaintext
   copies are the queued `SurfaceIssued` JSON frame in
   `agent-frames/<node>/<request>.json` (0600 in a 0700 directory, removed after delivery) and the
   agent's `surfaces/<id>/token` (also plaintext, 0600); the overlap is bounded; revocation removes
   a queued token; the ledger row has no token or hash.
4. Bundle: hash, size, `git bundle verify`, one ref, base ancestry and gates run before a ref
   update; a bundle that fails verification leaves no project ref, no merge and no staging file
   after settle. A post-import gate failure leaves only the inspection
   `refs/vowp/jobs/<id>`, never a merge.
5. Separate user: the documented shape does not claim a sandbox; the shared-checkout case is
   refused; the harness's access to the agent's own key is stated.
6. Redaction: the new status fields, ledger detail and bundle manifest carry names, prefixes,
   counts and timestamps only.

The review is a checklist with links to the named tests and the code; an unresolved item is a
`questions_for_owner` entry, not a silent waiver.

### 8.3 Owner live Mac smoke test

The phase exit records one smoke test, run once on the owner's Mac against the owner's home:

1. pair or start one local orchestrator;
2. complete one no-op turn and verify the same VPS session log;
3. stop and clean up the test orchestrator.

Status is **passed**: the owner ran the test on 2026-11-18 and recorded it on the evidence page.
There is no multi-day run or daily checklist.

### 8.4 Rollback drill

1. Stop the agent service (launchd `bootout` or `systemctl --user disable --now`); confirm no agent
   process and an inspectable spool.
2. `vowp down --drain`; wait for active runs and gates to reach zero; confirm the supervisor lock is
   released.
3. `vowp node revoke <name>` for each node; confirm runs are fenced and leases released.
4. Turn off WSS in the proxy and stop the loopback listener; turn off offload.
5. Restore the pre-phase backup into a staging directory, verify hashes and row counts, and swap it
   into a **copy** of the home; run the previous binary against the copy. The v7 table is additive
   in the migration, but a previous binary refuses a file whose `user_version` is higher than it
   supports, so the drill runs against the restored v6 copy, not the migrated live file. The
   rollback is tested on the copy, never on the live home.
6. Re-enable the v0.3 home-only flows and run their tests. Record the commands and results.

### 8.5 Release gates

| Gate | Evidence |
|---|---|
| `report` | the slice's report has `status: done` and the commands below |
| `owns_respected` | `git diff --name-only <base>..HEAD` lists only the slice's `owns` |
| `secret_scan` | no secret value, token, key or `.env` in the diff; only names and placeholders |
| `review` | an independent verifier reads the diff and this design |
| `bash docs/tests/run.sh` | links and documented commands pass |
| `python3 docs/reference/check-reference.py` | the CLI and schema pages match the constants |
| `cargo fmt --all -- --check` | formatting |
| `cargo clippy --workspace --all-targets -- -D warnings` | no warnings |
| `cargo test --workspace` | every suite passes |
| Security review | §8.2 checklist with test links |
| Owner live Mac smoke test | §8.3 smoke-test record; passed on 2026-11-18 |
| Rollback drill | §8.4 commands and results on the copy |

The phase itself performs no publish, deploy, release tag or visibility change; 0.4.0 packaging
is a separate release job after the gates pass.

### 8.6 Files, tests and non-slice

| Ship | Test | Not in this slice |
|---|---|---|
| **Ship:** the guide, the security page, the crate/design pages, the regenerated reference, the roadmap/README status, and a report with the smoke-test/rollback evidence. **Files:** `docs/guides/agent-node.md` *(new)*, `docs/guides/README.md`, `docs/guides/install.md`, `docs/guides/multi-node.md`, `docs/guides/operating.md`, `docs/security.md`, `docs/design/crates/vowp-agent.md`, `docs/design/crates/vowp-core.md`, `docs/design/crates/vowp-store.md`, `docs/design/crates/vowp-cli.md`, `docs/design/cli.md`, `docs/design/types.md`, `docs/design/schema/global.sql`, `docs/reference/cli.md` and the schema pages (regenerated), `docs/design/v0.4/hardening.md`, `docs/design/README.md`, `docs/roadmap.md`, `CHANGELOG.md`. | `bash docs/tests/run.sh`, `python3 docs/reference/check-reference.py` and the manual review/smoke test/drill; no new unit test is added by a docs-only change. | A release, tag or announcement; a public claim; a hosted relay; a date. |

## 9. Ordered slice list

Each slice is one worker and one review round. No two concurrently runnable slices write the same
path; the dependencies make the CLI and docs slices last. The library order is 4b → 4d, with 4c and
4e independent: 4d re-opens `crates/vowp-agent/src/client.rs` and the protocol
`config.rs`/`tests/config.rs` after 4b merges, to add the bundle dispatch and the bundle config
value. A slice's `owns_respected` gate compares its diff against its dependency's merged base, so a
re-opened path shows only that slice's change. The Files lists in §§2–8 name every file an item
touches across the phase. Where a file is under `crates/vowp-cli/**`, it belongs to
`v4-local-4f-cli-surface`, with four literal-only exceptions: `crates/vowp-cli/src/setup/commands.rs`
(4b, the `NodeFile` literal), `crates/vowp-cli/src/cmd/surface.rs` (4c, the two
`revoke_surface`/`rotate_surface_token` call sites and their in-file test call), and
`crates/vowp-cli/tests/agent.rs` plus `crates/vowp-cli/tests/offload.rs` (4b and 4d, only the
`AgentStatus` and `AssignmentPayload` literals named above). The library slice that an item also
names owns only its library paths. All slices depend on `v4-local-4a-surface-rotate` being merged
where they touch the surface outbox, and on the current `main` otherwise. The ids are proposals;
the supervisor's graph is authoritative.

### 9.1 `v4-local-4b-node-runtime`

**Scope.** Make the node deployable: the optional WSS transport and loopback listener behind a
proxy, the `[ingress.wss]` table on protocol `NodeFile`, the launchd/systemd-user service install
and uninstall, the `--service` run mode with the `run` marker and the drain, the ownership/root
checks that a separate user relies on, and the agent-side surface state that turns a refused token
into `needs_pair` with the new status fields. It also updates the one full `NodeFile` literal in
`crates/vowp-cli/src/setup/commands.rs`. The transport, service and surface-state modules are new;
no store or session semantics change.

**Owns.** `crates/vowp-protocol/src/config.rs`, `crates/vowp-protocol/tests/config.rs`,
`crates/vowp-agent/src/client.rs`, `crates/vowp-agent/src/config.rs`,
`crates/vowp-agent/src/isolation.rs` *(new)*, `crates/vowp-agent/src/lib.rs`,
`crates/vowp-agent/src/service.rs` *(new)*, `crates/vowp-agent/src/status.rs`,
`crates/vowp-agent/src/surface_state.rs` *(new)*, `crates/vowp-agent/src/transport.rs`,
`crates/vowp-agent/src/wss.rs` *(new)*, `crates/vowp-agent/Cargo.toml`,
`crates/vowp-agent/tests/service.rs` *(new)*, `crates/vowp-agent/tests/surface_pair.rs`,
`crates/vowp-agent/tests/transport.rs` *(new)*, `crates/vowp-cli/src/setup/commands.rs`,
`crates/vowp-cli/tests/agent.rs` (the two `AgentStatus` literals only).

The setup file is 4b's only CLI source path and the edit is limited to the `NodeFile` literal; 4b
also updates the two `AgentStatus` literals in `crates/vowp-cli/tests/agent.rs` for the new
`surfaces` field, and only those literals. `crates/vowp-cli/tests/core.rs` uses
`..NodeFile::default()` and is unchanged.

**Depends on.** Nothing beyond the current `main`; the CLI wiring lands separately.

### 9.2 `v4-local-4c-rotation-ux`

**Scope.** Finish the home side of rotation after `v4-local-4a-surface-rotate`: the `--grace` and
`--wait` policy (the flags themselves are CLI), the `spool.rs` envelope that carries
`queued_at`/`overlap_until` and the delivery deadline, the home's prefix/expiry read data, the
`node_ledger` rows for rotate and revoke, and the `--replace` pairing path. The token mechanics,
overlap and outbox already exist; this slice adds the offline edge, the audit rows and the re-pair
transaction. The pairing-issue ledger row is in 4d's `gateway.rs`, because that call site has `Deps`
and `issue_surface_token` keeps its `&Store` argument. The rotate/revoke functions gain the `&Deps`
that `Ledger` needs, so 4c also owns the two call sites and the in-file test call in
`crates/vowp-cli/src/cmd/surface.rs` for that signature update only. The frame and its schema are
unchanged. The agent-side `needs_pair` state and status fields are in `v4-local-4b-node-runtime`;
the CLI flags and columns are in `v4-local-4f-cli-surface`.

**Owns.** `crates/vowp-core/src/agent/mod.rs`, `crates/vowp-core/src/agent/pair.rs`,
`crates/vowp-core/src/agent/spool.rs` (created by `4a`), `crates/vowp-store/src/repo_surfaces.rs`,
`crates/vowp-core/tests/agent.rs`, `crates/vowp-cli/src/cmd/surface.rs` (the two ledger-aware call
sites and their in-file test call only).

**Depends on.** `v4-local-4a-surface-rotate` (merged).

### 9.3 `v4-local-4d-bundle-result`

**Scope.** Add the bundle result: the five frames, the enabled config value, the assignment field,
the `BundleAccept`/`BundleAbort` dispatch in the agent serve loop, agent-side
creation/chunking/resume, home-side verification/staging and the result hand-off, plus the bundle
fields in the durable result record. It also writes the pairing-issue `node_ledger` row at the
gateway call site. Push stays the default and nothing falls back.

**Owns.** `crates/vowp-protocol/src/agent.rs`, `crates/vowp-protocol/src/offload.rs`,
`crates/vowp-protocol/src/config.rs`, `crates/vowp-protocol/tests/agent.rs`,
`crates/vowp-protocol/tests/offload.rs`, `crates/vowp-protocol/tests/config.rs`,
`crates/vowp-store/tests/offload.rs`, `crates/vowp-core/src/agent/gateway.rs`,
`crates/vowp-core/src/agent/result.rs`, `crates/vowp-core/src/agent/dispatch.rs`,
`crates/vowp-core/src/offload/policy.rs`, `crates/vowp-core/tests/offload_result.rs`,
`crates/vowp-core/tests/offload_e2e.rs`, `crates/vowp-agent/src/client.rs`,
`crates/vowp-agent/src/executor.rs`, `crates/vowp-agent/src/worktree.rs`,
`crates/vowp-agent/tests/executor.rs`, `crates/vowp-cli/tests/agent.rs` and
`crates/vowp-cli/tests/offload.rs` (the `AssignmentPayload` literals only; 4f keeps the rest).

**Depends on.** `v4-local-4b-node-runtime` (it re-opens that slice's `client.rs` and protocol
`config.rs`); it shares no path with the rotation or metrics slices.

### 9.4 `v4-local-4e-metrics-history`

**Scope.** Add global schema v7 `capacity_rollups` and its `migrate_v06`/`migrate_v06_if_needed`
v6-to-v7 migration and backup in `migrate.rs`, called from the shared
`Store::open_global_unbound` path in `db.rs` (not from `Store::open_writable`), the 5-minute rollup
writer and pruner, the `CapacityHistory` document and its `schema.rs` registration, and the
read-only `vowp capacity history` data path. Raw samples stay 48 hours, rollups 30 days. Cost
remains the existing ledger.

**Owns.** `crates/vowp-store/src/schema/global.sql`, `crates/vowp-store/src/db.rs`,
`crates/vowp-store/src/migrate.rs`, `crates/vowp-store/src/repo_capacity.rs`,
`crates/vowp-store/tests/migrate.rs`, `crates/vowp-store/tests/metrics.rs` *(new)*,
`crates/vowp-core/src/capacity/mod.rs`, `crates/vowp-core/src/capacity/sampler.rs`,
`crates/vowp-core/src/capacity/history.rs` *(new)*, `crates/vowp-core/tests/capacity_measure.rs`,
`crates/vowp-protocol/src/capacity.rs`, `crates/vowp-protocol/src/schema.rs`,
`crates/vowp-protocol/tests/schema.rs`.

**Depends on.** Nothing beyond `main`.

### 9.5 `v4-local-4f-cli-surface`

**Scope.** Wire the four library slices into one user-facing CLI release: the hidden gateway
`--listen`, `agent service install|uninstall|status`, `agent run --service`, `agent doctor`, the WSS
enroll flags, `surface rotate --wait/--grace`, `surface pair --replace`, the prefix/expiry columns
and status array, `capacity history`, and the doctor warnings. Add the CLI and JSON tests; do not
change a library API owned by an earlier slice without a planner widening.

**Owns.** `crates/vowp-cli/src/**`, `crates/vowp-cli/tests/**` (the existing `agent.rs`,
`capacity.rs`, `doctor.rs`, `offload.rs` and `up.rs` tests are extended here; the library tests stay
with their slices).

**Depends on.** `v4-local-4b-node-runtime`, `v4-local-4c-rotation-ux`,
`v4-local-4d-bundle-result`, `v4-local-4e-metrics-history`.

### 9.6 `v4-local-4g-hardening-docs`

**Scope.** The guide, the security page, the crate/design/reference pages, the status lines and the
release-gate evidence (security review, owner live Mac smoke test, rollback drill). It is docs-only;
it runs after the CLI surface is merged so every command it shows exists.

**Owns.** `docs/**`, `CHANGELOG.md`.

**Depends on.** `v4-local-4f-cli-surface`.

## 10. Phase-4 test matrix

| Scenario | Setup | Expected |
|---|---|---|
| WSS frame parity | local TLS proxy, temp home, fake node key | the same frames, auth and `Resume` as SSH stdio; no SSH fallback |
| WSS bad certificate | test CA, wrong host or changed fingerprint | refused before `AgentHello`; no TOFU reset |
| Service install | temp `HOME`/`XDG_CONFIG_HOME`, fake manager | one 0600 unit; no manager call without `--now`; uninstall only its own file |
| Service drain | fake transport, open spool, surface lease | spool flushed, `SurfaceDetach`, lease released, exit 0 |
| Revoked under service | fake home sends `Revoked` | launchd `run` removed / systemd exit 5; no restart loop |
| Rotation offline | fake clock, queued frame, reconnect after overlap | old token refused, new token applied, status shows the new expiry |
| Rotation past expiry | queued frame older than 30 days | frame discarded, surface expired, `needs_pair` |
| Re-pair | refused token, `surface pair --replace` | old surface revoked, new pending surface, no stale frame |
| Bundle happy path | temp git repo, fake agent, bundle mode | verified bundle, base/head, gates, merge queue, no push |
| Bundle mismatch | altered byte, second ref, wrong head | rejected, no ref update, staging removed |
| Bundle resume | cut mid-transfer, reconnect | resume at offset, one verified bundle |
| Separate user | temp state dir with mode 0755 / foreign uid predicate | refused with the path and expected mode |
| Metrics rollup | fake clock, fake `/proc`, temp DB | 5-minute points, idempotent, 30-day prune, gap not zero |
| Metrics read | read-only store | no ledger/target/advice row; schema-valid JSON |
| Rollback drill | restored v6 copy of a home, previous binary | v0.3 flows pass; no data loss; the live v7 file is not downgraded |

All tests are hermetic: temporary `VOWP_HOME`/`VOWP_AGENT_HOME`, a private loopback port for the
proxy and the daemon, real git in temporary repositories, fake harnesses and a fake service manager.
No test touches the real home, `~/.vowp`, a real tmux server, `launchctl`, `systemctl`, `sudo`, the
network beyond loopback or a real `claude`/`opencode` binary.

## 11. Open questions

Each has a recommended default; none is an owner decision on a public or hosted service.

1. **Offline rotation default.** Should a rotation for an offline node keep the 10-minute overlap
   (security first) or wait for delivery plus the overlap (availability first)? Recommended: keep
   the 10-minute default, add the explicit `--grace` (max 24 hours) and `--wait`; a node offline
   past the replacement's 30-day life re-pairs. The outbox already survives a restart, so no silent
   token resurrection is needed.
2. **Certificate fingerprint.** Should a normal certificate renewal require an owner confirmation,
   or should the agent pin only the CA? Recommended: warn and require `agent trust` on a changed
   leaf fingerprint by default; `tls = "pin"` is the hard-pin self-signed mode. A CA-only pin is a
   later option if renewals become noisy.
3. **Bundle cap.** Is 256 MiB per run and a 5-minute transfer the right cap? Recommended: yes; it
   matches the ASIF upload cap's order of magnitude and fits a normal commit range. A larger cap
   is a config value later, not a code change.
4. **Metrics retention.** Is 30 days of 5-minute rollups enough, with 48 hours of raw samples?
   Recommended: yes for a single-home VPS; 1-hour read-time buckets cover a month and the storage
   stays around 7 MiB together (about 5.5 MiB of raw rows plus 1.2 MiB of rollups). Alerts are
   explicitly out of scope.
5. **Separate user on macOS.** Is a `LaunchAgent` in the dedicated user's GUI session acceptable,
   with a root `LaunchDaemon` unsupported? Recommended: yes; it keeps the no-root rule and the
   owner can log the user in once. A root daemon would be a new owner decision.
6. **Surface re-pair command.** Is `surface pair --replace <id>` enough, or should a dedicated
   `surface re-pair <id>` exist? Recommended: `--replace`; one command, one transaction, no second
   concept. The agent's message names the exact command.
7. **WSS config location.** Is `[ingress.wss]` in the node `config.toml` the right home for the
   listener, or should it be a `vowp up` flag only? Recommended: the config table, because the
   listener is a long-lived deployment choice and `vowp up` should start it consistently; a flag
   can override it for a one-off test.

## 12. See also

- [`local-orchestrator.md`](local-orchestrator.md) — the accepted transport, offload, result and
  phase plan this page implements.
- [`live-upgrade-and-drain.md`](live-upgrade-and-drain.md) — drain, deferred migration and rollback
  mechanics the phase-4 drill uses.
- [`../v0.3/http-api.md`](../v0.3/http-api.md) — the loopback daemon that must stay the only
  listener and the reason a WSS ingress is a proxy, not a new API.
- [`../../security.md`](../../security.md) — trust boundaries, secrets, redaction and the hardening
  checklist.
- [`../../concepts/capacity.md`](../../concepts/capacity.md) — the measurement, admission, brake and
  advice rules the metrics history reuses.
- [`../sessions.md`](../sessions.md) — the log, capsule and bundle rules; why a run returns a git
  bundle, not a session bundle.
- [`../cli.md`](../cli.md) — the command contract the CLI slice extends.
- [`../crates/vowp-agent.md`](../crates/vowp-agent.md),
  [`../crates/vowp-core.md`](../crates/vowp-core.md),
  [`../crates/vowp-store.md`](../crates/vowp-store.md) and
  [`../crates/vowp-cli.md`](../crates/vowp-cli.md) — the crate APIs the build slices extend.
- [`../../guides/install.md`](../../guides/install.md),
  [`../../guides/multi-node.md`](../../guides/multi-node.md) and
  [`../../guides/operating.md`](../../guides/operating.md) — the current service, node and
  operating guides the docs slice updates.
- [`../../roadmap.md`](../../roadmap.md) — where phase 4 sits and the follow-ups it does and does
  not absorb.
