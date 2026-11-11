# Security policy

VOWP is an early release (0.2.0). It runs AI coding agents that can edit files and execute commands, so
the security model is deliberately narrow: no credentials, stock harness binaries, private tmux servers
and loopback-only local state.

## Supported versions

Security fixes land on `main` and in the latest `0.2.x` release. Pre-0.2 builds and older pre-release
builds are not supported. There is no security team and no response-time guarantee; reports are handled
on a best-effort basis by the repository owner.

## Report a vulnerability privately

Use GitHub private vulnerability reporting once the repository is public:

<https://github.com/syver777/vowp/security/advisories/new>

Do not open a public issue, discussion or pull request for a vulnerability. In the report, include the
affected version or commit, the operating system, reproduction steps, and the impact you expect. Do not
include credentials or other secret values; refer to secrets by name only. If GitHub private reporting is
not available yet, wait until the repository is public and use the same link.

## Threat model

1. **No credential storage.** VOWP stores no provider keys, tokens or harness logins. Keys stay in the
   environment or in each harness's own store; VOWP passes environment-variable *names*, never values.
2. **Stock harness binaries.** VOWP drives the official `claude`, `opencode` or configured CLI through
   its documented headless/JSON interface. It does not fork, patch, spoof or scrape the harness, and it
   does not read the harness's auth files.
3. **Private tmux sockets.** Every VOWP tmux server uses a socket derived from the node home, plus a
   private `TMUX_TMPDIR`; a missing or `default` socket is refused before tmux is spawned. VOWP never
   drives the user's default server.
4. **Loopback-only.** In v0.2 the supervisor opens no public HTTP, SSE or API port. The opencode app host
   and the one-time secret paste page bind `127.0.0.1` only; remote access is expected through SSH or a
   tunnel, never a public port.
5. **Local user trust.** VOWP runs as the invoking user and stores state under the node home (usually
   `~/.vowp`) with that user's permissions. It is not a multi-tenant sandbox; protect the account and
   the filesystem.
6. **Untrusted repository and model output.** Code, comments, issues, prompts and tool output can carry
   instructions aimed at an agent. VOWP's gates, owned-path checks and approvals are backstops, not a
   guarantee against prompt injection inside an allowed path.
7. **Git and remotes.** Workers run in their own worktrees; the merge queue is the only writer of the
   default branch; the secret scan runs again on the rebased tree before a push. Git credentials belong
   to the user's Git configuration, not to VOWP.
8. **Guarded promotion.** Guards, a serving snapshot, a diff, human approval by default, deploy, probe and
   a recorded rollback gate every Work → Production change. A refusal is staged, never retried with a
   weakened check.
9. **Bounded autonomy.** Continues, restarts, fix rounds and repeated actions are bounded. A provider
   limit parks a job; it does not silently switch to another paid provider.
10. **Supply chain and releases.** Rust dependencies are pinned by `Cargo.lock`; the release workflow
    builds archives from a tag with SHA-256 checksums, and the release process runs `vowp --version` on each
    artifact before publishing.

## Scope

Reports about the VOWP crates, workflows and bundled framework files are in scope. Vulnerabilities in
third-party harnesses or model providers should be reported to those projects directly. The detailed
security model, including what VOWP does **not** protect against, is in [`docs/security.md`](docs/security.md).
