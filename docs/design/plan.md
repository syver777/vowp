# VOWP — Virtual Open Work-to-Production

VOWP is an open-source framework for running AI coding agents on your own machines — a laptop, a server, or both — as a team. Each agent can be Claude Code, opencode or another harness. Work is planned as a **graph** of jobs. Deterministic **loops** keep it moving. Each job runs in a **tmux window** you can watch from any terminal. Every conversation is a **universal session** that can move between harnesses, models and machines without losing its context. Finished work moves from **Work to Production** only through guarded, reversible promotions.

This document describes how VOWP works. The imperative spec for each crate is in the neighbouring files; `decisions.md` records the places where that spec tightens or clarifies this design.

---

## 1. Principles

1. **Stock harnesses, no forks.** VOWP drives Claude Code, opencode and others through their official headless/JSON interfaces. It never scrapes a TUI, never sends keystrokes to answer dialogs, and never reads, stores or forwards anyone's login credentials.
2. **The database is the truth for decisions; files are the truth for content.** Each node has one SQLite database (`~/.vowp/vowp.db`, WAL mode, one writer: the supervisor) holding nodes, projects, jobs, edges, gates and evidence, runs, workers, sessions (index), loops, schedules, ledger and inbox. Content stays in plain files: append-only session logs (JSONL) and blobs, briefs and reports (Markdown, because agents read and write files naturally), and `vowp.toml`. Kill everything, restart, and it continues where it was.
3. **Deterministic loops, LLM judgment.** A plain program ticks every 60 seconds and never "decides to stop". Language models are called only for judgment: planning, diagnosing a stall, reviewing a report, writing a handoff. The program cannot idle the way an agent can.
4. **Any model, any machine.** A model is a setting in a profile ladder, not an assumption. The same job can run on one harness on a laptop or another on a server.
5. **Evidence over claims.** A job is done when its gates pass with recorded evidence (commands run, exit codes, diffs), not when the agent says so.
6. **Work → Production is guarded and reversible.** Promotions snapshot what is live, ship only the intended change, record the rollback and require a human yes by default.
7. **Cost is reported, not capped.** Every call's real cost comes from the harness's own receipt. Totals are reported at checkpoints. There are no invented budgets, but there are hard bounds on retries and no-progress loops.
8. **Humans are never a bottleneck for unrelated work.** A question for the owner parks one job and lands in an inbox; everything else keeps running.

---

## 2. Concepts

| Term | Meaning |
|---|---|
| **Node** | A machine running VOWP: your laptop (`local`), a server (`server`), any host. |
| **Harness** | An agent program: Claude Code, opencode, Codex, Gemini CLI… |
| **Adapter** | VOWP code that drives one harness headlessly and turns its output into canonical events. |
| **Session** | A universal, harness-neutral conversation with a stable id and an append-only event log. It can have several native sessions underneath (one per harness run). |
| **Capsule** | A structured handoff (intent, decisions, state, files, gotchas, next steps) written when a session switches harness/model/node. |
| **Switch** | Moving a session to another executor (harness + model + node), recorded as lineage. |
| **Window** | Any view onto live work: a tmux window running a job, the board, a chat window into a session. Switching the harness behind a session does not close its window. |
| **Job** | A node in the graph: one unit of work with a brief, owned files, gates and a report. |
| **Graph** | Jobs plus edges (needs, verifies, fixes, escalates, blocks). It is a DAG except for bounded loop edges. |
| **Gate** | A machine-checkable condition with recorded evidence: report fields present, command exits 0, branch pushed, reviewer passed… |
| **Loop** | A repeating process with a tick, an input, actions and bounds (section 7). |
| **Supervisor** | The deterministic program that runs the loops. |
| **Roles** | **Planner** (owns the graph, writes briefs, reviews), **Keeper** (judgment when things stall or fail; takes over planning when the planner is unavailable), **Worker** (does a job), **Verifier** (checks a job independently). |
| **Profile** | A role's model ladder: an ordered list of executors, best first, with a cheaper fallback behind it. |
| **Slot** | A concurrent worker place (also a tmux window). |
| **Inbox** | Questions and approvals waiting for a human. |
| **Ledger** | Append-only record of dispatches, gate results, merges, costs and switches. |
| **Environment** | Where code runs: `work` (staging) and `prod`, each with its own checkout, deploy commands and guards. |
| **Promotion** | Moving a verified change from Work to Production through guards, with a serving snapshot and rollback. |

