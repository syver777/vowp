# crate `vowp-cli` (binary `vowp`)

Thin binary over `vowp-core`: argument parsing (clap derive), rendering (human + `--json`), exit codes, `--node` over ssh, signal handling.
All behaviour of commands is in `cli.md`; this file fixes the **module layout, ownership, shared API and tests**. `anyhow` is allowed only here
(`main.rs`); inner modules return `CliError`. Dependencies: `conventions.md` §3.

## 1. Module layout

The crate is `[[bin]] name = "vowp"`, with a tiny `src/lib.rs` exposing `pub fn run(args: Vec<String>, io: &mut dyn IoStreams) -> i32` so tests call commands in-process
(no subprocess needed except for the few `assert_cmd` smoke tests).

```
crates/vowp-cli/
  Cargo.toml
  src/main.rs         fn main() -> ExitCode { vowp_cli::run(std::env::args().collect(), &mut RealIo) }   (anyhow only here, for panics/IO setup)
  src/lib.rs          pub fn run, IoStreams trait (stdout/stderr/stdin writers+readers; tests use a buffer impl), re-exports
  src/cli.rs          clap derive structs for EVERY command in cli.md (global flags + subcommand enum), parse -> `Command`
  src/ctx.rs          Ctx { paths, deps: vowp_core::Deps, project: ProjectName, json: bool, color: bool }, Ctx::build(&Global) (loads vowp.toml, opens Deps with SystemRunner/SystemGit/Tmux)
  src/output.rs       Table (aligned columns), `Out::{human, json}`, time formatting, `json_error`
  src/errors.rs       CliError + exit-code mapping (cli.md §1.1)
  src/remote.rs       --node over ssh: build_ssh_argv, run_remote
  src/signals.rs      SIGTERM/SIGINT -> CancelToken with signal_hook::flag
  src/cmd/mod.rs      `pub mod` for all of the below and `pub fn dispatch(ctx, Command) -> Result<i32, CliError>`
  src/cmd/init.rs  doctor.rs  job.rs  graph.rs
  src/cmd/up.rs  status.rs  board.rs  attach.rs  plan.rs  window.rs   (`up`,`down`, `status`, `board`, `attach`, `plan`, hidden `__window`)
  src/cmd/session.rs  inbox.rs  cost.rs  node.rs  offload.rs  loops.rs  schedule.rs  maint.rs   (`maint` = reindex, backup, schema, version)
  src/cmd/session_asif.rs  (ASIF export/import facade: flags, temp files, remote stream)
  src/cmd/promote.rs  (`promote`, `rollback`, `releases`)
  src/cmd/capacity.rs  (`capacity show|report|set|reload|history`; the report adds effective
                       provider caps and reload pending, `reload` asks the supervisor or checks
                       locally, and `history` reads the bounded rollups read-only)
  src/cmd/agent.rs    (`agent enroll|paths|pair|run|status|service|doctor` plus hidden `agent gateway`; no store or project context for the Mac commands)
  src/agent_exec.rs   (`CoreRunHandler`: the Mac assignment executor used by `agent run`, including bundle chunking)
  src/cmd/surface.rs  (`surface pair|ls|rotate|revoke`; home-only global-store surface pairing)
  src/mcp/{mod,jsonrpc,tools,backend,proxy}.rs  (home stdio MCP server and Mac-side local proxy)
  src/framework.rs    embeds the templates from the repo `framework/` dir with include_str!/include_dir-free explicit list; `init` writes them
  tests/*.rs          tests for the commands
```
Every command module in `src/cmd/` is real code. A command that cannot run returns an explicit error (exit code 1, message `not implemented in this build`);
no command may ship as a stub in a final build: the acceptance test `no_command_returns_not_implemented` iterates every command in `cli.rs` with `--help`-free dry arguments and fails on `NotImplemented`.

## 2. Shared API (frozen)

