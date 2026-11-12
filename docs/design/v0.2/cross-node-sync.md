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

- **`session push <node> <session>`** as the remaining convenience wrapper around the existing
  bundle and `--node` transport, for two-home or offline use. `session pull <node> <session>` landed
  in the first slice below.
- **Directory copy.** Landed in the third slice below as `session pull <node> <session> --full`:
  it copies `sessions/<id>/` (log, blobs, capsules, `adapters.json`) with a manifest, checksums and
  a 1 GiB cap, then runs `vowp reindex`; the full log is preserved rather than the bundle's capsule
  + tail.
- **Divergence reporting, never a silent merge.** Landed in the fourth slice below for
  `--same-id` and `--full`: if both sides advanced the same session, report the two heads and
  require an explicit choice. Automatic merge and a force flag remain out of scope.
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

1. **Child vs same id.** The v0.1 child rule is still the default. The second slice adds an
   explicit `--same-id` mode: it preserves the origin id only when it is absent locally, refuses a
   duplicate, and treats a `reindex` event-id collision as a conflict. The fourth slice reports the
   two heads on a duplicate instead of a bare refusal. An automatic choice between the two modes
   is still open.
2. **Direction.** Explicit `pull`/`push` first; a bidirectional `session sync` only with a visible
   divergence report (now implemented for explicit pulls, fourth slice). Never guess a merge.
3. **Blobs.** The full pull refuses a session whose declared file sizes exceed the 1 GiB limit
   before copying anything; there is no partial copy and no "blob unavailable" placeholder. A
   progress display and a configurable limit remain open.
4. **Auth and paths.** The remote home, project and cwd may differ. The bundle already carries
   `cwd`; import must keep the existing allow-list check.
5. **Offline queue.** A push that fails should be retryable without duplicating a child; decide
   whether the request is idempotent by content checksum.
6. **Lease interaction.** The full pull is a read-only snapshot: it takes no lock and migrates no
   lease. The operator must leave the source lease released or quiescent while it runs; a live
   source lease is not cut off and enforcement remains open.

## First v0.3 slice (implemented)

`vowp session pull <node> <session>` calls the remote `session export --out -` over the existing
SSH argv/capture path, stages the bundle in a private temporary directory, verifies its format,
protocol version and checksum with `bundle::from_json`, and imports it as a local child through the
same path as `session import`. It prints the source and child ids (`--json`:
`{"node","source_session","child_session"}`) and removes the staged file on every path. A failed
remote run, malformed/truncated bundle or checksum mismatch imports nothing; a local node is
refused.

The tests use the existing fake-`ssh` harness and a temporary remote home:
`session_pull_imports_a_child_and_prints_both_ids`,
`session_pull_refuses_a_corrupt_bundle_before_importing` and
`session_pull_reports_a_failed_remote_export_and_imports_nothing`. The checksum test fails if
verification is removed, and the success test fails if the child's `parent_session_id` is wrong.

Still out of scope for this slice: directory copy, `session push`, bidirectional sync/merge, daemon
transport, lease interaction and offline queueing. This is independent of v0.2 and does not block
the v0.2 release.

## Second v0.3 slice (implemented)

`vowp session import <file> --same-id` and `vowp session pull <node> <session> --same-id` keep the
origin session id instead of creating a child. The mode is refused with exit 4 when the id already
exists locally as a row or as `sessions/<id>/`; the existing session is never overwritten or
merged. A same-id session has no parent. The importer writes a fresh `session.created`, copies the
whole bundle tail into the log with original event ids and timestamps (sequence numbers are
reassigned contiguously), attaches the bundle capsule, writes inline blobs and runs `reindex`.
A duplicate event id anywhere in the index is a conflict, not a warning. The default child import
and its output are unchanged; `--same-id` pull JSON is
`{"node","source_session","same_id_session"}`.

The tests are `same_id_import_keeps_the_origin_id_without_a_parent`,
`same_id_import_refuses_an_existing_session_and_changes_nothing`,
`same_id_import_rejects_a_duplicate_event_id_before_writing`,
`child_import_still_creates_a_child_with_the_parent` and the CLI tests
`session_pull_same_id_imports_the_origin_id_without_a_parent`,
`session_pull_same_id_reports_divergence_without_mutating_it`,
`session_pull_same_id_refuses_a_corrupt_bundle_before_importing` and
`session_import_same_id_keeps_the_bundle_id_and_refuses_a_second_import`.

