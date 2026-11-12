# Security model

VOWP runs agents that edit code, execute commands and, through promotion, can change what is live. The
security model is built around five limits: the home is the only writer, VOWP drives stock harnesses,
every job is confined to its own worktree and owned paths, secrets are named and injected only where
configured, and any public or production action needs an explicit human approval by default.

This page describes the model and what it does **not** protect against. The vulnerability-reporting
policy is in [`SECURITY.md`](../SECURITY.md).

> **Status: current.** The home/control split, leases, MCP write checks, secrets, the daemon's two
> token-gated writes, lifecycle, backup, gates, promotion and `vowp doctor` on this page are implemented,
> and these rules are binding on the code on `main`.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/syver777/vowp/security/advisories/new>

Do not open a public issue, discussion or pull request. Include the affected version or commit, the
operating system, reproduction steps and the impact you expect. Do not include credentials or other
secret values; refer to secrets by name only.

VOWP is pre-release. Security fixes land on `main` and in the latest `0.2.x` pre-release; older
pre-release builds are not supported.

## Assets and trust boundaries

| Asset | Where it lives | Why it matters |
|---|---|---|
| Source code and production environments | Project checkouts, worktrees, deploy targets | An agent can change them. |
| Prompts, tool output, file contents | Session logs, blobs, run mirrors under `~/.vowp` | They often contain sensitive code or data. |
| The global and project databases | `~/.vowp/vowp.db`, `~/.vowp/projects/<name>/project.db` | Jobs, decisions, leases and the audit trail. |
| Provider spend | Harness accounts, the secret store, environment variables | A runaway loop costs money. |
| Secret values | `~/.vowp/secrets/` (basic mode), `/home/vowp-vault/secrets/` (vault mode) or the service environment | They unlock providers and deploy targets. |
| Harness logins | Claude Code/opencode's own storage | VOWP must never read or copy them. |
| The control cache and tunnels | `~/.vowp-control/` | Snapshot content, spooled events and local forward records. |
| App config and opencode pair links | `~/.vowp/orchestrators/<name>/` | They grant access to the session and its tools. |

Trust boundaries:

- The **user** trusts the home to run only the configured executors, within the configured
  directories, and to stop at a human approval before public actions.