```rust
pub trait IoStreams { fn out(&mut self) -> &mut dyn std::io::Write; fn err(&mut self) -> &mut dyn std::io::Write; fn input(&mut self) -> &mut dyn std::io::BufRead; fn is_tty(&self) -> bool; }

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")] Invalid(String), #[error("{0}")] NotFound(String), #[error("{0}")] Conflict(String),
    #[error("{0}")] Guard(String), #[error("{0}")] Limited(String), #[error("remote: {0}")] Remote(String),
    #[error("not implemented: {0}")] NotImplemented(&'static str),
    #[error(transparent)] Core(#[from] vowp_core::CoreError), #[error(transparent)] Store(#[from] vowp_store::StoreError),
    #[error(transparent)] Protocol(#[from] vowp_protocol::ProtocolError), #[error("io: {0}")] Io(#[from] std::io::Error),
}
impl CliError { pub fn kind(&self) -> vowp_protocol::ErrorKind; pub fn exit_code(&self) -> i32; pub fn hint(&self) -> Option<String>; }

pub struct Global { pub json: bool, pub node: Option<String>, pub project: Option<String>, pub home: Option<PathBuf>, pub no_color: bool, pub quiet: bool, pub verbose: u8 }
pub struct Ctx { /* see layout */ }
impl Ctx { pub fn build(g: &Global, io: &dyn IoStreams) -> Result<Ctx, CliError>; pub fn store(&self) -> Result<vowp_store::Store, CliError>; pub fn clock(&self) -> &dyn Clock; }

pub struct Table { /* header + rows */ }
impl Table { pub fn new(headers: &[&str]) -> Self; pub fn row(&mut self, cells: Vec<String>); pub fn render(&self, color: bool) -> String; }
pub fn print_json<T: serde::Serialize>(io: &mut dyn IoStreams, v: &T) -> Result<(), CliError>;       // pretty-printed, trailing newline
pub fn print_error(io: &mut dyn IoStreams, e: &CliError, json: bool);                                   // cli.md §1.1 shapes
pub fn local_time(ts: Timestamp, now: Timestamp) -> String;                                           // "HH:MM" today else "MM-DD HH:MM" (UTC offset from [node] tz_offset_minutes)

pub mod remote {
    pub fn build_ssh_argv(target: &str, args: &[String], interactive: bool) -> Vec<String>;             // pure; quoting via vowp_tmux::quote::shell_quote
    pub fn run_remote(target: &str, args: &[String], interactive: bool) -> Result<i32, CliError>;        // spawns ssh; 255 -> Remote error
}
```

## 3. Agent commands (`cmd/agent.rs`)

`vowp agent` is the v0.4 Mac-side client from
[`vowp-agent.md`](vowp-agent.md) and `cli.md` §12. The Mac commands are handled before `Ctx::build`
and before the generic `--node` redirect: they use `~/.vowp-agent` (or `VOWP_AGENT_HOME`) and never
open a project store. `agent run` and `chat --agent` build adapters through
`vowp_core::AdapterRegistry`; neither opens a database. The hidden `agent gateway` command is the
home-side exception: it discovers the node with `node_ctx::NodeCtx::discover` and requires the home
role. An explicit `--project` or a cwd inside a registered checkout builds the project-scoped `Ctx`
and opens `Ctx::store()`; everywhere else it ensures the reserved `_node` project and opens the
home's global store with `Store::open_global` and `node_ctx::home_deps`. The gateway resolves each
session's or offload assignment's project store per connection.

- `agent enroll --target <ssh-target> --code-file <path|->` reads `-` from the `IoStreams` stdin,
  trims the line, validates the 52-character code shape and passes it only to
  `AgentClient::enroll`. The code never appears in argv, `ps`, output or a log. Success writes
  `agent.json` 0600 and prints the node, home and protocol; `--json` prints
  `{"node","home","protocol","heartbeat_secs"}`.
- `agent pair --code-file <path|-> --surface <id> --session <id> [--app <app>]` reads the code
  from the `IoStreams` stdin, calls `AgentClient::pair` and prints the surface id, scopes, expiry
  and token path. `--json` prints `{"surface_id","scopes","expires_at","token_path"}`. The token
  is written only to `~/.vowp-agent/surfaces/<id>/token` (0600) and is never printed. A bad or
  replayed code is a non-retryable refusal (exit 5).
- `agent run [--foreground] [--role surface|executor|both]` loads `agent.json` and `node.key`,
  builds the SSH transport, wraps a `CoreRunHandler` in an `Arc` and calls
  `AgentClient::run_with_handler`. `status.json` is rewritten on every heartbeat. The Phase 0
  role spellings are unchanged and always run in the foreground; `--foreground` is accepted for
  the documented surface. A revoked credential exits 5 and a superseded connection exits 4.
- `agent status [--json]` loads `status.json`; without `agent.json` it exits 3 with the enrollment
  command in the message. It fills missing fields from `agent.json` and `node.pub`, recounts
  `spool/`, and prints the human lines `connection`, `node`, `home`, `protocol`, `last heartbeat`,
  `pending spool`, `key`, optional `surface token expires` and optional `last error`. The local
  `surfaces` array adds `prefix` and `state` per surface and never a token.
- `agent enroll ... [--wss <url>] [--tls-fingerprint <sha256:hex>]` stores a WSS transport instead
  of SSH. The URL must be exactly `wss://host[:port]/agent/v1`; `--tls-fingerprint` requires
  `--wss` and selects the `pin` TLS mode. The first system-mode connection records the leaf
  fingerprint in `agent.json`. A TLS or handshake refusal maps to a guard (exit 5).
