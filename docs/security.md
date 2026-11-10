# Security model

VOWP runs agents that edit code, execute commands and, through promotion, can change what is live. The
security model is therefore built around four limits: VOWP holds no credentials, it only drives stock
harnesses, every job is confined to its own worktree and owned paths, and any public or production
action needs an explicit human approval by default.

This page describes the model and what it does **not** protect against. The vulnerability-reporting
policy is in [`SECURITY.md`](../SECURITY.md).

> **Status: current.** Gates, promotion, `vowp doctor`, the supervisor and the session commands on this
> page are implemented, and these rules are binding on the code on `main`.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/syver777/vowp/security/advisories/new>

Do not open a public issue, discussion or pull request. Include the affected version or commit, the
operating system, reproduction steps and the impact you expect. Do not include credentials or other
secret values; refer to secrets by name only.

VOWP is pre-release. Security fixes land on `main` and in the latest `0.1.x` pre-release; older
pre-release builds are not supported.

## Assets and trust boundaries

| Asset | Where it lives | Why it matters |
|---|---|---|
| Source code and production environments | Project checkouts, worktrees, deploy targets | An agent can change them. |
| Prompts, tool output, file contents | Session logs, blobs, run mirrors under `~/.vowp` | They often contain sensitive code or data. |
| Provider spend | Harness accounts, environment variables | A runaway loop costs money. |
| Harness logins | Claude Code/opencode's own storage | VOWP must never read or copy them. |
| The job graph and evidence | `vowp.db`, briefs, reports | Decisions and audit trail. |

Trust boundaries:

- The **user** trusts VOWP to run only the configured executors, within the configured directories, and
  to stop at a human approval before public actions.
- VOWP **trusts the harness** to be the authenticated client it is; it does not implement a second login
  or token flow.
- VOWP **does not trust repository content or tool output**. A README, an issue body or a test fixture
  can contain instructions aimed at an agent. Gates, ownership and approvals are the backstop; VOWP is
  not a sandbox and does not claim to make an untrusted repository safe.

## The rules and how they are enforced

| Rule | Enforcement |
|---|---|
| **Local only** | The supervisor opens no listening sockets in v0.1. There is no HTTP API. Remote access is SSH through the `--node` flag; the v0.2 API will bind `127.0.0.1` only. |
| **No credentials stored** | `vowp.db`, logs and reports hold no tokens. `env_pass` contains environment-variable **names**; `HarnessConfig` has no value fields. `vowp doctor` reports presence by name and never calls the network. |
| **Stock binaries only** | Adapters spawn `claude`, `opencode` or a configured CLI exactly as a user would. No client spoofing, no "sign in with Claude" inside VOWP, no token forwarding. The optional `session watch` importer reads transcript JSONL read-only; it does not read auth files. |
| **cwd allow-list** | Before a run, the session layer checks that the working directory is inside the job worktree, the project path, the worktree root or `[node].cwd_allow`. Anything else is refused. |
| **Worktree isolation** | One worktree per job on branch `job/<id>`. The `owns_respected` gate checks the diff and uncommitted files against the job's globs; dispatch-time locks prevent overlapping owners from running together. |
| **tmux safety** | Only tagged `vowp-` windows are touched. Every window is a read-only view. There is no respawn or keystroke API; `capture-pane` is display-only and never drives a decision. |
| **Guarded promotion** | Guards → serving bank → diff → approval → deploy → probe → rollback record. A refusal is staged, never retried with a weakened check. |
| **Nothing public without a human** | Publishing, deploying, visibility changes, releases and new accounts require an approval inbox item answered by a human, unless the environment is explicitly configured with `approval = "policy"` and the operator passes `--yes`. |
| **Secret scan** | The `secret_scan` gate runs on every job diff and again on the merge queue's rebased tree before a push. A hit blocks the push and stores only the file, line and rule name. |
| **Provider spend** | A rung not listed in the profile is never used. Credits exhausted parks jobs and opens an inbox item; it never silently switches to another paid provider. |
| **Bounded autonomy** | Continues, hang restarts, fix rounds and repeated actions are bounded. The supervisor cannot be told to "just keep going" past those bounds. |

## Secrets

VOWP refers to secrets by **name** only. That applies to configuration, reports, logs, test fixtures,
error messages and commands.

- A harness may need a provider key. You set the environment variable in the shell that starts the
  supervisor or harness, exactly as the harness expects. VOWP does not read the value.
- `[harnesses.<name>].env_pass` is a list of **names** to forward to a child process. It is not a place
  to put values.
- `vowp doctor` prints `present`/`absent` for a named variable, never its content, and never makes a
  network call to test it.
- `vowp_adapters::classify::redact` removes secret-looking values from limit and run detail strings
  before storage. This is a backstop, not a guarantee that all logs are safe to publish.
- Reports and inbox answers must not contain secret values. If a task seems to need one, the agent must
  stop and ask the owner.

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

## Session logs are sensitive

`~/.vowp` is node state, never committed. Session logs and blobs can contain:

- the prompts you typed and the model's replies;
- tool output, including file contents, diffs and command output;
- file changes and attachments;
- limit messages that may include account details before redaction.

Treat `~/.vowp` like a work journal: back it up if you need it, restrict access to your user account,
and never paste raw logs into a public issue. `vowp backup` copies the database, not the
session directories; if you need the logs, copy `~/.vowp` while the supervisor is stopped or use a
filesystem snapshot.

VOWP does not encrypt `~/.vowp` in v0.1. Disk encryption and account permissions are your
responsibility.

## Filesystem and process isolation

- **One worktree per job.** A worker's `cwd` is its worktree, not the main checkout. The merge queue is
  the only writer to the default branch.