---

## 3. Architecture

```
            You:  terminal · browser · phone
                         │  (ssh / tunnel)
          ┌──────────────┴───────────────┐   ┌────────────┴──────────────┐
          │ NODE: local                  │   │ NODE: server               │
          │  vowp CLI ─────────────────────► │  vowp CLI (same binary)    │
          │  sessions store (~/.vowp)    │   │  sessions store (~/.vowp)  │
          │  adapters: claude-code, …    │   │  supervisor (systemd user) │
          └──────────────────────────────┘   │   ├ L1 tick 60s            │
                                             │   ├ graph + scheduler      │
                                             │   ├ gates + merge queue    │
                                             │   ├ failover/switcher      │
                                             │   ├ ledger, inbox, reports │
                                             │   └ tmux window manager    │
                                             │  tmux session vowp-<proj>  │
                                             │   board│planner│keeper│job…│
                                             │  adapters (headless JSON): │
                                             │   claude -p stream-json    │
                                             │   opencode run --format json│
                                             │  git worktrees per job     │
                                             └────────────────────────────┘
```

Rules:

- The supervisor and the CLI talk to harnesses only through adapters. Adapters only use documented headless modes.
- A project's graph and supervisor live on one node (its "home"). Other nodes see it over SSH (`vowp --node server …`). There is no cross-node sync in v0.1: a session moves between nodes as an exported bundle (capsule plus recent tail).
- The tmux windows show live runs. They are views, never control channels: VOWP never sends keystrokes to a harness TUI.
- An HTTP+SSE daemon API and a web/PWA window are v0.2. Their schemas are stable in v0.1, but the transport and UI are not part of v0.1.

---

## 4. Universal sessions

v0.1 is simplified to what the supported harnesses officially offer today.

### 4.1 Event log

`~/.vowp/sessions/<sessionId>/log.jsonl` is append-only. Every line is an envelope:

```ts
interface EventEnvelope<T = unknown> {
  id: string;            // ULID
  sessionId: string;     // canonical ULID
  seq: number;           // 1,2,3… gaps are rejected
  ts: string;            // ISO-8601 ms
  type: EventType;       // see below
  actor: "user" | "assistant" | "system" | "tool";
  source: { harness: "claude-code" | "opencode" | "cli" | "vowp"; model?: string; node?: string; nativeSessionId?: string };
  payload: T;
  raw?: unknown;         // original harness event (display only, never replayed)
  replayable?: boolean;
}
```

Event types: `session.created`, `session.executor` (attach/detach), `turn.started`/`turn.ended`, `user.message`, `assistant.message`, `assistant.reasoning` (display only), `tool.call`, `tool.result` (large output spilled to `blobs/<sha256>`), `file.change`, `plan`, `limit` (`quota|credits|rate|context|auth`, `resetsAt?`), `cost` (`usd`, tokens, `source: receipt|price-table|unknown`), `switch`, `capsule`, `error`, `note`.

The JSONL is authoritative for session content and fsynced at turn ends. The session rows and event index in `vowp.db` are rebuildable from it (`vowp reindex`), and a session directory copied to another node opens there.

### 4.2 Adapters in v0.1