- `agent service install|uninstall|status` renders or removes exactly one 0600 launchd/systemd-user
  unit and reads the manager. `--print` writes nothing and calls no manager; `--now` runs the
  current user's manager commands. A foreign unit is exit 4, an unsupported platform exit 5 and a
  failed manager exit 7. The state directory and every token survive `uninstall`.
- `agent run --service` checks the state directory and root rule, writes the launchd `run` marker,
  and drains on SIGTERM/SIGINT. `agent doctor` reports local warnings by name only and never reads
  a copied login file.
- `agent gateway --listen <addr>` (hidden) serves the optional WSS listener on a loopback address
  with the `/agent/v1` route and the `vowp-agent/1` subprotocol. `vowp up` starts it from
  `[ingress.wss]`; the listener never terminates TLS.
- `agent gateway --stdio` (hidden) is the home side: it requires `--stdio`, discovers the node,
  refuses any role but home with exit 5 and an uninitialised home with exit 3, and calls
  `vowp_core::agent::gateway::serve`. Only an explicit `--project` or a cwd inside a registered
  checkout builds the project-scoped `Ctx` and opens `Ctx::store()`; otherwise it ensures the
  reserved `_node` project row and database and opens the global store with `node_ctx::home_deps`.
  Stdout carries JSON frames only; errors go to stderr. Phase 0 has no `--socket`.

`AgentStatus` is CLI-local (it is not an exported protocol schema), has no token or secret field,
and serialises as:

```json
{
  "connection": "connected",
  "node": "mac-studio",
  "home": "home-1",
  "protocol": "vowp-agent/1",
  "last_heartbeat": "2026-11-14T10:00:00.000Z",
  "last_error": null,
  "pending_spool_bytes": 42,
  "key_fingerprint": "sha256:0123456789abcdef",
  "surface_token_expires_at": null,
  "surfaces": [
    {"surface": "01ARZ3NDEKTSV4RRFFQ69G5FAW", "prefix": "AbCdEfGhIjKl",
     "expires_at": "2026-12-16T10:00:00.000Z", "state": "paired"}
  ]
}
```

### Assignment executor (`agent_exec.rs`)

`CoreRunHandler` is the [`vowp-agent`](vowp-agent.md) `RunHandler` behind `agent run`. It uses
the merged worktree, path-map and run-sink APIs; it does not change the `vowp-agent` trait and
does not open a store. One handler lives for one agent connection.

- **Accept.** The assignment's project must exist in the saved path map (`ProjectUnmapped`).
  `PathMap::new` canonicalises the local and worktree roots and checks they exist, are
  directories and carry a write bit (`PathMissing`/`PathNotWritable`/`NotADirectory`); the
  `cwd_ref` is resolved through the same map, so traversal, a symlink escape, a case-only alias
  and a subdirectory outside the allow list (`CwdOutsideAllow`) are refused. The checkout's
  `vowp.toml` supplies the harness registry; a missing or unprobeable harness is
  `HarnessMissing`. Every `env_names` and `secret_names` entry must resolve locally, otherwise
  the rejection is `EnvMissing` with the **name** in the detail. Free disk below the 5 GiB
  reserve is `DiskLow`, an active run for the same job or an existing unowned worktree is
  `WorktreeConflict`, and an invalid base commit/remote is `Other`. The handler reads
  `agent.json` on each accept, so `agent paths add|rm` is visible without a restart.
- **Worktree.** `run` calls `WorktreeManager::create` with the home-supplied base commit, the
  assignment's remote and the reserve, producing `<worktree_root>/<job>` on `job/<id>`. A
  clean, owned worktree at exactly the base is reused; a stale owned one is recreated; an
  unowned directory is refused. The main checkout is never checked out, reset or cleaned.
- **Adapter and events.** The adapter starts in the worktree with the assignment prompt and the
  resolved environment. `adapter_for` is shared with `chat --agent`, so the `vowp.toml` harness
  configuration is read in one place. Events are converted to `RemoteEvent`s with fresh stable
  ids and streamed through the injected `RunSink`; the adapter's terminal `turn.ended` is
  skipped because the terminal `TurnSummary` is canonical. A quiet adapter gets a liveness
  heartbeat every ten seconds. The loop stops the adapter when the sink is stopped, when cancel
  or a fence arrives, or when the sink reports the spool cap; the summary records `cancelled`,
  `fenced` or `spool_full` in its result text and uses `Killed`/`Interrupted` or `Error`/`Error`
  as appropriate.