Still out of scope for this slice: directory copy (landed in the third slice below), divergence
reporting (landed in the fourth slice below), merge, bidirectional sync, `session push`, lease
interaction and offline queueing. A same-id pull is still a one-way snapshot; the source log is
not locked. The bundle carries only the replayable tail, so the local log is a partial copy even
though it keeps the origin id.

## Third v0.3 slice (implemented)

`vowp session pull <node> <session> --full` copies the raw session directory instead of a bundle.
The local side runs the source's hidden `session dir-export` over the same SSH argv path; the first
line of its stdout is a `vowp-session-dir-v1` manifest (`origin`, and `files` with each relative
path, byte size and lowercase sha256), followed by the file bytes in manifest order. The layout is
`log.jsonl`, optional `adapters.json`, `blobs/<sha256>` and `capsules/<name>.json`; `.lock`,
`import.json` and a previous `manifest.json` are never copied.

The default cap is 1 GiB (`SESSION_DIR_MAX_BYTES`, 1 073 741 824 bytes). The source checks it from
file sizes before hashing and the local side checks the manifest total again before creating
staging, so an over-cap session fails with the limit named and copies nothing. There is no
blob-unavailable placeholder in this slice: the pull refuses rather than importing a partial
session.

A full pull always keeps the origin id and never creates a child or rewrites a bundle. The id must
be absent locally as a row and as `sessions/<id>/`; a present id is exit 4 and leaves the existing
copy untouched. The files are written into a private staging directory under `<home>/tmp`, every
digest is checked, the staged log is verified (a torn tail or corrupt line is refused), the
manifest is written into the directory, and only then is `sessions/<id>/` renamed into place. A
failed copy, digest, size or verification leaves no `sessions/<id>/` and no staging directory.
After the rename `vowp reindex` rebuilds the index; a reindex that does not index the session
removes the new directory and fails.

The pull is read-only at the source: it takes no log lock and moves no lease. The source lease must
be released or quiescent, and both nodes must run a build with the hidden export command. A
compacted source has no `log.jsonl` and is refused with a hint to use the bundle pull.

Tests: `session_pull_full_imports_the_origin_id_with_the_full_log` (full log beyond the 20-event
bundle tail, blob, capsule, `adapters.json`, manifest verification and `vowp reindex`),
`session_pull_full_reports_divergence_without_mutating_it`,
`session_pull_full_refuses_a_corrupt_snapshot_without_leaving_a_directory`,
`session_pull_full_refuses_an_over_cap_snapshot_before_copying`,
`session_pull_full_reports_a_failed_remote_export_and_imports_nothing`, and the store tests
`session_dir_manifest_verifies_and_detects_corruption`,
`session_dir_manifest_refuses_a_directory_over_the_size_cap` and
`session_dir_manifest_rejects_traversal_and_reserved_paths`. The default child pull remains
`session_pull_imports_a_child_and_prints_both_ids`.

Still out of scope: divergence reporting (landed in the fourth slice below), merge, bidirectional
sync, `session push`, live lease migration, a daemon transport, offline queueing, a progress
display, blob deduplication and a configurable size limit.

## Fourth v0.3 slice (implemented)

`vowp session pull --same-id` and `vowp session pull --full` now report divergence instead of a
bare "already exists" refusal when the origin id is present locally. The bundle and directory
paths are both checked before any local session file, row or index entry is written.

The human report names the local head (last log seq, last event type/timestamp, state and executor
where available) and the remote head (bundle tail seq/event/timestamp, or the source's read-only
`session show --json --events 1` for the full path because a directory manifest has no log
position), then prints `choose which side advances; no merge performed` and exits 4. `--json`
writes one stable document:
`{"error":{"kind":"conflict","code":"session_divergence","message","hint","node",
"source_session","local","remote","note"}}`; each head has `session`, `node`, `last_seq`,
`last_event`, `last_event_at`, `state`, `executor` and `updated_at`. The local log, manifest and
row are untouched, the full stream is aborted before staging, and no token, cwd or home path is
printed. The choice is manual: delete or export one side, then pull again. There is no
force/overwrite flag and no merge.

Tests: `session_pull_same_id_reports_divergence_without_mutating_it`,
`session_pull_full_reports_divergence_without_mutating_it` and
`session_pull_full_prefix_of_an_existing_session_keeps_the_local_conflict`, plus the unchanged
empty-local and default-child tests. Store tests cover `SessionLogReader::head`; the core test
covers the bundle head and its compacted-bundle case.

Still out of scope: automatic merge, a force/overwrite flag, bidirectional sync, `session push`,
live lease migration, a daemon transport, offline queueing, a progress display, blob deduplication
and a configurable size limit.