| Adapter | How it drives the harness | Resume | Limits detected |
|---|---|---|---|
| **claude-code** | `claude -p --output-format stream-json --verbose [--resume <id>] --model … --effort … --permission-mode …` (official headless mode; optionally `--input-format stream-json` for multi-turn) | `--resume <native id>` | usage/session/weekly limit text and the reset time, 429/overloaded, auth errors; unknown repeated errors are treated as "unavailable for 15 min" |
| **opencode** | `opencode run --format json --session <id> --model provider/model#variant --auto` against the background service (sessions also show in the opencode app); attaching to `opencode serve` through its HTTP API is an option | `--session <id>` | provider 402/insufficient credit, 429, auth, context overflow |
| **cli** (generic) | Any command with JSON or plain output (e.g. `codex exec --json`, `gemini -p --output-format json`) plus a mapping file | per tool | exit codes and patterns from the mapping file |
| **acp** (v0.2) | Agent Client Protocol (`claude-code-acp` and others) for richer events and permission flow | yes | yes |

The adapter contract is `probe()`, `capabilities()`, `start(input)` → `send/events/interrupt/stop`, so adding a harness is one file plus one capability entry.

### 4.3 Capsule and switch

The capsule contains intent, constraints, decisions, state (done/in-progress/blocked/next), files, gotchas, open questions and telemetry. It is written by the cheapest capable model on the node, validated against the schema, and repaired once. The previous capsule is kept on failure.

The switch protocol is: freeze → drain ≤ 5 s → capture capsule → stop → start the target seeded with the capsule plus the last two turns as plain text → record `switch` → unfreeze. Switches are triggered by the user, by a `limit` event per the profile policy, by an adapter crash (if configured), or by context pressure.

Cross-node in v0.1: `vowp session export <id>` writes a bundle (capsule + tail + lineage), and `vowp session import` on the other node creates a child session.

### 4.4 A typical handoff

- You work in Claude Code as usual. `vowp session watch` records that session into the canonical store through the claude-code adapter's transcript importer.
- When the harness reports a limit, `vowp session switch <id> --to <harness:model> --node server` hands the session to another executor on another node with a capsule. You keep talking through `vowp chat <id>` in any terminal.
- When the original provider resets, switch back the same way. The lineage shows both halves as one conversation.

---

## 5. Windows

### 5.1 tmux window manager (v0.1)

- **One dedicated tmux session per project:** `vowp-<project>`. VOWP never touches other tmux sessions.
- **Layout:** window 0 `board` (live overview), 1 `supervisor` (loop log), 2 `planner`, 3 `keeper`, then one window per running job (`job-<id>`), and `inbox`.
- **Each job window shows a pretty-printed view of the job's event log.** The harness runs headless and its JSON goes to the log, so the window is readable and the log is complete.
- **Safety rules:**
  - Never `respawn-pane` without an explicit command. Recycling a window means kill it and create a new one.
  - Tag every VOWP window (`@vowp=1`), and only ever kill tagged windows.
  - `capture-pane` is for display only. Logs drive decisions, never screen text.
  - No keystrokes are ever sent to a harness to answer prompts. Permissions are set by harness mode and policy.
- **Access:** `ssh -t server tmux attach -t vowp-<project>`, or `vowp attach --node server`. Job sessions also appear in the opencode app.

### 5.2 Terminal windows built into the CLI (v0.1)

- `vowp board` is a refreshing dashboard: jobs by state, slots, current planner/keeper executor, limits and reset times, costs, inbox count, last checkpoint.
- `vowp chat <session>` is a window into any session on any harness. Type to send, stream replies, and use `/switch <harness:model>`, `/capsule` and `/export`.
- `vowp graph` prints the graph as ASCII or Mermaid.

### 5.3 Web window (v0.2)

The PWA (nodes, sessions, session view with an executor switcher, capsule view, board) runs on top of the daemon API. Event and job schemas are frozen in v0.1 so v0.2 only adds transport and UI.

---

## 6. The graph

### 6.1 Job node

Stored in the node database (`jobs` + `edges` tables) and edited only through `vowp job add|update` (validated), never by hand. `vowp graph export` writes it as YAML so a project can keep its graph in git.