- **Env, secrets and redaction.** Only names cross the wire. Values resolve from this machine's
  environment, are passed to the adapter, and are replaced with `[REDACTED:<NAME>]` in every
  event payload, raw line, limit message and result text before the sink sees them. The known
  worktree and project roots are replaced with `worktree:<project>/<job>` and
  `project:<project>`; the agent state directory becomes `<agent-home>`. Rejection details,
  push failures and local error summaries carry no absolute path and no value.
- **Result return and cleanup.** After the adapter exits the summary carries the exit code,
  outcome, native id, cost, head commit and `PushStatus`. A non-empty `remote_url` with
  `result_transport = "push"` causes `Worktree::push_job(remote_url)`: only
  `refs/heads/job/<id>` is pushed, the push URL must match the home project after credentials are
  stripped, no tag or default-branch push is attempted, and a mismatch or git failure is
  `PushStatus::Failed` with the worktree kept for inspection. An empty `remote_url` is
  `PushStatus::NotRequested`. With `result_transport = "bundle"` the handler creates one
  `base..job/<id>` bundle under `run-bundles/<run>/`, sends `BundleStart`/`BundleChunk`/`BundleEnd`
  at the home's offset and never pushes or falls back. `Cleanup` removes only the job worktree and
  `job/<id>` branch through `WorktreeManager::cleanup`; it never touches the main checkout or a
  directory VOWP did not create, and it waits briefly for a cancelled run to leave the worktree.

`agent status` keeps its connection view and adds the local `surfaces` array; the `status.json`
snapshot carries the connection fields and no path map, secret name or secret value. The human
view keeps its local-only `paths` block, and `--json` includes the same `paths`, exactly as before.

### Surface commands (`cmd/surface.rs`)

`vowp surface` is handled before the generic `--node` redirect and the normal command dispatch,
because its `--node` flag names the surface's node rather than an SSH target. The
commands are home-only (a control machine exits 5) and open the global store through
`Ctx::fleet_store()`, so no project selection is required.

- `surface pair --session <id> --node <name> --kind opencode|chat [--app <app>] [--replace <old>]`
  calls `create_pending_surface`, or `replace_pending_surface` when `--replace` names an old
  surface. It validates the node, session and app, inserts a `pending` surface bound to the
  session, and inserts a ten-minute `surface` enrollment code whose plaintext is shown once. The
  command prints the code, the expiry and the exact Mac command `vowp agent pair --code-file -
  --surface <id> --session <id> [--app <app>]`; the code is never written to a file, the database
  or stderr. `--json` prints
  `{"surface","node","session","kind","app","code","expires_at","command"}` plus `replaced` when
  one was named. A missing `--node`/`--session`, an unknown kind or an invalid id is exit 2; an
  unknown node or session is exit 3; a duplicate pending surface for the same
  node/session/kind/app is exit 4; a revoked node is exit 5 through `CoreError`.
- `surface ls [--node <name>]` lists the surface ids this CLI created, reading each surface's
  current state from the store and each token's scopes, prefix and expiry from `surface_tokens`.
  Human output is `ID NODE KIND STATE SCOPES PREFIX EXPIRES OVERLAP`; `--json` prints an array of
  `{"id","node","kind","state","scopes","prefix","expires_at","overlap_until","expired","pending","queued_at","delivery_deadline"}`
  (`queued_at` and `delivery_deadline` are omitted when absent; the other optional fields are
  `null`). It never prints a token or a token hash. The id index is
  `<home>/surfaces.json` (0600): surface ids are not secrets, and the v4 store read model has no
  list-all query, so the CLI records the ids it created.
- `surface revoke <id> [--yes]` calls `vowp_core::agent::pair::revoke_surface`, which revokes
  every active token, removes a queued rotation and marks the surface `revoked`. Without `--yes`
  it refuses with exit 5; an unknown surface is exit 3. `--json` prints
  `{"surface","status":"revoked","tokens_revoked":N}`. No token is printed.
- `surface rotate <id> [--node <name>] [--wait <duration>] [--grace <duration>]` calls
  `rotate_surface_with_policy`, which mints a 30-day replacement token with the surface's current
  scopes, shortens every other active token to `now + grace` without extending a sooner expiry,
  queues one `SurfaceIssued` envelope in `<home>/agent-frames/<node>/<surface>.json` and waits if
  asked. The default grace is ten minutes and the cap is 24 hours. The token is never printed.
  Human output is `rotated surface <id> on <node>`, `prefix`, `expires`, `old token expires` and
  `delivery: pending` while undelivered; `--json` prints
  `{"surface","node","prefix","expires_at","old_expires_at","pending","queued_at"}`. `--node`
  names the surface's node, not an SSH target; a different node is exit 4. `--wait` exits 7 with
  `rotation_pending` when the frame is still queued. A pending surface is exit 4; an unknown
  surface is exit 3; a revoked surface is exit 5. When the node is not online the rotation still
  succeeds (exit 0), the frame stays queued for the next authenticated connection and the command
  writes `warning: node <n> is offline; the agent must re-pair before <old expiry>` to stderr.

