# v0.2 / v0.3 — cross-node session sync

Status: current for the commands shown; **partly superseded by v0.2**. v0.2's remote execution (`docs/design/v0.2/home-and-surfaces.md`
§7.2) keeps one session on the home and moves only the **executor** to another machine. That is the
product behaviour "the same conversation from the laptop and the phone"; a session no longer has to
be copied between nodes for a human to continue it.

This note now records two things:

1. what v0.2 ships instead of a bundle move;
2. what remains a real v0.3 problem for two homes or offline copies.

## What v0.2 ships

- **One home owns the log.** A laptop surface runs `vowp __exec`, streams the harness events over
  SSH into `vowp session ingest` on the home, and the home assigns `seq` and appends. A drop is
  spooled and replayed; ingest is idempotent by event id. The same session id and the same log are
  used on every surface.
- **The session does not fork.** There is no child session, no second log and no import for the
  laptop case. The v0.1 bundle remains for an explicit export/import to another installation.
- **A lease decides who writes.** The old surface is cut off with a notice and follows read-only;
  the new surface continues from the capsule. See the v0.2 spec §7.3.
- **A bundle move still exists** for an installation that is offline or for a deliberate copy to a
  second home: `vowp session export` / `session import` behave exactly as in v0.1 (`vowp-bundle-v1`,
  checksum, child session, capsule + tail, inline blobs ≤ 64 KiB).

So the v0.1/v0.2 answer to "move a session between nodes without hand-copying a file" is: you do
not move it; you move the executor. The bundle is the escape hatch, not the everyday path.

## What remains for v0.3

- **`session pull <node> <session>` / `session push`** as convenience wrappers around the existing
  bundle and `--node` transport, for two-home or offline use.
- **Directory copy.** Copy `sessions/<id>/` (log, blobs, capsules, `adapters.json`) with a manifest,
  checksums and a size cap, then run `vowp reindex`; preserve the full log rather than the bundle's
  capsule + tail.
- **Same-id import.** A pull that preserves the origin id when it is absent locally. This needs
  duplicate detection and reindex collision rules.
- **Divergence reporting, never a silent merge.** If both sides advanced the same session, report
  the two heads and require an explicit choice.
- **Daemon transport.** The optional second transport after the v0.3 HTTP API exists. SSH stays the
  only v0.2/v0.3 transport until then.

## Non-goals (unchanged)

- A distributed database, leader election or two supervisors writing one project.
- Automatic conflict resolution when both nodes advanced the same session.
- Copying a running job's worktree or uncommitted files. A job stays homed on one node.
- A hosted relay.

## Existing crates it builds on

- `vowp-core::session::bundle` — `vowp-bundle-v1`, checksum, capsule, tail and inline blobs.
- `vowp-core::session::service` — import creates a child session and seeds the first turn.
- `vowp-store::session_log` and `vowp-store::reindex` — the log is authoritative and the index is
  rebuildable.
- `vowp-cli::remote` — `build_ssh_argv`, `strip_node_flag` and the exit-code mapping.
- `vowp-cli::cmd::session` — `remote_export`, `remote_import` and `remote_switch` already stream a
  bundle over SSH.
- The `nodes` table — name, SSH target, capabilities and last-seen time.

## Open questions (v0.3)

1. **Child vs same id.** Preserving the origin id simplifies lineage but makes duplicate detection
   and `reindex` collisions a new problem. v0.3 starts with the v0.1 child rule and adds a
   same-id flag only after the duplicate tests exist.
2. **Direction.** Explicit `pull`/`push` first; a bidirectional `session sync` only with a visible
   divergence report. Never guess a merge.
3. **Blobs.** A directory copy needs a size limit, a progress display and a clear "blob
   unavailable" marker for the rest.
4. **Auth and paths.** The remote home, project and cwd may differ. The bundle already carries
   `cwd`; import must keep the existing allow-list check.
5. **Offline queue.** A push that fails should be retryable without duplicating a child; decide
   whether the request is idempotent by content checksum.
6. **Lease interaction.** A session pulled to a second home cannot have two live leases. v0.3 must
   define whether a pull requires the source lease to be released or is a read-only snapshot.

## First v0.3 slice

Add `session pull <node> <session>` that calls the remote export over the existing SSH argv builder,
writes the bundle to a temporary file, imports it as a child and prints both session ids. Test with
the existing fake-`ssh` harness and a temporary remote home; the test must fail if the bundle
checksum is not verified or if the child's `parent_session_id` is wrong.

No directory copy, no daemon transport and no bidirectional merge in the first slice. This is
independent of v0.2 and does not block the v0.2 release.