```yaml
id: graph-engine
title: "Graph engine: DAG, loop edges, ownership locks, scheduler"
kind: build            # plan | spec | build | test | verify | fix | merge | docs | research | promote | release | decision
role: worker           # planner | keeper | worker | verifier
profile: worker        # model ladder (section 10)
needs: [protocol]      # must be done first
owns: ["crates/vowp-graph/**"]            # the only paths this job may change (lock)
reads: ["crates/vowp-protocol/**", "docs/design/**"]
brief: .vowp/jobs/graph-engine/brief.md
report: .vowp/jobs/graph-engine/report.md
gates:
  - report: { fields: [status, branch, commits, tests] }
  - cmd: { run: "cargo test -p vowp-graph", expect_exit: 0 }
  - cmd: { run: "cargo clippy --workspace --all-targets -- -D warnings", expect_exit: 0 }
  - owns_respected: true            # diff touches only `owns`
  - review: { by: verifier }        # spawns a verify job
loop: { until: gates, max_attempts: 3, on_exhausted: escalate }
priority: 50
state: pending
```

### 6.2 Edges

| Edge | Meaning |
|---|---|
| `needs` | A depends on B being done. |
| `verifies` | An automatic verify job for a build job. |
| `fixes` | A fix job created by a failed verify. This is a **loop edge**, bounded by `max_attempts`. |
| `escalates` | Exhausted attempts or a human question → a `decision` job in the inbox. |
| `blocks` | A decision job blocks the jobs waiting on it. |

### 6.3 States

```
pending ──needs done──► ready ──dispatched──► running ──report+cmd gates──► reviewing ──verify pass──► merging ──► done
                                             │   ▲                            │
                                    no progress  │ continue/nudge             └─verify fail──► fixing (loop edge, ≤3) ─► reviewing
                                             ▼   │
                                          stalled ┘──attempts exhausted──► failed ──keeper──► replanned | parked
any state ──needs a human / provider blocked──► parked ──resolved──► ready
```

### 6.4 Scheduler rules

1. Only `ready` jobs are dispatched, by priority, then age.
2. **Ownership locks:** no two running jobs may have overlapping `owns` globs. Conflicts wait. Two jobs never need the same file at once, and the planner splits work so that holds.
3. **Slots and provider concurrency:** max workers per node, and max concurrent sessions per provider (lowered automatically after 429s).
4. **One git worktree per job** on branch `job/<id>`; the worker's working directory is that worktree.
5. **Single writer to `main`:** only the supervisor's merge queue writes `main`. It rebases the job branch, runs the repo-wide test command, fast-forwards and pushes. A conflict becomes a `merge` job for the same owner.

---

## 7. The loops

| Loop | Tick | What it does | Bounds |
|---|---|---|---|
| **L1 Supervisor** | 60 s, deterministic | Reconciles every job: process alive? new events? report present? limits? Runs gates, moves states, feeds the merge queue, dispatches ready jobs, triggers L4/L5, writes the board. | Never stops. The watchdog restarts it if it dies. |
| **L2 Worker** | per job | Starts the job's session from its brief. When the run ends without a report, sends "you stopped before writing your report — continue" into the same session. Then gates. | 2 automatic continues, then the keeper diagnoses. |
| **L3 Verify** | after a report | A separate verifier session reads the brief, the diff and the gate evidence, and passes or lists concrete failures. Failure creates a `fix` job with the findings. | 3 fix rounds, then escalate. |
| **L4 Planner** | on events + 30 min heartbeat | Gets a digest (finished, failed, stalled, inbox) and acts with `vowp job add/update` and brief files: decomposes, reprioritises, reviews reports against evidence, writes checkpoint summaries. | One planner turn at a time. |
| **L5 Keeper** | on stalls/failures + 20 min heartbeat | Diagnoses stalls (reads logs), decides nudge / restart / replan / park, writes nudges. **Takes over the planner role when the planner's harness is unavailable.** | One keeper turn at a time. |
| **L6 Failover** | on `limit` events | Provider limit → capsule → the next ladder rung. Probes the limited provider after the reset time (one tiny call, then every 15 min) and switches back with a capsule. | Never silently moves work to a *paid* provider the profile doesn't list. Credits exhausted → park + inbox. |
| **L7 Human** | continuous | Questions and approvals go to `inbox/` (and optionally a phone notification). Only the asking job waits. | — |
| **L8 Watchdog** | systemd user service (`Restart=always`, linger on) + 5-min cron check | Keeps L1 alive across crashes and reboots. | — |
| **L9 Schedules** | every tick | Runs due rows of the `schedules` table (cron expressions or intervals): create a job, start a loop, or run a command. Examples: "every 30 min planner heartbeat", "07:00 weekdays morning report", "01:00 dependency-update job", "hourly provider-credit check", "nightly database backup". | Missed runs while the node was down run once, not N times. |