### Node enrollment commands (`cmd/node.rs`)

These are home-only (exit 5 on a control machine) and require the home store:

- `node enroll <name> [--label <label>]...` calls `vowp_core::agent::enroll::create_pending`, which
  inserts an `agent`-kind `pending` node and a ten-minute code. It prints the 52-character code once
  and the `vowp agent enroll --target <home-ssh-target> --code-file -` command; `--json` prints
  `{"node","code","expires_at","command"}`. The code is never written to a file or the log. An
  existing node is exit 4. `--label` is repeatable.
- `node revoke <name> [--yes]` calls `enroll::revoke`, which marks the node `revoked` and revokes
  every credential. Without `--yes` it refuses (exit 5). `--json` prints
  `{"node","status":"revoked","credentials_revoked":N}`. An unknown node is exit 3. Phase 0 has no
  live connection registry; the gateway sends `Revoked` on the node's next frame.
- `node show <name> [--json]` returns the redacted `NodeCard` plus `last_seen_at` from the node row
  and never a path map, absolute local path or secret name. Human output prints status, presence,
  labels, capacity, mapped projects, agent version, protocol, last seen and last error. `node ls`
  and its JSON `Node` shape are unchanged.

### Offload commands (`cmd/offload.rs`)

The read-only v0.4 Phase 2 commands from `cli.md` §14. Neither sends `AssignRun`, creates a
worktree or runs an agent.

- `offload policy [--project <p>] [--json]` uses `ctx.config().offload`; it does not open a store
  for the document itself. It fills defaults through `OffloadConfig::project_policy` and
  `node_policy`, adds the selected project and registered node cards, and prints one
  `ProjectPolicyRow` per project and one `NodePolicyRow` per node. `--project` is the global
  selector: clap records whether that flag was passed, and an explicit selection restricts the
  project rows to that project, including when the selected root is the cwd. The human view is a
  scalar block followed by two `Table`s; `--json` is one `EffectivePolicy` object.
- `offload status [--json]` opens `Ctx::fleet_store`, reads node cards, live assignments
  (`queued|sent|accepted|running|disconnected`) and per-node limits, then opens every registered
  project read-only in turn. For each ready job whose effective placement (job row, else project
  default, else `offload.default`) is not `home` it builds `vowp_core::offload::NodeFacts` and
  calls `evaluate` with the project's effective policy. The human view is the
  `nodes`/`assignments`/`ready` tables; `--json` is `OffloadStatusOutput`.
  `AssignmentRow` deliberately omits `AssignmentPayload` (its prompt is not part of this view), and
  no row carries an absolute local path.
- `job add --placement` parses `Placement`, refuses `node:<name>` with exit 5 when
  `offload.enabled = false` before inserting anything, writes the `job_placement` row immediately
  after the job row, and deletes the job if either the placement or brief write fails. `job show`
  adds the placement to human output and to its JSON object; `job ls` is unchanged.

### Orchestrator commands (`cmd/orchestrator.rs`)

The v0.2 `new/ls/stop/resume` handlers keep their home behaviour. The v0.4 slice adds the
node that runs the app:

- `vowp orchestrator new --app <app> [--node <name>] [--project <p>] [--name <n>]` passes the
  node to `Orchestrator::create_on_node`. For this one command the global `--node` names the
  orchestrator's node, not an SSH redirect: `Cli::try_parse_from` moves the parsed global value
  into `OrchestratorNewArgs.node` before `remote::maybe_run` sees it, and the `new` help footer
  says so. Every other command keeps the global redirect. A remote-node orchestrator never takes
  a home lease, never starts a home host and never starts a keeper; the home creates the
  session/card and sends a typed command. A node value that names the home (the local `deps.node`
  or a registry row marked `is_local`) keeps the v0.2 home host and lease, so `new`/`resume`
  start the detached keeper for it.
- `orchestrator ls` adds a `NODE` column (`home` for a v0.2 orchestrator) and the JSON card
  carries `node` and the typed `state`. `stop`/`resume <name>` read the node from
  `orchestrator.json`; `resume` on an offline node prints `node_offline` and exits 7.