- **Ownership locks.** Dispatch refuses overlapping `owns` globs. `owns_respected` checks both committed
  and uncommitted paths, so a file written but not committed is still a violation.
- **No `sudo`.** The framework's agent permissions deny `sudo` and `git push` to workers; the supervisor
  itself runs as the user and does not install system packages.
- **Timeouts and process groups.** Command gates have timeouts and kill the process group, not just the
  immediate child, so a shell cannot leave an orphan running; harness runs are stopped through the
  adapter's interrupt/stop path.
- **No real home/network in tests.** The test suite uses temp directories, a private tmux socket **and** a
  private `TMUX_TMPDIR`, fake harness scripts and recorded fixtures. Running `cargo test` is safe with a
  live tmux server: the tests remove `TMUX`, pass a private `-L` socket (`vowp-<hash>` derived from the
  home in CLI tests, `vowp-test-…` in the tmux crate) and assert the resolved socket is not the default
  one. This keeps CI from touching a developer's state, not just from leaking secrets.

## tmux

The window manager is deliberately narrow:

- VOWP uses its **own private tmux server**, not your default server. The socket name is derived from
  the node home (`vowp-<hash>`) and every internal command runs with `-L <socket>` and
  `TMUX_TMPDIR=<node-home>/tmux`. A command with no private socket, or with the literal socket
  `default`, is refused before tmux is spawned.
- `vowp attach` execs its tmux client through
  `env -u TMUX -u TMUX_PANE TMUX_TMPDIR=<node-home>/tmux`, so it reaches the private server even when
  VOWP was started from inside your own tmux session.
- At `vowp up` the socket and the tmux server pid are recorded in `<node-home>/tmux-server`.
  `vowp down --kill-windows`, `kill_session` and `kill_window` refuse when the recorded socket or pid
  does not match the live server, so a server VOWP did not create is never cleaned up.
- It creates or attaches only the `vowp-<project>` session and tagged windows.
- It never kills untagged windows, never respawns a pane and never sends keystrokes. There is no
  `kill-server` command in the product path; only explicit private-socket test guards and the
  `hello-vowp` script's own private cleanup may stop a server they created.
- A job window runs `vowp __window job <id>`, which is a read-only follower of the session
  log. The harness runs headless; the window is not a control channel.
- `capture-pane` is for display only. Decisions come from the log and the database, never from screen
  text.

If tmux is absent, `vowp up --no-tmux` runs without windows. `vowp attach <job-id>` falls back to
tailing the job's latest session log.

Contributors and agents must follow the same rule in manual repros: never run a tmux command
against the default server; use a private `-L <unique>` socket and `TMUX_TMPDIR`, and never
`-L default`. Create the `TMUX_TMPDIR` directory before the first command: tmux silently
falls back to the default server when the named directory does not exist.

## Multi-node and SSH

- The `--node <name>` global flag re-executes the same command over SSH with `BatchMode=yes` and a
  connect timeout. It does not open a port or send a token.
- Arguments are single-quote escaped into one remote command string; no argument is interpolated
  unquoted.
- The remote account needs its own harness logins and its own provider environment. VOWP does not copy
  them.
- A session moves between nodes as a bundle with a checksum, not as a live connection.

## Promotion guardrails

- Guards run before the serving bank; a failure refuses the release and deploys nothing.
- The serving bank snapshots what is live before the deploy.
- The candidate is diffed against the banked copy; unexpected files refuse the release.
- Approval is a human inbox item by default. `--yes` is honoured only for `approval = "policy"`; it
  never bypasses a human approval.
- Deploy and probe must both pass. A probe failure records `failed` and keeps the rollback available.
- Every terminal state appends a line to `~/.vowp/releases.log` with the rollback command.

## What VOWP does not protect against

Be explicit about the limits:

- **A malicious repository.** An agent may follow instructions embedded in code, comments, issues or
  tool output. VOWP's gates catch unowned files, failing checks and obvious secrets; they do not stop a
  determined prompt-injection attack that stays inside its owned paths and passes its tests. Review
  briefs and run VOWP on repositories you trust.
- **A compromised harness or provider.** VOWP drives the official binary; if that binary or the model
  provider is compromised, VOWP cannot make it safe.
- **A malicious local user.** VOWP stores state in your home directory with your user's permissions. It
  is not a multi-tenant sandbox.
- **All secret formats.** The scan covers the named rules above. A custom or unusual credential can
  still pass. Do not rely on the scan as the only control.
- **Production correctness.** Guards and probes catch the cases they are configured to catch. They are
  not a substitute for reviewing the change.
- **Network egress.** The supervisor opens no listening sockets and makes no model-provider calls, but it
  is not offline. The merge queue runs `git fetch` and `git push` against the configured remote, the
  `--node` flag runs commands over SSH, and the harnesses it starts make their own provider calls.
  Restrict egress at the OS or network level if that matters to you.

## Hardening checklist

- [ ] Run the supervisor as a dedicated user or account with access only to the projects it needs.
- [ ] Keep `~/.vowp` out of backups you share and out of any repository.
- [ ] Use SSH keys with batch mode for remote nodes; do not put passwords in commands.
- [ ] Keep provider keys in the environment or the harness's own store, never in `vowp.toml`.
- [ ] Review `[node].cwd_allow`; keep it as small as the work requires.
- [ ] Review `[profiles.*].ladder`; every rung is a provider you are willing to pay or quota-limit.
- [ ] Keep `[project].check_cmds` meaningful: the merge queue is the last automated check before `main`.
- [ ] Keep `[merge].secret_scan = true`.
- [ ] Use `approval = "human"` for production; do not set `policy` just to avoid answering the inbox.
- [ ] Back up the database before upgrading; test the rollback path.
- [ ] Treat session logs as sensitive and rotate them with your normal retention policy.