Loops are rows too (`loops` table: kind, interval, bounds, last/next run, state), so `vowp loops` lists them and you can pause or resume any loop. System cron and systemd only keep the supervisor itself alive.

**No-progress detection:** a job is "stalled" after 20 min with no new events and no new commits. A loop that repeats the same action 3 times without new evidence is stopped and escalated.

### 7.1 Database schema (v0.1, one SQLite file per node)

| Table | Holds |
|---|---|
| `nodes` | This node and known remote nodes (name, ssh target, last seen, capabilities) |
| `projects` | Projects homed on this node (path, repo, default profiles, environments) |
| `jobs`, `edges` | The graph: job fields from 6.1, edges with kind |
| `runs` | Every attempt: job, role, harness, model, native session id, pid/window, start/end, exit, outcome |
| `gate_results` | Gate, pass/fail, evidence (command, exit, output digest/blob ref), time |
| `workers` | Slots and their current run |
| `sessions`, `session_events` | Universal sessions and an index of their logs (the JSONL stays authoritative) |
| `capsules`, `switches` | Handoffs and lineage |
| `loops`, `schedules` | Loop definitions/state; cron and interval schedules with last/next run |
| `ledger` | Dispatches, merges, promotions and costs (usd, tokens, source: receipt/price-table/unknown) |
| `inbox` | Questions and approvals for humans, with answers |
| `limits` | Per provider/harness: kind, until, last probe |
| `releases` | Promotions with serving snapshot and rollback command |

Postgres or replicated SQLite (libSQL) is a v0.3 option for teams and multi-node sync; v0.1 stays embedded and zero-ops.

---

## 8. Gates (as data)

| Gate | Passes when | Evidence recorded |
|---|---|---|
| `report` | The report exists and its front-matter has the required fields with valid values | Parsed front-matter |
| `cmd` | The command exits with the expected code (optional output pattern) | Command, exit code, last 200 lines, duration |
| `owns_respected` | `git diff main...job/<id>` touches only `owns` | File list |
| `pushed` | The branch/commit exists on the remote | `git ls-remote` output |
| `review` | The verifier passed | Verifier report |
| `script` | A custom checker exits 0 | Same as `cmd` |
| `deploy` | The deploy command ran **and** the post-deploy probe matched | Versions before/after, probe output, rollback command |

**The anti-pattern is banned:** a gate may never be "file exists" for anything that must *work* (deploy scripts, tests, builds). It must execute and check results.

**Report front-matter:**

```yaml
---
job: graph-engine
status: done            # done | partial | blocked
branch: job/graph-engine
commits: [abc1234, def5678]
tests: { command: "cargo test -p vowp-graph", passed: 42, failed: 0 }
files_changed: [crates/vowp-graph/src/scheduler.rs, …]
open_items: []
questions_for_owner: []
---
```

**Brief front-matter** (the job's contract, generated from the graph node plus the planner's text):