- The output is the `OrchestratorCard`; a node card carries no directory, port, token, pair
  link or secret. Human output prints the orchestrator directory and the card's join text only.

### Local chat (`chat --agent`)

`vowp chat --agent --session <id> [--executor <exec>] [--node <name>]` runs the Phase 1 local
surface. The positional id stays required for the home spelling; `Cli::try_parse_from` in
`src/cli.rs` inserts the `--session` value before clap parses an agent command, so `vowp chat`
without an id still fails with clap's missing-argument error and the help tree keeps `<ID>`. The
parser records `--agent`, and `lib.rs` dispatches it to `src/chat/agent.rs` before the
control-machine route and before `Ctx::build`. The Mac therefore needs no local home database and
the command is never forwarded to the home. The bridge resolves its checkout from `--project
<dir>` or the nearest `vowp.toml`; it does not open the node store.

The handler loads `~/.vowp-agent` (or `VOWP_AGENT_HOME`), the node key and the surface token
bound to the session, then calls `vowp_agent::ChatClient::turn`. The local checkout's `vowp.toml`
supplies the project name, the working directory and the default executor (the first rung of the
`worker` profile; `--executor` overrides it). The agent acquires
the lease, sends `TurnBegin` with a project-relative `PathRef`, runs the adapter on this machine,
spools each event before sending it and prints assistant text, tools, errors and notes in the
home chat's 80-column format. The home log is authoritative; the CLI never opens a second session
log or takes the home lease. Ctrl-C (through `SignalWatcher`) stops the adapter and releases the
lease; a takeover or lease loss prints `This surface is read-only: <reason>` and exits 0; a full
spool exits 7 with the cap in the message. `--follow` and `--takeover` are refused in agent mode.

### MCP server and local proxy (`mcp/`)

`vowp mcp` has two modes. Both speak newline-delimited JSON-RPC 2.0 on stdin/stdout and share the
framing and local-method code in `src/mcp/jsonrpc.rs` and `src/mcp/mod.rs`, the tool catalogue in
`src/mcp/tools.rs` and the home execution path in `src/mcp/backend.rs`.

- `vowp mcp --session <id> --surface <id> --token-file <file>` is the home server. It builds
  `Ctx`, opens the home store and executes every tool through `mcp/backend.rs`. Writes re-read the
  lease token on every call; a takeover makes the surface read-only. `tunnel_open`/`tunnel_close`
  are control-machine tools and `secret_request` returns the paste URL, never the value.
- `vowp mcp --agent-socket <path> --surface <id>` is the Mac proxy (`src/mcp/proxy.rs`). `lib.rs`
  dispatches it before the generic `--node` redirect and before `Ctx::build`, so it opens no home
  store, project database or adapter. `--session` and `--token-file` are rejected with it; the
  session comes from the pairing binding `<state_dir>/surfaces/<surface>/session` next to the
  socket, and `--surface` must be a 26-character ULID.
  - `initialize`, `server/discover`, `ping` and `tools/list` are answered locally, so an app can
    connect while the home is unreachable. The list omits the tools a local surface always
    refuses.
  - `tools/call` checks that the socket is a private (0600) Unix socket, connects once, sends one
    typed `ToolCall` with a per-process request id and returns the home's `result` unchanged. A
    typed home `ToolError` is preserved as an MCP `isError` result with its `code` and
    `retryable` members; a `lease_revoked` or `refused` result is never retried.
  - A missing or unusable socket, a home that is not connected and a closed exchange return a
    typed `home_offline` (retryable) rather than a store error; the agent's own `error` frame is
    passed through. No error, log line or response includes the socket path, a token or an
    argument value.
  - `tunnel_open`, `tunnel_close`, `secret_request`, `node_enroll`, `node_revoke`, `surface_pair`
    and `offload_policy_set` are refused locally with code `refused` and never touch the socket.
    Lease and token-scope checks stay on the home: the proxy checks neither its own view nor the
    agent's.
  - The offline snapshot cache is a later slice; this build has no local read path.

### ASIF export and import (`cmd/session_asif.rs`)

`vowp session export|import --format asif` keeps the command dispatch in `cmd/session.rs` and
puts every ASIF rule in `cmd/session_asif.rs`; the bundle handlers are unchanged. The module:

- **Validates the format contract before touching the store.** `--tail` is invalid for ASIF;
  `--redact`, `--redact-file`, `--keep-host-paths`, `--report`, `--strict`, `--allow-loss` and
  the hidden `--asif-stream` are invalid for a bundle; `--same-id` and `--executor` are invalid
  for ASIF; `--report -` is refused; `--strict` and `--allow-loss` conflict through clap. Every
  refusal is `CliError::Invalid` (exit 2).
