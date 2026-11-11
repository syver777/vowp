# Data lifecycle

VOWP creates databases, session logs, blobs, run mirrors, worktrees, backups and samples. The lifecycle
model answers one question for every file: **what is it, and may it ever be deleted?** The answer is a
class. Four classes are permanent by construction; five are disposable and collected by vowp `gc --apply`.

> **Status: current.** Classes, vowp `gc`, closed-session compaction, backup/restore and the nightly
> GC schedule are implemented on `main`. The binding specification is
> [`docs/design/v0.2/home-and-surfaces.md`](../design/v0.2/home-and-surfaces.md) §10.

## Classes

| Class | Examples | Default | Derived from |
|---|---|---|---|
| **production** | releases, rollback snapshots, serving banks, prod pointers | **Never auto-deleted** | `releases` rows, `[env.*]` paths, catalog |
| **truth** | global and project databases, capsules, passing gate evidence, decisions and ledger, backups, archived project data | **Never auto-deleted**; backed up | database files, `capsules/`, `gate_results`, `catalog` |
| **open work** | worktrees of unmerged or unpushed jobs, active or open sessions, orchestrator sessions, the live lock and heartbeat | **Never auto-deleted** | `jobs` state and worktree, `sessions` state, `orchestrators/` |
| **secrets** | `~/.vowp/secrets/`, vault files | **Never auto-deleted**; **never backed up in plain text** | secrets directory, mode file |
| **closed session** | `log.jsonl` and blobs of an idle session whose job is done, or an explicitly closed session | Kept 30 days after the last update, then compacted to a capsule and summary | `sessions.state='closed'` or a done job |
| **run output** | `runs/*.jsonl`, `*.stderr` | 14 days; gzip after 1 day | `runs/` and the run rows |
| **worktree** | a job worktree | Removed after merge; a failed, cancelled or replanned job's worktree is kept 7 days | job state and worktree path |
| **sample** | `host_samples` | One row per supervisor tick, kept for 24 hours; no automatic one-minute rollup | the table |
| **cache** | disposable `tmp/` files | 24 hours | `tmp/` |

The release log (`releases.log`) is not currently classified; GC leaves it untouched because it is
outside the classified set. Adding it to the catalog is a follow-up.

The classification is **derived from database state wherever possible**, so no registration hook is
needed when a run, session, worktree or sample is created. Known paths are scanned directly too:
`backups/`, `gates/` and `archive/` under the home are truth, release snapshots and `[env.*]` checkouts
are production, and the sessions and worktree rows supply the rest. The `catalog` table is a durable
side table merged in by path; a catalog row can only strengthen protection, never downgrade a derived
class. In this build the only production writer of new catalog rows is the run-output gzip step, which
records the `.gz` file and marks any existing catalog row for the old path deleted. Adding catalog rows
for the other classes is a follow-up.

An orchestrator session is always **open work**, even if its bound project has finished a job. A stale
`done` job can never make a live session compactable; only an idle or explicitly closed session moves
to the closed-session class.

## `gc`

vowp `gc` is a dry run by default. It lists files per class, their bytes and a five-path sample, and
changes nothing. vowp `gc --apply` deletes only the five disposable classes once their expiry has
passed:

- **closed sessions:** compact first, then remove the full log and non-evidence blobs;
- **run output:** gzip after one day and delete after fourteen;
- **worktrees:** remove per the worktree rule, never the main checkout and never an open job;
- **samples:** prune expired host-sample rows;
- **cache:** remove expired temporary files.

It never deletes production, truth, open work or secrets, even if a catalog row carries an expiry. It
refuses to delete a classified catalog or run-output path outside the home; worktree removal is allowed
outside the home and is checked against the main checkout instead. The `--class` filter narrows a pass
to one class, and `--json` prints the report. `--now` exists for tests and takes an RFC 3339 instant.

A nightly L9 schedule, `gc-nightly`, runs vowp `gc --apply` every 24 hours for each project. The supervisor
creates the schedule once per project if it is missing. The same tick enforces the 50 MB raw run-output
cap; vowp `gc --apply` forces a cap scan as well.

The report has one row per class and the totals:

```json
{
  "generated_at": "2026-11-10T14:02:11.000Z",
  "dry_run": true,
  "classes": [
    {"class": "closed_session", "files": 2, "bytes": 18342, "sample_paths": ["…/log.jsonl"]}
  ],
  "deleted_files": 0,
  "freed_bytes": 0,
  "compacted_sessions": 0,
  "errors": []
}
```

### Closed-session compaction

When a session is closed and 30 days have passed since its last update, compaction:

1. writes one final capsule (`trigger='job_end'` when none exists);
2. writes `sessions/<id>/summary.md` with the capsule markdown, the first and last turn times and the
   event count;
3. sets `sessions.compacted_at` and keeps the `sessions`, `session_events` and `capsules` rows — the
   index is the history;
4. deletes `log.jsonl`, `.lock` and the session's blobs, except blobs referenced by passing gate
   evidence (truth).

After compaction, vowp `session tail` and vowp `session show --events` exit 3 with a message pointing
at vowp `session show --capsule`. vowp `session show --capsule` and vowp `session export` keep working;
an export from a compacted session carries its latest capsule, an empty event tail and `compacted_at`
set, while the command's human output is the output path (or the bundle itself with `--out -`). There is
no full-log replay after compaction, which is the point of the class. vowp `reindex` skips a session
with no log and reports it; it never deletes the row.

### Run output

Raw harness output is useful for debugging and is not conversation content. VOWP keeps it bounded:

- a completed file is capped at 50 MB and keeps its last 50 MB, starting at a line boundary;
- after one day, vowp `gc --apply` gzips it in place and keeps the `.gz` name plus a catalog row;
- after fourteen days it is deleted;
- an oversized in-flight file is truncated to zero in place, so the writer keeps the same inode and
  the canonical session log is unaffected.

The gzip step uses the external `gzip` binary; a failure is reported in the GC report rather than
silently leaving the file uncompressed.

### Worktrees

The merge queue removes a merged job's worktree as part of the merge, as in v0.1. A failed, cancelled
or replanned job keeps its worktree for seven days so a human can inspect it; the branch is kept even
after the worktree is removed. A worktree for a job that is still pending, ready, running, stalled,
gating, reviewing, fixing, merging or parked is **open work** and is never removed by GC. A path that is
the main checkout is refused.

### Samples and cache

Host samples are written once per supervisor tick (60 seconds by default) and kept for 24 hours. The
pruner also knows how to retain one-minute rows for 30 days if such rows exist, but the sampler does
not create them in this build; a minute rollup is still a follow-up. Disposable files under `tmp/`
expire after 24 hours; the snapshot writer uses that directory for its atomic rename, so a live file is
not a cache candidate until it is old.

## Backup

One backup is one directory (or, for the legacy `--to <file>.db` form, one global-database file). A
directory backup contains:

- the global database;
- every project database;
- the authoritative session content: `log.jsonl`, blobs, capsules, `summary.md` and `adapters.json`;
- an informational catalog file;
- `manifest.json` with a format tag, version, schema versions, a SHA-256 for every content file and row
  counts for the listed tables. The global count covers `nodes`, `projects`, `sessions`,
  `session_events` and `capsules`; each project count covers `jobs`, `runs`, `edges`, `loops`,
  `schedules`, `inbox`, `ledger` and `releases`. Restore checks `integrity_check` and exactly those
  counts, not every table in the schema.

Without the session content a restore would keep the index rows but lose every conversation, so it is
never optional. Secrets are never included; the manifest records that they were skipped and restore
tells the owner to set them again with vowp `secret set`.

- vowp `backup [--to <path|rsync-target|git-remote>] [--offsite <target>] [--encrypt <age-recipient>] [--json]`
- vowp `backup --restore <dir> --to-home <fresh-home> [--force]`

- With no destination, the backup goes to `~/.vowp/backups/<UTC timestamp>/`. Backups in that default
  directory are rotated: the newest backup per day is a daily candidate, the newest daily candidate per
  ISO week is a weekly candidate, and the union of the newest 7 daily and 4 weekly backups is kept.
- `--to` may be a directory, a `.db` file (legacy single global-DB copy), a `user@host:path`, an
  `rsync://` target or a `git:<remote>` target. Only the `git:` prefix selects git; other remote-looking
  targets go to rsync. `--offsite` is an explicit alias for an offsite target.
- `--encrypt` runs the external `age` binary. A missing `age`, `rsync` or `git` is a clear exit 5 naming
  the binary it needs; VOWP never silently writes an unencrypted copy.
- `--restore` stages into a sibling directory of `--to-home`, verifies every hash and row count, runs
  `integrity_check`, and only then swaps the verified home into place. It refuses to overwrite an
  existing home without `--force` and refuses to restore over the home that is serving the command.
  A corrupt backup leaves the target untouched.

The legacy `--to <file>.db` form copies only the global database. It stays compatible with the v0.1
backup test and still opens as an unbound global handle; it is not a full home backup.

## Migration and the archive

vowp `migrate` (also run automatically by `vowp up` unless `--no-migrate`) splits a v0.1 home into the global
database and one database per project. It writes a pre-migration backup first, refuses to run while a
supervisor is up, verifies row counts per table and records the result in `migration.json` while the
operation is in flight. vowp `migrate --dry-run` reports what would change; vowp `migrate --rollback <backup>`
restores the pre-migration backup instead of migrating. A failed or interrupted migration is
reconciled on the next startup; it does not leave two half-live databases.

vowp `project rm <name> --yes` archives the project directory under
`~/.vowp/archive/projects/<name>-<timestamp>/` and removes the registry row. The archive is truth and
is never deleted by GC. Production catalog rows that mention the project are kept.

## Retention at a glance

| Data | Keep | Then |
|---|---|---|
| Global DB, project DBs, capsules, passing gate evidence, ledger, decisions | forever | backed up |
| Production releases and serving banks | forever | never deleted |
| Open worktrees, active sessions, orchestrator sessions | while open | never deleted by GC |
| Secrets | while configured | never deleted by GC, never backed up |
| Closed session log and blobs | 30 days after close | compact to capsule + summary |
| Raw run output | 14 days | gzip after 1 day, delete after 14 |
| Failed/cancelled/replanned worktree | 7 days after the terminal state | remove worktree, keep branch |
| Host samples | 24 hours (one row per tick) | prune; no minute rollup yet |
| Disposable temp/cache | 24 hours | delete |
| Backups in the default directory | 7 daily + 4 weekly | rotate |

## What the lifecycle never does

- It never deletes a production, truth, open-work or secret path. A property test enumerates protected
  paths before and after an apply pass and asserts they still exist.
- It never removes the main checkout or an open job's worktree.
- It never compacts a live, blocked, switching or orchestrator session.
- It never deletes a session row; compaction removes the full log and keeps the index and capsules.
- It never includes a secret in a backup, even with `--encrypt`.
- It never silently falls back to an unencrypted offsite copy when a tool is missing.
- It never kills a running job to reclaim space. The disk brake stops new admissions; GC removes only
  expired disposable files.

Next: [capacity](capacity.md), or [home and surfaces](home-and-surfaces.md).