```yaml
---
job: graph-engine
owns: ["crates/vowp-graph/**"]
must_not_touch: ["everything else"]
needs_done: [protocol]
gates: [report, "cargo test -p vowp-graph", "cargo clippy --workspace --all-targets -- -D warnings", owns_respected, review]
done_when: "report has status: done and every gate passes"
---
```

---

## 9. Work → Production

```toml
[env.work]
checkout = "~/myapp"
deploy = ["supabase functions deploy {fn} --project-ref WORK_REF"]

[env.prod]
checkout = "~/myapp-prod"           # fast-forward-only clone of main
deploy = ["vercel deploy --prod"]
guards = ["clean_tree", "head_equals_remote", "build", "no_active_jobs:./scripts/guard.sh"]
approval = "human"                 # human | policy
serving_bank = "./scripts/bank-serving.sh {target}"   # snapshot what is live before deploying
```

`vowp promote <change> --to prod` runs: guards → **serving bank** (snapshot live) → diff candidate vs serving (must equal the intended change) → approval → deploy → probe → record version, rollback command and release-log line. `vowp rollback <release>` redeploys the banked serving copy.

The guards exist because **live is not always git**, **a local build is not the deploy build** (upload-set rules like `.vercelignore` are checked), and **a refused deploy is staged, never worked around**.

---

## 10. Providers, models and failover

Profiles are configured in `vowp.toml`. Each rung is `harness:model[@effort][#variant]`; the exact model ids are whatever the named harness accepts. A placeholder example:

```toml
[profiles.planner]
ladder   = ["claude-code:model-a@medium", "opencode:provider/model-b#max"]
on_quota = "next_then_probe_back"      # on a limit: capsule -> next rung; probe the limited provider after the reset

[profiles.keeper]
ladder = ["opencode:provider/model-b#max"]

[profiles.worker]
ladder = ["opencode:provider/model-b#max"]

[profiles.verifier]
ladder = ["opencode:provider/model-b#max"]

[providers.provider]
max_concurrent = 6
on_credits_exhausted = "park"     # report the blocker, never switch to another paid provider silently
```

`vowp doctor` checks that each profile's harnesses are installed and that provider credentials are present **by environment-variable name only**; it never prints a value and never calls the network.

| Signal | Example | Action |
|---|---|---|
| `quota` | "You've hit your limit · resets …" | Capsule → next rung; probe back after the reset |
| `credits` | 402 / insufficient balance | Park every job on that provider; inbox; stop |
| `rate` | 429 | Backoff with jitter; lower that provider's concurrency |
| `auth` | 401 / login expired | Park; inbox ("log in again") |
| `context` | Context overflow | Capsule → fresh session on the same harness |
| `crash` | Process exited without a report | L2 continue (×2) → keeper |

**Cost ledger:** opencode's `step_finish` events and Claude's stream-json `result` carry usage and cost. VOWP records `usd` with `source: receipt`, or a price-table estimate labelled `source: price-table`, or `unknown` (never assumed free). `vowp cost` shows totals by job, role, provider and model.

---

## 11. Framework files the agents follow

Shipped under `framework/` (`templates/`, `skills/`, `opencode/`) and installed by `vowp init`:

- **`AGENTS.md`** (harness-neutral). Covers working autonomously until the report exists, owned paths only, never committing secrets (names only), reporting costs, verifying against real artifacts, and staging any refused action rather than working around it.
- **`CLAUDE.md`** that just imports `AGENTS.md` (`@AGENTS.md`), so both harnesses read the same rules.
- **`brief.md` and `report.md` templates** with the front-matter above.
- **Skills** in `.agents/skills/`, loaded by both harnesses: `vowp-worker` (the job contract), `vowp-planner` (how to shape the graph, split ownership and write briefs), `vowp-verifier` (how to judge evidence).
- **`opencode.jsonc` agent definitions** for planner, keeper, worker and verifier, with permission presets so no dialog ever needs a keystroke.

---

## 12. Repo layout and stack