- **Exports through `vowp_core::session::asif::export_asif`.** The default output is
  `./<resolved-id>.asif.zip`; `--out -` is refused on a TTY and with `--json`, and otherwise
  streams the exact package from a private `<home>/tmp` file. `--report` keeps the full adapter
  report; without it no sidecar is written. Human output is the path on stdout and the loss
  summary on stderr. `--json` prints `AsifExportOutput` (`format`, `path`, `session`, `bytes`,
  `report_path`, `report`).
- **Imports through `vowp_core::session::asif::import_asif`.** `-` stdin is staged in a private
  0600 file under `<home>/tmp` and capped with `MAX_ASIF_ARCHIVE_BYTES`; the default `--cwd` is
  `Ctx::project_root`; `--branch` is forwarded. The target is always a fresh id. Human output is
  the new session id; `--json` prints the protocol `AsifImportResponse`; reindex warnings go to
  stderr and are never added to the loss list.
- **Applies the VOWP strict policy.** It always calls the core facade with `strict: false`. It
  sets `allow_loss` from `--allow-loss`; in `--strict` mode it also sets it true so the adapter
  declares recoverable integrity/ambiguity problems and the core `Blocking`/`Integrity` filter
  can refuse them. A refusal is exit 5 and leaves existing `--out`/`--report` files untouched.
  `--allow-loss` keeps those declared losses non-fatal.
- **Owns the remote half.** Export reads `--redact-file` locally, forwards literals as
  `--redact`, runs the remote hidden `--asif-stream` command, parses one bounded envelope line,
  streams exactly `bytes` to `<out>.tmp` (or a private file for stdout), checks for an extra
  byte, waits for the exit code, verifies SHA-256 and only then renames or copies. A malformed
  envelope, truncation, extra byte or digest mismatch is `CliError::Remote` (exit 7). Import
  rejects a directory and `--report` with `--node`, streams the file/stdin, and relays the
  remote `AsifImportResponse` and exit code. Neither path re-checks the remote strict decision.
- **Never writes a session on failure.** All staging lives under `<home>/tmp` or `<out>.tmp`
  and is removed on drop; the core facade removes its own work directory on every error.

The loss summary is one stderr line with the count and unique kinds, or `asif: no declared
losses`; it also names the unsupported VOWP operations (`vowp.jobs`, `vowp.gates`,
`vowp.leases`, `vowp.project-db`). The summary and every error stay path-free and never echo a
redaction value.

## 4. Rules

1. Every command handler is `fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &XArgs) -> Result<i32, CliError>`; no handler prints directly to `std::io`.
2. `--json` output types are the protocol types (types.md) wherever one exists (`Job`, `Session`, `InboxItem`, …); only summaries (`status`, `doctor`, `cost`) have CLI-local `Serialize` structs, defined next to the command.
3. Commands that mutate job state use `vowp_core::requests`: if a supervisor holds the lock they `push` a request and poll the job row ≤ 5 s; else `apply_direct`.
4. `Ctx::build` never creates `~/.vowp` implicitly except for `init`; other commands fail with `NotFound` ("run `vowp init`").
5. Interactive commands (`chat`, `attach`, `board`) check `is_tty()`; without a TTY they degrade as stated in `cli.md`.
6. Secrets: `doctor` reports env var **names** present/absent only.
7. `vowp --help` text for each command is the first line of its section in `cli.md`.

## 5. Named tests

