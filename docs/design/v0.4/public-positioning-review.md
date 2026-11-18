# Public positioning review — b49f90e

Status: private review note, 2026-11-13. The public commit
[`b49f90e`](https://github.com/syver777/vowp/commit/b49f90e) was read from raw GitHub; its history is
**not** merged into this repository. This note records what was ported, what was deliberately not
ported, and the public claims that still need qualification.

## Verdict

The VPS-first positioning matches the architecture: one home owns the databases, session logs,
leases, supervisor and snapshot; a control machine is a surface; an executor node can run a turn.
The new "0.4 direction" matches our design in
[`local-orchestrator.md`](local-orchestrator.md): the VPS remains the only writer, the local app is a
surface, and an outbound-only node runs offloaded jobs with per-node path mapping. No design change
is needed.

The public message has six claims that need qualification, one redaction regression and two roadmap
fixes. The proposed wording below is **not applied to the public README**; it is shown as a diff for
the owner to approve.

## What was ported

On `main`, preserving unrelated work:

- `docs/concepts/what-is-vowp.md` (new), `docs/concepts/README.md`,
  `docs/concepts/capacity.md`, `docs/concepts/home-and-surfaces.md`,
  `docs/concepts/overview.md`, `docs/concepts/sessions-and-switching.md`.
- `docs/guides/README.md`, `docs/guides/multi-node.md`, `docs/guides/sessions.md`.
- `docs/release/0.3.0.md`.
- `docs/design/README.md` (v0.3 status) and `docs/design/plan.md` (VPS-first paragraph).

On `docs/public-positioning`, because `v4-s2-remaining-redaction-gaps` currently owns these paths:

- `README.md` positioning (the private redaction paragraph is preserved).
- `docs/guides/operating.md` and `docs/guides/vps.md` v0.3/VPS-first wording (the private redaction
  paragraph is preserved).

Deliberately not ported:

- `docs/roadmap.md` and `docs/design/v0.2/README.md` / `docs/design/v0.3/README.md` are owned by the
  pending `v4-d1-roadmap-0-4` job. That job should carry the v0.4 roadmap section and the 0.3.0
  status alignment. Its current brief does not mention the public v0.4 section, so either extend
  that brief or treat [`local-orchestrator.md`](local-orchestrator.md) as the v0.4 record.
- The public `README.md` and `docs/guides/operating.md` redaction paragraphs are **older than the
  private tree** and, at `b49f90e`, older than the public code for gate evidence and ledger details.
  The private supervisor-log, gate-evidence and ledger redaction is already in place, and `v4-s2` is
  closing the switch/watch gaps. Porting the public paragraph would regress the private docs.

## Claims to qualify

### Redaction regression (read first)

The commit changes the `README.md` and `docs/guides/operating.md` redaction text in the **wrong
direction**. The public base before `b49f90e` already said exact-value redaction covered the
supervisor log, command-gate evidence and ledger rows; the commit removes that coverage and lists
those paths as gaps again:

```diff
-- Exact-value redaction covers session-service and remote-ingest events, the snapshot, capsules, the
--  supervisor log, command-gate evidence, ledger rows and the finished report and run mirror. It does
--  **not** cover the switch turn path or the `session watch` importer in this build ...
+- Exact-value redaction covers session-service and remote-ingest events, the snapshot, capsules and
+-  the finished report and run mirror. It does **not** cover the supervisor log, gate command evidence,
+-  ledger rows, the switch turn path or the `session watch` importer in this build ...
```

The public code at `b49f90e` **does** redact gate evidence and ledger details:
`crates/vowp-core/src/gates/cmd.rs` calls `vault::redact_text`/`redact_bytes`, and
`crates/vowp-core/src/ledger/record.rs` redacts the summary and string details before insert. The
public `docs/security.md` still lists gate evidence and ledger rows as unredacted, so the commit is
following a stale security page instead of the code. The public base README was closer to the code
for gate/ledger. The supervisor-log claim is a separate question at the public tag; the private
`v4-s1` job already covers it.

Do not publish the README/operating hunk as written. Keep the gate and ledger coverage in the public
README, and update `docs/security.md` to match the code. The private tree has the gate, ledger and
supervisor-log redaction and is closing switch/watch in `v4-s2`; I did not port the public hunk into
the private README or operating guide.

### 1. "Fleet and worker tracking ... see the whole fleet in one snapshot"

Public README, "What you get":

> Register executor nodes, track workers and see the whole fleet in one snapshot.

The snapshot has projects, sessions, capacity, provider limits, inbox and orchestrators. It has no
node registry; worker slots are in the project board (`Board.slots_busy`/`slots_total`) and the
project DB, not `snapshot.json`. `vowp node ls` and `/api/v1/nodes` are the node read models.

Proposed public replacement:

```diff
-| **Fleet and worker tracking** | Register executor nodes, track workers and see the whole fleet in one snapshot. |
+| **Fleet and worker tracking** | Register executor nodes, see the node registry and the project board's worker slots, and read the fleet snapshot for projects, runs, capacity, limits and inbox. |
```

### 2. "Interchangeable sessions ... switch node ... with a capsule"

Public README, "What you get":

> A session is a harness-neutral append-only log. Switch model, harness, node or surface with a
> capsule; the history stays.

A model, harness or surface switch is a capsule switch. A node move is an explicit export/import or
`session pull` (bundle or full-directory copy); it is not a capsule-only live switch. v0.4's local
node also keeps the home as the only writer; it does not add live node migration.

Proposed public replacement:

```diff
-| **Interchangeable sessions** | A session is a harness-neutral append-only log. Switch model, harness, node or surface with a capsule; the history stays. |
+| **Interchangeable sessions** | A session is a harness-neutral append-only log. Switch model, harness or surface with a capsule; move a session to another node with an explicit export/import or `session pull`; the history stays. |
```

The same caveat applies to the new page's "A session can move between models, harnesses, nodes and
surfaces" and to `docs/design/plan.md`'s "move between harnesses, models and nodes".

### 3. "Codex ... through their documented headless interfaces"

Public README introduction:

> It drives the harnesses you already run — Claude Code, opencode, Codex and other CLI agents —
> through their documented headless interfaces.

Claude Code, opencode and ACP are implemented and tested. Codex is an **illustrative generic CLI
mapping**; `docs/design/adapters.md` §4.4 says the mapping is "Illustrative, untested against the
real tools" with hand-built fixtures. The generic adapter is real; the Codex-specific claim is not
verified.

Proposed public replacement:

```diff
-It drives the harnesses you already run — Claude Code, opencode, Codex and other CLI agents —
-through their documented headless interfaces, keeps their logins out of the loop, and gives them a
-shared home, history and delivery process.
+It drives the harnesses you already run — Claude Code and opencode through their documented
+headless interfaces, ACP servers through the protocol, and Codex or another CLI agent through the
+generic mapping adapter. The mapping is configuration you pin and test. VOWP keeps harness logins
+out of the loop and gives the agents a shared home, history and delivery process.
```

The new page's §5 repeats the same overclaim and needs the same qualification.

### 4. "No cost dashboard"

Public roadmap, "What is deliberately not in v0.3": "No cost dashboard. The ACP adapter ships in
v0.3; the cost dashboard remains deferred." `docs/concepts/overview.md` also lists "The browser cost
dashboard" as deferred.

The tree ships `crates/vowp-daemon/web/cost.html`, `/api/v1/cost` and the cost-page tests. A basic
cost page and report are implemented; if the intended claim is the broader v0.3 design note
(charts, receipts, all groupings), say "full cost dashboard", not "no cost dashboard".

Proposed public replacement:

```diff
-- **No cost dashboard.** The ACP adapter ships in v0.3; the cost dashboard remains deferred.
+- **No full cost dashboard.** The ACP adapter ships in v0.3; a basic loopback cost page and
+  `/api/v1/cost` ship, while the broader cost-dashboard design remains deferred.
```

### 5. "The API ... ships in 0.3.0" needs its scope

Public roadmap:

> The API, window, phone board, ACP adapter and explicit session pull ship in 0.3.0; the remaining
> rows are deferred.

What ships is the read API plus the two token-gated writes (inbox answer and local session switch).
The jobs page and job retry/park/cancel routes are still in progress (`v4-w1-jobs-page` and
`v4-a1-job-actions-api`). The public README's "loopback window" is fine; the roadmap's "the API"
reads as the whole API.

Proposed public replacement:

```diff
-These items were the v0.3 plan. The API, window, phone board, ACP adapter and explicit session pull
-ship in 0.3.0; the remaining rows are deferred.
+These items were the v0.3 plan. The read API and two token-gated writes, the window, the phone
+board, the ACP adapter and explicit session pull ship in 0.3.0; job actions, the jobs page and the
+rest of the API remain follow-ups.
```

### 6. "The home opens no public port"

Public README and the new page say the home opens no public port; the next sentence says access is
SSH. The VOWP processes open no public port, but the home does run `sshd` on 22; the literal
sentence is false.

Proposed public replacement:

```diff
-The home opens no public port. The opencode app server and the one-time secret page listen on
-loopback and are reached through an SSH tunnel.
+VOWP opens no public port. The home is reached over SSH; the opencode app server, the daemon and
+the one-time secret page listen on loopback and are reached through an SSH tunnel.
```

The new page's "It opens no public port" and "the home opens no public port" need the same wording.

### 7. Small roadmap fixes for `v4-d1`

`docs/roadmap.md` is owned by the pending `v4-d1-roadmap-0-4` job, so it was not ported. Two public
lines there are stale even after the v0.3 heading sweep:

- The "No VPS metrics history..." bullet still says "infrastructure that v0.2 does not provide"; it
  should say v0.3.
- "The only listeners are loopback-only: the opencode app server joined through an SSH tunnel and
  the one-time secret paste page" predates the v0.3 `vowp-daemon`/window; add the loopback daemon
  and window to the list.

## What is not a problem

- The public 0.3 status alignment (v0.2 → v0.3) matches the private version `0.3.0` and the merged
  daemon, window, phone board, ACP adapter and session pull.
- The public `v0.3.0` setup URL resolves (`/docs/setup/agent-setup.md` returns 200).
- The public 0.4 paragraph matches our accepted design: SSH-first, VPS-only writer, local surface,
  outbound-only node and per-node path mapping. It does not promise code.
- The Linux-VPS home wording matches the private README and install guide; macOS is the control
  machine platform. This is a support-policy statement, not a new implementation claim.

## Next public update

Nothing is pushed. The complete proposed patch is
[`public-positioning-b49f90e.patch`](public-positioning-b49f90e.patch). Apply it on the Mac from the
public repository root:

```sh
# copy this patch to the Mac first; run from the public repository root
git apply -p1 /path/to/public-positioning-b49f90e.patch
```

It touches `README.md`, `docs/concepts/what-is-vowp.md`, `docs/concepts/overview.md`,
`docs/design/plan.md`, `docs/guides/operating.md`, `docs/roadmap.md` and `docs/security.md`. It was
dry-run checked against the raw `b49f90e` files. Do not regenerate private history from the public
repo. The private README and the two running-job guides can then be updated from the corrected
public text after `v4-s2` merges.