**Rust (stable), one static-ish binary `vowp`.** Crates: `rusqlite` (bundled SQLite), `serde`/`serde_json`, `clap`, `tokio` only where streams need it (prefer plain threads), `axum` for the v0.2 API, `cron` for schedules, `globset` for ownership, `ulid`, `time`. Distributed as prebuilt binaries for Linux (x86_64/arm64) and macOS (arm64/x86_64) on GitHub Releases, an `install.sh`, Homebrew and `cargo install vowp` later, and optionally an npm wrapper with prebuilt binaries (the esbuild/Codex pattern). **MIT** license. The v0.2 web window is TypeScript and is served by the daemon as static files.

```
vowp/
├─ crates/
│  ├─ vowp-protocol/  events, capsule, job, gate, profile, config types (serde) + JSON Schema export
│  ├─ vowp-store/     SQLite schema + migrations, JSONL logs, blobs, reindex
│  ├─ vowp-graph/     DAG + loop edges, state machine, ownership locks, scheduler (pure functions)
│  ├─ vowp-tmux/      safe window manager (private-socket tests)
│  ├─ vowp-adapters/  claude-code, opencode, cli (generic)
│  ├─ vowp-core/      supervisor loops, schedules, gates, merge queue, failover/switch, capsule writer,
│  │                  ledger, inbox, checkpoints, promote/rollback
│  └─ vowp-cli/       the `vowp` binary
├─ framework/        AGENTS.md, CLAUDE.md, brief/report templates, skills, opencode.jsonc (embedded in the binary)
├─ docs/             concepts, quickstarts, guides, reference, design
├─ examples/hello-vowp/   a tiny project with a 3-job graph you can run in 5 minutes
├─ scripts/          install.sh, systemd unit, cron watchdog
└─ .github/workflows/ci.yml   fmt, clippy, test on Linux + macOS; release binaries on tags
```

### CLI v0.1

```
vowp init                         # vowp.toml + framework files + .vowp/ in the current repo
vowp doctor                       # node, tmux, git, harnesses found, providers reachable by env name
vowp up | down | status | board   # supervisor + tmux session
vowp plan "goal"                  # planner turns a goal into a graph (you review)
vowp job add|update|ls|show|retry|park
vowp graph [--mermaid]
vowp attach [job]                 # tmux attach / pretty log follow
vowp session ls|show|chat|switch|export|import|watch
vowp inbox ls|answer
vowp cost [--by job|role|model]
vowp node add|ls                  # ssh nodes; any command takes --node <name>
vowp promote|rollback|releases
vowp loops [pause|resume <loop>]
vowp schedule add "<cron>" <job-template|loop|command> | ls | pause | rm
vowp reindex | backup
```

---

## 13. Security and compliance

- The supervisor and any API bind to `127.0.0.1` only. Remote access is SSH (or a tunnel). A per-node credential is used for the v0.2 API.
- VOWP stores **no credentials**. Harness logins stay in each harness. Provider keys are environment variables the user already has. Logs and reports refer to secret **names**, never values. A secret-scan gate runs before every push.
- Stock binaries only: no client spoofing, no "Sign in with Claude" inside VOWP, no credential forwarding. Using a subscription means running the user's own, signed-in, official `claude` CLI on their own machine.
- `cwd` allow-list per node. Jobs run in their own worktrees.

---

## 14. Scope

**v0.1:** everything in sections 4–12 except the HTTP API, the web window, ACP and multi-user.

- **Included:** protocol, store, graph, tmux, both adapters, supervisor loops L1–L8, failover, capsule switch (CLI), CLI core, framework templates, docs, install, example, CI.
- **Optional in v0.1:** promote/rollback, website, `vowp session watch` importer for interactive Claude sessions.

**v0.2:** daemon HTTP+SSE API, the web/PWA window, ACP adapter, a phone-friendly board, cross-node session sync, a cost dashboard.

**v0.3:** multi-user, quotas, server metrics, parallel fan-out policies, hosted relay.
