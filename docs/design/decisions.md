# decisions.md — deviations and clarifications relative to `plan.md`

`plan.md` stands. This file records every place where the spec adds to, tightens or deliberately differs from it, with the reason.
Anything not listed here follows the design summary.

| # | Plan says | Spec does | Reason |
|---|---|---|---|
| D1 | §6.3 states: pending, ready, running, stalled, reviewing, fixing, merging, done, failed, replanned, parked | Adds **`gating`** (command gates running between `running` and `reviewing`) and **`cancelled`** (terminal, human cancel) | The design diagram shows gates as a transition label; as a state it makes the loop idempotent across crashes (don't re-run 30-minute gates twice) and lets the board show it. |
| D2 | §2 "one writer: the supervisor" | Loops are single-threaded writers; **short CLI mutations** (`job add`, `inbox answer`, `schedule add`, …) also write directly in one transaction under WAL with `busy_timeout=5000` | Requiring every CLI call to go through a daemon would need the v0.2 HTTP API. With a supervisor running, state-machine transitions are performed only by it (CLI `retry`/`park` enqueue a *request row*: `kv` key `req:<ulid>` handled on next tick). With no supervisor running, the CLI's state-changing commands apply the same request directly through `vowp_graph::transition` (`apply_direct`, cli.md §1). |
| D3 | §4.2 opencode "against the background service … `opencode serve` is an option" | v0.1 uses `opencode run --format json` only | One process per run; `--session` resumes. `serve` is v0.2 (ACP/HTTP). |
| D4 | §8 gate table (report, cmd, owns_respected, pushed, review, script, deploy) | Adds **`secret_scan`** gate kind | The security model requires a secret-scan before every push; making it a gate gives it evidence and a place in the merge queue. |
| D5 | §6.1 `fixes` edge "bounded by `max_attempts`" | Fix jobs run **in the original job's branch/worktree**, have no verify job of their own; the original job goes `fixing → gating → reviewing` again | Keeps one diff to verify; avoids merge conflicts between fix and original. Cheap command gates re-run before another verifier is spent (D17). |
| D6 | §7 "L2 … 2 automatic continues" | Continues counted per **attempt** (reset when a fix round starts); a hung-before-first-output run is restarted up to 3 times (`hang_restarts`), separate from continues | opencode sometimes prints nothing for several minutes after start; a separate bound restarts the process without spending a continue. |
| D7 | §7 stall "20 min with no new events and no new commits" | 20 min default (`stall_minutes`); **8 min if the run has produced no output at all** (`silent_minutes`); both configurable under `[loops.l1-supervisor]` | A run that has emitted nothing is a different failure from a slow run. |
| D8 | §12 `vowp-adapters` has `claude-code, opencode, cli` | The adapter **trait and its data types live in `vowp-protocol`** (types.md §12); `vowp-adapters` holds the implementations and shared classification helpers | Core must depend on the trait, not on a specific adapter file, so adding an adapter does not change core. |
| D9 | §12 "tokio only where streams need it" | No tokio at all in v0.1 | Every stream is a child-process pipe; threads suffice. |
| D10 | §12 stack lists `cron`, `globset`, `ulid`, `time` | Adds `serde_yaml_ng` (YAML front-matter and graph export), `regex`, `which`-free PATH lookup, `sha2`, `schemars`, `toml` | Report/brief front-matter is YAML; gates use regexes. |
| D11 | §7.1 table list | Adds two auxiliary tables: `kv` (small state, flags, request rows) and `session_natives` (index of harness-side session ids; mirrors `adapters.json`) | Needed for `paused` flag / CLI→supervisor requests, and for fast `--resume` lookups. Both rebuildable or trivial. |
| D12 | §12 CLI `vowp job add|update|ls|show|retry|park` | Adds `job cancel`, `vowp schema`, `vowp session tail`, `vowp __window` (hidden) | `session tail` is what a tmux job window runs (read-only pretty log follower); `schema` exports JSON Schema. |
| D13 | §4.1 `seq` "gaps are rejected" | Append **rejects** a gap or duplicate seq (`StoreError::SeqGap`); on opening a log with a torn (non-newline-terminated, unparsable) final line the store truncates it and records the fact | Crash safety: a crash mid-write must not poison the log. |
| D14 | §10 `on_quota = "next_then_probe_back"` only | `OnQuota` enum: `next_then_probe_back` (default), `next`, `park` | Needed so every provider's profiles can say what quota means. |
| D15 | §10 limit detection "unknown repeated errors treated as unavailable for 15 min" | Exactly: 3 consecutive runs of the same provider ending `error`/`crash` with no limit signal ⇒ `limits.kind='unavailable'`, `resets_at = now + 15 min` | Gives the vague statement a testable threshold. |
| D17 | §6.3: fix round returns the job to `reviewing` | After a fix job finishes the original goes `fixing → gating` (command gates re-run first), then `reviewing`; a finished fix of a `merge` job returns to `merging`; a failed fix job ⇒ original `failed` (loops.md §2 T22; a failed fix job is T24) | Cheap command gates run before another verifier is spent. |
| D18 | §12 CLI list | Adds keeper commands `vowp job nudge\|restart\|replan\|fail <id>` (write request rows), `vowp up --foreground\|--detach`, `vowp down --kill-windows`, `vowp inbox ask`, `vowp status --check`, files `~/.vowp/heartbeat` and `~/.vowp/board.json` | Needed for L5/L8 (loops.md §7, §10). |

## Open questions recorded for the owner (none blocking)

1. `serde_yaml_ng` vs `serde_norway` availability is checked with `cargo add`; either is acceptable (conventions.md §3).
2. `vowp.toml` timezone support: v0.1 accepts `UTC` and fixed offsets (`+02:00`) only; IANA names such as `Europe/Oslo` need a tz database
   crate (`jiff` or `chrono-tz`) — deferred to v0.2 unless the owner wants it earlier. The Claude limit text `resets 11pm (Europe/Oslo)` is parsed with a
   configurable `[node] tz_offset_minutes` fallback and the harness's own `resetsAt` when available (adapters.md §3.4).

## Errata

1. **D5 / T22 (fix rounds):** chose `fixing → gating → reviewing` (D5 now matches D17/T22) over `fixing → reviewing` because cheap command gates must re-run before another verifier is spent.
2. **`Request::SwitchSession`:** removed the reserved variant (and the `SwitchRequest` trait comment) instead of inventing an effect; v0.1 `session switch` runs inline, and `request_rows_apply_every_kind_end_to_end` now covers every remaining kind in the loops.md step-0 table.
3. **Keeper nudge text:** dropped the optional `text` from `Request::NudgeJob` and the CLI `--text` flag instead of adding a pending-nudge column; T10 is a plain continue run, matching `JobEvent::KeeperNudge` and `l2::continue_prompt(job)`.
4. **CLI flags without APIs:** added `bundle::import(..., cwd)` and `NewSession.executor` so `--cwd` and `--executor` are implementable; removed `session switch --no-seed-tail` instead of adding a second seed-setting path through `run_turn`/`TurnRequest`. The `seed_prompt(capsule, tail, tail_turns, next_input)` signature is now the one used at every call site.
5. **`vowp cost` SOURCE:** added `receipt_entries` / `price_table_entries` to `CostRow` alongside `unknown_entries`; the CLI derives `receipt`/`price-table`/`unknown`/`mixed` from those counts.
6. **Schedule fire/advance order:** chose advance-before-fire (`ScheduleEngine::tick`) and rewrote loops.md §11 to match: advance `last_run_at`/`next_run_at`, then write the ledger key, then execute the action (at-most-once). The `Loop` action pushes `Request::RunLoop{name}` (not `Wake`), and the request handler sets `loops.next_run_at=now`.
7. **Stall/fix bounds:** added `LoopBounds.silent_minutes`, `max_hang_restarts`, `planner_min_gap_minutes`, `idle_gap_minutes`, and `checkpoint_minutes`; removed `max_fix_rounds` because `JobLoop.max_attempts` is the single fix-round bound; narrowed loops.md §12's "all numbers configurable" claim to the fields that exist.
8. **Schedule JSON:** relabelled the old line as `ScheduleAction` and added full `Schedule`, `GateResult`, `Worker`, and `Switch` fixtures in types.md §15; `types_md_examples_validate` names all example fixtures and `all_schemas()` exports `Worker`, `Profile`, and `Edge`.
9. **D2 direct CLI application:** documented the no-supervisor exception in D2 (CLI `apply_direct` uses the same graph transition) instead of removing direct application.
10. **`job retry` counters:** removed the redundant `--reset-attempts` flag and `reset_attempts` field; T32's `ResetCounters` always zeroes `attempts`, `continues`, and `hang_restarts`.
11. **Claude transcript importer:** chose `vowp_adapters::claude_code::import_transcript(path, tz_offset_minutes) -> Result<Vec<HarnessEvent>, AdapterError>`; core's `ClaudeTranscriptImporter::scan_once(dir)` is the directory/offset wrapper over it.
12. **Capsule path:** fixed the schema comment to `sessions/<id>/capsules/<at_seq:010>-<ulid>.json` (10-digit zero-padded), matching types.md, architecture.md, sessions.md, and crates/vowp-store.md.
13. **Reindex acceptance:** added `reindex_restores_user_session_role` to the reindex test list so the user-session-role regression is enforceable.
14. **`tz_offset_minutes` at init:** kept the claim and made `vowp init` write `[node] tz_offset_minutes` from the machine's current UTC offset, matching adapters.md and the `[node]` config field.
