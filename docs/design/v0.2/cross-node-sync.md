# v0.2 — cross-node session sync

Status: current for the commands shown; this note is design only. v0.1 moves a session as an explicit
`vowp-bundle-v1` file or a cross-node switch; it does not replicate the database or the session
directory.

## Goal

Move a session between nodes without the user copying a bundle file by hand, while keeping one
canonical log per session and the existing recovery rules:

- A planned `session pull <node> <session>` subcommand fetches a session from a registered node and
  creates a local child session.
- A planned `session push <node> <session>` subcommand sends a local session to another node for
  continuation there.
- A later slice keeps a session directory in sync incrementally (new log lines and blobs) and reports
  divergence instead of silently merging.
- Lineage stays visible: the child keeps `parent_session_id`, the switch rows and the source node.

The transport starts with SSH because `--node` already has a tested argv builder and a fake-`ssh`
harness. The daemon API is an optional second transport after [`http-api.md`](http-api.md) exists.

## Non-goals

- A distributed database, leader election or two supervisors writing one project.
- Automatic conflict resolution when both nodes advanced the same session.
- Copying a running job's worktree or uncommitted files. A job stays homed on one node in v0.2.
- A hosted relay (v0.3).

## Existing crates it builds on

- `vowp-core::session::bundle` — `vowp-bundle-v1`, checksum, capsule, tail and inline blobs ≤ 64 KiB.
- `vowp-core::session::service` — import creates a child session and seeds the first turn.
- `vowp-store::session_log` and `vowp-store::reindex` — the log is authoritative and the index is
  rebuildable.
- `vowp-cli::remote` — `build_ssh_argv`, `strip_node_flag` and the exit-code mapping.
- `vowp-cli::cmd::session` — `remote_export`, `remote_import` and `remote_switch` already stream a
  bundle over SSH.
- The `nodes` table — name, SSH target, capabilities and last-seen time.

## Two possible slices

1. **Bundle move (small).** `session pull` runs the remote `vowp session export <id> --out -`, writes
   the bundle locally and imports it as a child. It reuses every v0.1 invariant and loses only the
   full history (the bundle carries a capsule plus the recent tail). This is the first slice.
2. **Directory copy (larger).** Copy `sessions/<id>/` (log, blobs, capsules, `adapters.json`) over a
   stream, then run `vowp reindex`. It preserves the full log but needs a manifest, checksums, a size
   cap and a rule for a session that already exists locally. Keep it behind a flag until it is tested.

## Open questions

1. **Child vs same id.** The v0.1 import creates a child with a fresh ULID. Should a pull preserve
   the origin id when it is absent locally? Preserving it simplifies lineage but makes duplicate
   detection and `reindex` collisions a new problem.
2. **Direction.** Is `pull`/`push` enough, or should a bidirectional `session sync <node> <id>` exist?
   Start with explicit pull and push; never guess a merge.
3. **Blobs.** Inline only small blobs today. A directory copy needs a size limit, a progress display
   and a clear "blob unavailable" marker for the rest.
4. **Auth and paths.** The remote home, project and cwd may differ. The bundle already carries `cwd`;
   import must keep the existing allow-list check.
5. **Offline queue.** A push that fails should be retryable without duplicating a child; decide whether
   the request is idempotent by content checksum.

## First-issue-sized slice

Add the planned `session pull <node> <session>` command (not in the v0.1 CLI) that calls the remote
export over the existing SSH argv builder, writes the bundle to a temporary file, imports it as a child
and prints both session ids. Add a test that uses the existing fake-`ssh` harness and a temporary
remote home; the test must fail if the bundle checksum is not verified or if the child's
`parent_session_id` is wrong.

No directory copy, no daemon transport and no bidirectional merge in the first slice.
