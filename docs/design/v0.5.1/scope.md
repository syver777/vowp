# v0.5.1 — scope

Status: **accepted plan (2026-11-19); private repo only.** The public `syver777/vowp` repository stays
at its current state (`0.4.0` plus documentation media) and is **out of scope until the owner says
otherwise**. All work here targets `vowp-private` and the installed binaries on the owner's Mac and VPS.

## Baseline

`snapshot/0.5.0-private` marks the v0.5 code-complete tree (`f19f416`). Version strings were not
bumped at that point, so binaries built from it report `0.4.0` although they contain v0.5 code. That
made it impossible to tell which engine was installed, so the first v0.5.1 job makes the version honest.

## Why v0.5.1 exists

Running StackSpirit against a real orchestrator on 2026-11-19 exposed engine-side gaps that unit tests
with fakes did not:

1. **Version honesty.** Binaries must say what they contain.
2. **Native session id.** `vowp agent orchestrator endpoint` reports `native_session_id: null` for an
   OpenCode-hosted orchestrator, because OpenCode's `service.json` carries no session id. A UI client
   then has to guess ("the newest session"), which is wrong as soon as a host has two sessions.
3. **Surface ownership.** A session has a lease with one writer, but a UI that attaches directly to the
   orchestrator's loopback engine bypasses it, so two surfaces (the OpenCode window and a desktop UI)
   can write to one session. Operators also cannot see which surface currently owns a session.
4. **Retention.** Closed-session and run-output retention are hard-coded (30 and 14 days) with no way
   to delete older material on demand.

## Slice status

Status of each slice in the tree. The proxy is **not** done.

| Slice | Status |
|---|---|
| 1. Honest versioning | done (`0.5.1-dev`) |
| 2. Native session id | done |
| 3. Session ownership, Slice 1 (foundation) | done |
| 3. Session ownership, Slice 2a (endpoint `owner`/`access` and `orchestrator attach`) | done |
| 3. Session ownership, Slice 2 loopback engine proxy | **deferred** (owner decision, 2026-11-20); ownership is recorded and cooperative |
| 3. Session ownership, Slice 3 (job ownership lifetime and takeover) | done |
| 3. Session ownership, Slice 4 (operator and design documentation) | this documentation job |
| 4. On-demand retention (`gc --older-than`) | done |

## Slices

### 1. Honest versioning
Bump the workspace, crates, packaging and version tests to `0.5.1-dev`. `vowp --version` then
distinguishes a v0.5.1 development build from the `0.5.0` snapshot and from the installed `0.4.0`.

### 2. Record the host's native session id
When an OpenCode-hosted orchestrator has a known app-side session, `orchestrator endpoint` reports it
(`native_session_id`), so clients never guess. When it is genuinely unknown the field stays `null`.

### 3. Session ownership (design first)
Design, then implement in separate jobs: a surface kind for a desktop UI, the active surface in the
endpoint and fleet read models, an explicit takeover that makes the previous writer read-only, and the
same rule for worker nodes. The first job is a design document only; implementation slices are queued
from it.

### 4. On-demand retention
Add an explicit way to collect closed sessions and run output older than a chosen age, through the
existing safe compaction path (never by deleting log directories behind the database's back), with a
refusal for any session that has a live lease or a running job.

## Out of scope

The public repository, publishing to crates.io or npm, `0.5.1` release notes, v0.6 (ACP chunks and
permissions, drafted in `docs/design/v0.6/scope.md` and **deferred**).