Listed in `cli.md` §8, grouped by the file table above. Additional: `help_lists_every_v0_1_command` (compares clap's subcommand set with a constant list copied from `cli.md`),
`no_command_returns_not_implemented`, `json_flag_outputs_valid_json_for_every_listing_command`, `exit_codes_follow_error_kind`, `ctx_build_requires_init`, `io_streams_buffer_impl_captures_output`,
`capacity_reload_check_reports_without_writing`, `capacity_reload_refuses_when_no_supervisor_runs`;
the agent slice adds `agent_status_without_enrollment_exits_three`, `agent_status_prints_json`,
`agent_enroll_help_names_code_file` and `agent_enroll_accepts_code_file_stdin`; the node
enrollment slice adds `node_enroll_creates_a_pending_node_and_prints_the_code_once`,
`node_enroll_refuses_a_duplicate_node`, `node_revoke_marks_the_node_and_revokes_credentials`,
`node_show_returns_the_redacted_card` and `node_enroll_is_home_only` in `tests/agent.rs`. The
surface-pairing slice adds `agent_pair_help_names_code_surface_and_session`,
`agent_pair_accepts_code_file_stdin`,
`agent_pair_without_enrollment_exits_three_and_prints_no_code`,
`surface_pair_cli_prints_code_once_and_lists_pending`,
`surface_revoke_marks_the_token_revoked`, `surface_command_requires_a_node_and_session`,
`surface_ls_never_prints_a_token` and `surface_commands_are_home_only` in `tests/agent.rs`,
plus `surface_commands_are_registered_and_node_is_a_filter` in `tests/core.rs`. The token-rotation
slice adds `surface_rotate_queues_for_an_offline_node_and_warns`,
`surface_rotate_never_prints_the_token` and `surface_rotate_uses_the_existing_exit_codes` in the
`src/cmd/surface.rs` unit tests. The
offload-policy slice adds `job_add_refuses_node_placement_when_offload_is_disabled`,
`job_add_persists_any_placement_and_job_show_reads_it`,
`offload_policy_json_is_the_effective_policy` and
`offload_status_lists_nodes_assignments_and_waiting_reasons` in `tests/offload.rs`. The ASIF
slice adds the tests listed in `cli.md` §9 under "The ASIF slice"; `tests/asif.rs` owns the
local flows and `tests/session.rs` the fake-ssh remote flows. The
assignment-executor slice adds `agent_run_creates_a_worktree_and_streams_adapter_events`,
`agent_run_pushes_only_the_job_branch_and_reports_the_head`,
`agent_run_refuses_a_missing_env_name_and_redacts_injected_values`,
`agent_run_keeps_the_worktree_on_push_failure`,
`agent_run_cleans_up_after_a_terminal_summary`, `agent_run_cancel_stops_the_adapter`,
`agent_run_does_not_touch_the_main_checkout` and `agent_run_rejects_an_unmapped_project` in
`tests/agent.rs`. They use a fake adapter script, a fake in-process gateway, real git in
temporary repositories and a temporary `VOWP_AGENT_HOME`. The local MCP proxy slice adds
`mcp_proxy_serves_initialize_list_and_ping_without_a_home`,
`mcp_proxy_forwards_a_tool_call_and_returns_the_result`,
`mcp_proxy_passes_a_lease_revoked_error_through`,
`mcp_proxy_refuses_tunnel_and_secret_tools_without_touching_the_socket`,
`mcp_proxy_returns_home_offline_when_the_socket_is_absent` and
`mcp_proxy_never_prints_arguments_or_the_socket_path` in `tests/mcp.rs`. They use a temporary
home with no database and a fake mode-0600 Unix socket. The local-orchestrator slice adds
`orchestrator_new_with_a_node_creates_a_card_and_sends_start`,
`orchestrator_new_refuses_an_unknown_or_revoked_node`,
`orchestrator_resume_on_an_offline_node_reports_node_offline`,
`orchestrator_stop_sends_detach_and_releases_the_lease`,
`orchestrator_disconnect_marks_it_degraded_and_reconnect_resumes`,
`orchestrator_resume_all_marks_an_offline_node_degraded`,
`orchestrator_new_on_the_home_node_keeps_the_local_path`,
`orchestrator_apply_result_correlates_the_late_node_result` and
`orchestrator_card_contains_no_path_or_secret` in `tests/orchestrator.rs`. They use a
temporary home, a fake app host and a fake agent command stream; no real app, tmux, network
or home directory is touched.

The phase-4 CLI slice adds `agent_service_install_print_writes_nothing_and_names_the_manager`,
`agent_service_install_without_home_exits_two`, `agent_service_unsupported_platform_is_exit_five`,
`agent_service_status_reads_the_manager_and_reports_unavailable_json` and
`agent_service_uninstall_removes_only_a_generated_unit_and_keeps_the_state` in `tests/agent.rs`;
`surface_rotate_wait_reports_rotation_pending_with_exit_seven`,
`surface_rotate_grace_is_capped_at_twenty_four_hours`,
`surface_rotate_grace_refuses_pending_and_revoked_surfaces`,
`surface_pair_replace_revokes_the_old_surface_and_clears_the_stale_frame` and the WSS enroll
refusal cases in `tests/agent.rs`; and
`capacity_history_aggregates_read_time_hour_buckets`,
`capacity_history_clamps_since_to_the_retained_window_and_says_so`,
`capacity_history_unknown_bucket_exits_two_with_the_accepted_values`,
`capacity_history_is_read_only` and `capacity_history_json_is_the_capacity_history_document` in
`tests/capacity.rs`. `agent_doctor_reports_warnings_by_name_only` is in `tests/doctor.rs`. They
use temporary homes, fake manager commands, a fake gateway and real git where needed; no real
launchd, systemd, network or home directory is touched.

## 6. Out of scope
Shell completions (v0.2), TUI board beyond redraw loop, windows (OS) support.