- The **home** is authoritative. A control machine and the apps and MCP instance on it never open a
  VOWP database file; every VOWP write goes to the home (over SSH or through the home's MCP server).
  On the home, the `vowp mcp` server is a `vowp` process: it reads and writes the home database
  directly, and every write tool checks the session lease. The one exception on a control machine is
  its own snapshot cache, spool, surface state and tunnel table.
- VOWP **trusts the harness** to be the authenticated client it is; it does not implement a second
  login or token flow. Harness logins stay under the harness's control.
- VOWP **does not trust repository content or tool output**. A README, an issue body or a test fixture
  can contain instructions aimed at an agent. Gates, ownership and approvals are the backstop; VOWP is
  not a sandbox and does not claim to make an untrusted repository safe.
- The **owner's SSH key** is the control boundary for a phone or laptop. Anyone with the key and the
  session lease can act as that surface.

## The rules and how they are enforced

| Rule | Enforcement |
|---|---|
| **One writer per session** | `sessions/<id>/log.jsonl` is appended only on the home, by the session service or the hidden `session ingest` path. `seq` has no gaps or duplicates; ingest is idempotent by event id. |
| **The home is authoritative** | A control machine and the apps and MCP instance on it never write a VOWP DB file; their commands route to the home. On the home, the `vowp mcp` server reads and writes the home database directly, and every write tool checks the session lease. A control machine keeps only a snapshot cache, a spool and local surface/tunnel state. |
| **No public listener** | The supervisor opens no public HTTP, SSE or API port. The opencode app host and the one-time secret paste page listen on loopback only, and a control machine reaches both through an SSH tunnel; the secret vector is one-time. Claude Remote Control opens no local port: it connects outbound to claude.ai from the home. |
| **Two daemon writes** | The loopback daemon serves every read without a token. `POST /api/v1/inbox/{id}/answer` and the local `POST /api/v1/sessions/{id}/switch` require `Authorization: Bearer <VOWP_API_TOKEN>`; a daemon started without the variable answers `503` and writes nothing. The token is read at startup, compared in constant time, never logged, echoed or put in a URL, and rotation is a daemon restart. |
| **No credentials in configuration** | `vowp.toml` holds no token values; `env_pass` contains environment-variable **names**; `HarnessConfig` has no value fields. VOWP stores the values it manages in the secret store, and a listed `env_pass` name may also be forwarded from the process environment (see [Secrets](#secrets)). `vowp doctor` reports presence by name and never calls the network. This covers configuration and argv, not every stored byte: exact-value redaction covers the session-service and remote-ingest event paths and the snapshot, but `supervisor.log`, gate evidence, ledger rows, the switch turn path and the `session watch` importer are written unredacted and can hold a value a command printed (see [Redaction](#redaction)). |
| **Stock binaries only** | Adapters spawn `claude`, `opencode` or a configured CLI exactly as a user would. No client spoofing, no "sign in with Claude" inside VOWP, no token forwarding. The optional `session watch` importer reads transcript JSONL read-only; it does not read auth files. |
| **cwd allow-list** | Before a run, the session layer checks that the working directory is inside the job worktree, the project path, the worktree root, `[node].cwd_allow` or the VOWP-owned orchestrator root `<VOWP_HOME>/orchestrators/<name>/`. Anything else, including paths elsewhere under `VOWP_HOME`, is refused. |
| **Worktree isolation** | One worktree per job on branch `job/<id>`. The `owns_respected` gate checks the diff and uncommitted files against the job's globs; dispatch-time locks prevent overlapping owners from running together. |
| **One active surface** | A lease row exists at most once per session. A surface without the current token can read in follow mode; every write is refused. A stale lease (60 s without heartbeat) is swept. Takeover drains the old turn, writes a capsule and cuts the old surface off. |
| **MCP writes check the lease** | Every `vowp mcp` write tool calls `surface::check_lease` with the token from the app's project-local token file. A moved session's old app gets the read-only notice; read tools keep working. |
| **Project-local app config** | `vowp orchestrator new` writes only under `~/.vowp/orchestrators/<name>/`, never the user's global app config. The opencode pair link is single-use, expires, and is treated as a secret. |
| **tmux safety** | Only tagged `vowp-` windows are touched. Every window is a read-only view. There is no respawn or keystroke API; `capture-pane` is display-only and never drives a decision. |
| **Guarded promotion** | Guards → serving bank → diff → approval → deploy → probe → rollback record. A refusal is staged, never retried with a weakened check. |
| **Nothing public without a human** | Publishing, deploying, visibility changes, releases and new accounts require an approval inbox item answered by a human, unless the environment is explicitly configured with `approval = "policy"` and the operator passes `--yes`. |
| **Secret scan** | The `secret_scan` gate is in the default gate set for code jobs and also runs on the merge queue's rebased tree before a push. A hit blocks the push and stores only the file, line and rule name. |
| **Provider spend** | A rung not listed in the profile is never used. Credits exhausted parks jobs and opens an inbox item; it never silently switches to another paid provider. |
| **Bounded autonomy** | Continues, hang restarts, fix rounds and repeated actions are bounded. The supervisor cannot be told to "just keep going" past those bounds. |

## Secrets

VOWP refers to secrets by **name** only. That applies to configuration, reports, logs, test fixtures,
error messages, commands and the setup plan.

### Modes and storage

| Mode | When | Storage | Owner |
|---|---|---|---|
| **vault user** | The `vowp-vault` Unix user and the helper exist, and one sudoers line lets the runtime user run the helper as `vowp-vault`. | `/home/vowp-vault/secrets/<NAME>`, mode 0600. | `vowp-vault` |
| **basic** | No sudo or no vault user (common on a VPS the owner does not administer as root). | `~/.vowp/secrets/<NAME>`, mode 0600. | the runtime user (`vowp` or the SSH user) |

`~/.vowp/secrets/mode` contains `vault` when vault mode is active (0600); an absent file means basic
mode, so the no-sudo path writes no marker. `vowp secret upgrade` prints the one sudo command that
creates the vault user and moves the files; without sudo, VOWP stays in basic mode.

**Basic mode does not separate the value from the agent's user.** The secret file is mode 0600 but
owned by the runtime user (`vowp` or the SSH user), and the harness is spawned as that same user, so
the harness can read `~/.vowp/secrets/<NAME>` if it tries. **Vault mode keeps the files out of the
runtime user's direct readable tree:** they live under `vowp-vault` at mode 0600. It is not
isolation from code that can invoke the helper: the sudoers rule lets the runtime user run the
helper as `vowp-vault` with no argument restriction, and the helper can read or write any stored
name and run the caller's command as `vowp-vault`. Treat the helper as the trust boundary, not the
Unix user alone. Neither mode encrypts the store at rest; the Unix account permissions and disk
encryption are the boundary. The vault user is not a hardware keystore.

### Commands

```sh
vowp secret set <NAME>          # hidden prompt; never an argument, never echoed
vowp secret ls                  # name and last-4 only
vowp secret rm <NAME> --yes
vowp secret upgrade
vowp secret request <NAME>      # one-time 127.0.0.1 paste page, URL and expiry
```

- `set` validates the name (`^[A-Z][A-Z0-9_]{0,63}$`), reads the value from a hidden prompt (no TTY →
  exit 5 with "run it in a terminal or use `vowp secret request`"), writes atomically with mode 0600,
  and records only the last four characters in the index. The value is never logged, never in argv and
  never in an event.
- On a control machine, `set/ls/rm/upgrade` route to the home over SSH; `set` uses `ssh -t` so the
  home's prompt has a TTY. The value crosses the encrypted SSH channel and is never in argv.
- `request` binds `127.0.0.1:0`, generates a 32-byte random token, prints a URL with a ten-minute
  expiry, accepts one POST, writes the value through the same store path, returns a page that says
  only `saved` and the last four characters, and closes. The AI sees `NAME saved, ends in …last4`,
  never the value. A request issued from a control machine opens or reuses a local tunnel and returns
  the **local** URL, so the browser never needs the home's port.
- The setup contract never asks for a value. A hand-off names the secret; the owner enters it with the
  hidden prompt or the one-time page.

### Injection

- A harness run's `env_pass` is a list of **names**. VOWP resolves each name from the secret store
  when a value is set there, and otherwise forwards it from the process environment of the machine
  running the turn; the home session service and the remote `__exec` path both do this. A name that
  cannot be resolved is a warning on the home path and an error on a remote surface. Promotion
  commands use the separate `[env.<name>].env_pass` list, also names only. The promoter resolves
  those names from the store once at the start of `promote` and `rollback`, before anything is
  banked or deployed, and passes the values only through the child environments of the serving
  bank, deploy, probe and rollback commands. A configured name that is not stored is an error that
  names the variable and aborts the operation before a command runs; harness `env_pass` keeps its
  warn-and-skip behaviour.
- `__vault-exec` is the hidden read/inject helper: `__vault-exec --env NAME -- <cmd…>` reads the
  named file and execs `<cmd…>` as `vowp-vault` with that one variable set. It is not limited to
  `env_pass` names: any stored name works, and the command runs with the vault user's access. The
  sudoers rule permits the runtime user to invoke the helper with no argument restriction, so the
  helper is the trust boundary; a harness that can call it can reach every stored secret, not only
  the names it was given.
- In vault mode the same helper is the only write path: `__vault-admin set <NAME>` reads the value
  from stdin (never argv), validates the name and writes the file as `vowp-vault`; `rm <NAME>`
  unlinks it. The sudoers rule names the helper binary, not fixed subcommands, and places no
  argument restriction on it; the helper's own parser chooses set/rm/probe/exec.
- In basic mode the helper is a plain exec with the same argv; the value is protected only by the
  runtime user's permissions, and the harness runs as that same user.

### Redaction

`vault::Redactor` loads the exact values (longest first) and replaces every exact occurrence with
`[REDACTED:<NAME>]`. The coverage and timing are:

- every event payload and `raw` appended by the session service or the remote ingest path, before
  `SessionLog::append`;
- the serialized snapshot before writing;
- a capsule body after the writer returns and before the caller sees it;
- the job report and latest run mirror after each turn finishes.

It is **not** applied to the supervisor log, gate command evidence, ledger `detail_json`, the
`session switch` turn path or the `session watch` importer: the supervisor log writer appends
directly, the gate runner inserts `GateResult.evidence` directly, the ledger writer appends `detail`
directly, and those two session paths write the harness event and imported transcript `raw` without
`redact_event`. A supervisor line, command gate, ledger row or event on those paths that contains an
injected value can therefore store it in plain text. The report and run mirror are raw during the
turn and only rewritten when it ends. Avoid commands that echo secret values, and treat all of these
files as sensitive. This is a known gap tracked as a follow-up; do not rely on event/snapshot
redaction to cover it.

The redactor never logs a value or its prefix; it logs only the name. The `secret_scan` gate scans the
changed paths and added lines of a job diff, and the merge queue scans its rebased candidate when
`[merge].secret_scan` is on (the default). Neither is a public-tree audit: before a public release,
the maintainers run a separate scan over the squashed candidate for credentials, absolute home paths,
private hostnames, SSH targets and large blobs, with an allowlist for legitimate matches. That scan is
part of the release process; it is not part of the product and does not run in the user's own
repository.

### The honest limit

**Any agent that can run arbitrary code inside a process that holds a secret can print it.** In vault
mode the files are outside the runtime user's direct readable tree, but the helper is a broad trust
boundary: the sudoers rule has no argument restriction, `__vault-exec` can read any stored name, and
it runs the caller's command as `vowp-vault`. `env_pass` is not an allow-list for the helper. In
basic mode VOWP does not separate them: the secret file is owned by the same user
as the harness, so a harness that reads `~/.vowp/secrets/<NAME>` can recover the value. Neither mode
encrypts the store at rest, and neither can make a secret unreadable to code that is given it. VOWP
keeps values out of configuration and git, and redacts exact injected values on the session-service
and remote-ingest event paths, in the snapshot, in capsules and in the finished report and run
mirror. It does **not** keep them out of every database row or every log: the supervisor log, gate
command evidence, ledger `detail_json`, the switch turn path and the `session watch` importer are
written without redaction. Treat the databases and the home as sensitive. Redaction is a backstop
for the places listed above, not a guarantee.

Also be explicit about the edges:

- Redaction matches the **exact** injected values. It does not catch a value the agent obtains another
  way, a transformed or encoded copy, or a value in a prompt the owner typed into the conversation.
- `secret_scan` covers the named patterns below. A custom or unusual credential can still pass.
- Harness logins (`~/.claude`, opencode credentials) stay under the harness's own control. VOWP does
  not read or copy them, and their security is the harness's.
- A secret pasted into chat is already in the model provider's transcript. If that happens, rotate it
  and use the hidden prompt or the one-time page.

### Secret-scan rules

The gate scans **added lines** in the job diff and secret-looking file paths. The matched text is never
stored; a hit records `{path, line, rule}` only.

| Rule name | What it catches |
|---|---|
| `sk_api_key` | OpenAI/Anthropic-style `sk-` API keys. |
| `supabase_secret` | Supabase secret keys. |
| `github_token` | Classic GitHub tokens. |
| `github_pat` | Fine-grained GitHub personal access tokens. |
| `aws_access_key` | AWS access key ids. |
| `private_key_block` | PEM private-key blocks. |
| `slack_token` | Slack bot/user/app tokens. |
| `secret_file` | A diff path matching `.env*`, `*.pem`, `id_rsa*` or `id_ed25519*` (line 0). |

A `.env` file is never read, printed or committed, even when it would be convenient for a test. Use a
temporary directory and a fake value.

## The daemon write token

The v0.3 loopback daemon (`vowp-daemon`) is read-only by default: every `GET` is token-free and
served from `Store::open_read_only`. The two exceptions are
`POST /api/v1/inbox/{id}/answer` and the local `POST /api/v1/sessions/{id}/switch`. Both are
gated by one bearer token, not by a user account.

- **Where the value lives.** The daemon reads `VOWP_API_TOKEN` from its own environment at
  startup. The value is not in `vowp.toml`, not in a token file and not in a URL. A missing or
  empty value disables writes: the route answers `503` and writes nothing. There is no
  placeholder mode that accepts an unauthenticated write.
- **Transport.** The request carries `Authorization: Bearer <token>`. A missing or wrong value
  is `401` with `WWW-Authenticate: Bearer`. The comparison is constant-time and the configured
  value is never logged, echoed or returned. The handlers check the token before they parse the
  body or load the row, so an unauthorized request cannot probe item or session ids or states.
- **Rotation.** Restart the daemon with a new `VOWP_API_TOKEN`. There is no token file, rotation
  service, multi-user account or per-user permission. The page keeps the value in
  `sessionStorage`; a `401` clears it and shows the unlock panel again.
- **Scope.** Both writes call `Store::open_writable`, which opens the existing home without
  `ensure` or a migration and refuses a missing or unversioned database. The answer stores the
  item with `answered_by = "web"` and, when the item belongs to a job, queues the same
  `answer_inbox` request as the CLI; the supervisor applies the request and the browser never
  changes job state directly. The switch runs the same `SessionService` switch as
  `vowp session switch` against that store, with the CLI's guards and a five-second drain.
  It is local-node only: the request has no node field and the daemon opens no SSH path.
- **Force.** A running session is refused with `409` unless the request sets
  `force: true`. Force lets the current turn drain for up to five seconds and then stops it;
  the page shows the returned state and never claims the executor moved when the switch was
  aborted. A `switching` or `closed` session and a target provider with an active limit are
  also refused with `409`, mirroring the CLI.
- **Exposure.** The listener binds `127.0.0.1` only. This is not TLS, not a multi-user login and
  not safe to expose on a public interface. A local process that can read the daemon's
  environment can read the token; reach the daemon through an SSH tunnel and treat the token as
  a secret value. Tests use a fixed fake token in-process and never read or set the real
  environment.

The rest of the API has no auth surface. Job retry/park/cancel, cross-node switching, capsule
edits and every other write remain later and must not reuse this token as a blanket service
credential without a new design.

## Session logs and the control cache are sensitive

`~/.vowp` and `~/.vowp-control` are node state, never committed. They can contain:

- the prompts you typed and the model's replies;
- tool output, including file contents, diffs and command output;
- file changes and attachments;
- limit messages that may include account details before redaction;
- app transcripts mirrored into the canonical log;
- snapshot content and spooled events on a control machine.

Treat both directories like a work journal: back them up if you need them, restrict access to your
user account, and never paste raw logs into a public issue. `vowp backup` includes the session content
but not secrets; the control cache is disposable and is not part of a home backup.

VOWP does not encrypt `~/.vowp` or `~/.vowp-control` at rest. Disk encryption and account permissions
are your responsibility.

## Filesystem and process isolation

- **One worktree per job.** A worker's `cwd` is its worktree, not the main checkout. The merge queue is
  the only writer to the default branch.
- **Ownership locks.** Dispatch refuses overlapping `owns` globs. `owns_respected` checks both committed
  and uncommitted paths, so a file written but not committed is still a violation.
- **No `sudo` for workers.** The framework's agent permissions deny `sudo` and `git push` to workers;
  the supervisor itself runs as the user and does not install system packages. The vault helper is
  the one broad exception: the sudoers rule lets the runtime user run it as `vowp-vault` with no
  argument restriction, so it can read or write any stored secret and run commands as `vowp-vault`.
  Treat the helper as a trust boundary, not a narrow one.
- **Timeouts and process groups.** Command gates have timeouts and kill the process group, not just the
  immediate child, so a shell cannot leave an orphan running; harness runs are stopped through the
  adapter's interrupt/stop path.
- **Capacity never kills work.** Admission and brakes stop new runs; capacity itself does not signal
  an existing run. Other paths can stop a run: switch/takeover, `vowp down`, `vowp job park` and
  `vowp job cancel`, and stall detection or a keeper restart. Those are explicit operator, keeper or
  lifecycle actions, not capacity decisions.
- **No real home/network in tests.** The test suite uses temp directories, a private tmux socket **and**
  a private `TMUX_TMPDIR`, fake harness scripts and recorded fixtures. Running `cargo test` is safe
  with a live tmux server: the tests remove `TMUX`, pass a private `-L` socket (`vowp-<hash>` derived
  from the home in CLI tests, `vowp-test-…` in the tmux crate) and assert the resolved socket is not
  the default one.

## Apps, MCP and leases

- The opencode app host runs on the home and binds loopback. The desktop joins through `vowp tunnel`
  and the app's own pairing flow; the phone runs `opencode --server` against the loopback port over
  SSH. No public port is opened. The opencode pair link is single-use and expires in five minutes;
  treat it as a secret. Claude Remote Control also runs on the home, but it opens no local port: the
  `claude remote-control` process connects outbound to claude.ai, and the desktop joins at
  `https://claude.ai/code` or the phone in the Claude mobile app. No VOWP tunnel or pair link is
  involved.
- `vowp orchestrator new` writes only the orchestrator's project-local config under
  `~/.vowp/orchestrators/<name>/`: `opencode.json` or `.mcp.json`, `AGENTS.md`, `orchestrator.json`,
  the lease token, host state, the transcript cursor (`mirror.json`), the keeper log
  (`keeper.log`) and the keeper pid (`keeper.pid`). It does not edit the user's global app config.
- `vowp mcp` is a stdio server. Every write tool reads the token file and checks the session lease; a
  stale token cannot write. The actor is recorded as `session:<id>/surface:<surface-id>`.
- The old surface of a takeover is not closed by VOWP (it cannot close another vendor's window). Its
  VOWP write tools refuse with the takeover notice; its reads continue.
- The opencode build may report an MCP server as connected without exposing its tools to the model.
  Configuration is not proof of tool availability; `vowp chat` and Claude Code are the proven tool
  paths. Do not treat "connected" as a security control.

## tmux

The window manager is deliberately narrow:

- VOWP uses its **own private tmux server**, not your default server. The socket name is derived from
  the home (`vowp-<hash>`) and every internal command runs with `-L <socket>` and
  `TMUX_TMPDIR=<home>/tmux`. A command with no private socket, or with the literal socket `default`,
  is refused before tmux is spawned.
- `vowp attach` execs its tmux client through
  `env -u TMUX -u TMUX_PANE TMUX_TMPDIR=<home>/tmux`, so it reaches the private server even when VOWP
  was started from inside your own tmux session.
- At `vowp up` the socket and the tmux server pid are recorded in `<home>/tmux-server`.
  `vowp down --kill-windows`, `kill_session` and `kill_window` refuse when the recorded socket or pid
  does not match the live server, so a server VOWP did not create is never cleaned up.
- It creates or attaches only the `vowp-home` and `vowp-<project>` sessions and tagged windows.
- It never kills untagged windows, never respawns a pane and never sends keystrokes. There is no
  `kill-server` command in the product path; only explicit private-socket test guards and the
  `hello-vowp` script's own private cleanup may stop a server they created.
- A job window runs `vowp __window job <id>`, which is a read-only follower of the session log. The
  harness runs headless; the window is not a control channel.
- `capture-pane` is for display only. Decisions come from the log and the database, never from screen
  text.

Contributors and agents must follow the same rule in manual repros: never run a tmux command against
the default server; use a private `-L <unique>` socket and `TMUX_TMPDIR`, and never `-L default`.
Create the `TMUX_TMPDIR` directory before the first command: tmux silently falls back to the default
server when the named directory does not exist.

## SSH, control machines and tunnels

- A control machine needs key-only SSH to the home. `BatchMode=yes` means a missing key fails instead
  of prompting; a failed key check is a stop, not a reason to enable passwords.
- `vowp control init` writes `~/.vowp-control/control.json` (0600) with the home target and the absolute
  remote home. A control machine installs no supervisor and opens no database.
- `vowp tunnel open` spawns `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o
  ServerAliveInterval=15 -L 127.0.0.1:<local>:<host>:<remote>` in its own process group and records
  the pid in `~/.vowp-control/tunnels.json` (0600). Tunnels are local to the machine that uses them;
  the home refuses `vowp tunnel`.
- `--node` re-executes a command over SSH with `BatchMode=yes` and a connect timeout. It does not open
  a port or send a token. Arguments are single-quote escaped into one remote command string; no
  argument is interpolated unquoted.
- The remote account needs its own harness logins and its own provider environment. VOWP does not copy
  them.
- A session moves between machines as a checksummed bundle, not as a live connection. The origin is
  never modified by an import.
- The phone is only as safe as its SSH key. Revoke a lost phone's key in `authorized_keys`; the
  session itself is unchanged.

## Promotion guardrails

- Guards run before the serving bank; a failure refuses the release and deploys nothing.
- The serving bank snapshots what is live before the deploy.
- The candidate is diffed against the banked copy; unexpected files refuse the release.
- Approval is a human inbox item by default. `--yes` is honoured only for `approval = "policy"`; it
  never bypasses a human approval.
- Deploy and probe must both pass. A probe failure records `failed` and keeps the rollback available.
- Every terminal state appends a line to `~/.vowp/releases.log` with the rollback command.

## Lifecycle and backup

- `production`, `truth`, `open_work` and `secrets` are never auto-deleted. `vowp gc --apply` only
  collects `closed_session`, `run_output`, `worktree`, `sample` and expired `cache` entries.
- A backup includes the global database, every project database, the session content and the catalog,
  with a manifest of SHA-256 hashes and row counts. It **never** includes secrets, encrypted or not;
  restore them with `vowp secret set`.
- `--encrypt` uses the external `age` binary. A missing tool is a clear exit 5, never a silent
  unencrypted copy. The external `rsync` and `git` handle offsite copies with argv, not a shell.
- Restore verifies hashes and row counts in a staging directory before swapping a fresh home into
  place; a corrupt backup leaves the target untouched.
- A control machine's snapshot cache and spool are disposable and never backed up by the home.

## What VOWP does not protect against

Be explicit about the limits:

- **A malicious repository.** An agent may follow instructions embedded in code, comments, issues or
  tool output. VOWP's gates catch unowned files, failing checks and obvious secrets; they do not stop a
  determined prompt-injection attack that stays inside its owned paths and passes its tests. Review
  briefs and run VOWP on repositories you trust.
- **A compromised harness or provider.** VOWP drives the official binary; if that binary or the model
  provider is compromised, VOWP cannot make it safe.
- **A malicious local user or a stolen control machine.** VOWP stores state in the home directory with
  the user's permissions and trusts the SSH key. It is not a multi-tenant sandbox. A stolen laptop or
  phone with the key and a live lease can act as that surface.
- **All secret formats or all leaks.** The scan covers the named rules; redaction covers exact injected
  values. Neither is a data-loss-prevention system. Code that is given a secret can print it.
- **Production correctness.** Guards and probes catch the cases they are configured to catch. They are
  not a substitute for reviewing the change.
- **Network egress.** The supervisor opens no public listening sockets and makes no model-provider
  calls, but it is not offline. The merge queue runs `git fetch` and `git push` against the configured
  remote, `--node` runs commands over SSH, the opencode app host binds loopback, Claude Remote
  Control connects outbound to claude.ai, and the harnesses it starts make their own provider calls.
  Restrict egress at the OS or network level if that matters to you.
- **The VPS provider.** The owner is responsible for the server, its disk, its snapshots and its
  billing. VOWP does not take provider-level backups.

## Hardening checklist

- [ ] Run the home as a dedicated non-root user with access only to the projects it needs.
- [ ] Use key-only SSH in batch mode; disable password authentication once keys work.
- [ ] Keep `~/.vowp` and `~/.vowp-control` out of shared backups and out of any repository.
- [ ] Open no public port. Reach the opencode app host and the secret page through `vowp tunnel` only;
      Claude Remote Control uses an outbound connection to claude.ai and no tunnel.
- [ ] Put provider values in the secret store or the service environment; never in `vowp.toml`, a
      command argument or a chat.
- [ ] Prefer vault mode; if you use basic mode, accept that the runtime user owns the files and say so
      in the setup report. In either mode, treat the vault helper and its sudoers rule as the secret
      boundary.
- [ ] Review `[node].cwd_allow`; keep it as small as the work requires.
- [ ] Review `[profiles.*].ladder`; every rung is a provider you are willing to pay or quota-limit.
- [ ] Keep `[project].check_cmds` meaningful: the merge queue is the last automated check before
      `main`.
- [ ] Keep `[merge].secret_scan = true`.
- [ ] Use `approval = "human"` for production; do not set `policy` just to avoid answering the inbox.
- [ ] Back up before upgrading; test the restore path and the migration rollback.
- [ ] Treat session logs, app transcripts, the control cache and opencode pair links as sensitive.
- [ ] Revoke a lost phone or laptop key; the lease and the key are the access.
