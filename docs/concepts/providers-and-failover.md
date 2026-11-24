# Providers and failover

VOWP treats a model as a setting, not an assumption. A role has a **profile**: an ordered ladder of
executors. The first rung whose provider is available runs the work; when a provider is limited, VOWP
either moves to the next rung with a capsule or parks the affected jobs and asks the owner. It never
silently moves work to a paid provider that the profile does not list.

> **Status: current.** Adapters, provider classification, the global limit rows and the `vowp status`,
> `vowp session`, `vowp inbox` and `vowp cost` commands are implemented on `main`.

## Profiles and ladders

```toml
# vowp.toml — excerpt; see docs/design/architecture.md §5.2 for the full example
[profiles.planner]
ladder = ["claude-code:model@medium", "opencode:provider/model#max"]
on_quota = "next_then_probe_back"   # move to the next rung, probe the first after the reset

[profiles.keeper]
ladder = ["opencode:provider/model#max"]

[profiles.worker]
ladder = ["opencode:provider/model#max"]

[profiles.verifier]
ladder = ["opencode:provider/model#max"]
```

The four role names `planner`, `keeper`, `worker` and `verifier` must exist after project and machine
configuration are merged. A job's `profile` field selects its ladder; by default it is its role name.

`on_quota` controls what a quota limit means:

| Value | Behaviour |
|---|---|
| `next_then_probe_back` | Move to the next unlisted-by-limit rung now; probe the limited provider after its reset and switch back when it succeeds. Default. |
| `next` | Move to the next rung and stay there. No probe-back. |
| `park` | Park the affected jobs and open an inbox item. |

### One home, shared limit rows

The global database holds one `limits` row per provider, so every project sees the same limit state and
dispatch skips a limited provider. The follow-up action is **project-local** in this build:

- `credits` and `auth` parking runs in the project whose supervisor handles the event and parks only
  that project's jobs; the global `park:<provider>` request is consumed by one project tick, so another
  project's jobs may not be parked automatically;
- a `rate` limit updates the shared row, but the effective concurrency count is computed from one
  project's open runs;
- a `quota` limit moves the session that hit it, and dispatch in every project skips that provider
  until the reset.

Machine-wide parking and a shared concurrency cap are code follow-ups. Job cost stays attached to its
project and run; the limit row is the shared fact.

### Executor grammar

```
harness ":" model [ "@" effort ] [ "#" variant ]
```

- `harness` is `claude-code`, `opencode`, `cli` or (for VOWP's own events) `vowp`.
- `@effort` is Claude Code only; `#variant` is opencode only. Both on one executor is invalid.
- The provider key is `claude` for Claude Code, the text before the first `/` for opencode
  (`opencode:provider/model#max` → `provider`), and the mapping entry name for a generic CLI.

This one string is what you put in a profile, pass to `vowp session switch --to`, or see in
`vowp session show`.

## Provider limits

Adapters classify harness output into five limit kinds. The supervisor never re-parses the text; it
acts on the classified event.

| Kind | Typical signal | Action |
|---|---|---|
| `quota` | "You've hit your limit · resets …" | Record the reset time (+2 min margin), hand the session to the next unlisted rung with a capsule, probe back after the reset. If no rung remains, park the job with `ProviderBlocked` and one inbox notice. |
| `credits` | 402 / insufficient balance | Record `credits` with no reset time. `on_credits_exhausted = "park"` (default): park every job whose **current** rung uses that provider, open one inbox item, stop dispatching to it, never switch. `"next"`: still record the row, then move the affected session to the next healthy rung declared in its ladder; if none is healthy, park as above. |
| `rate` | 429 / overloaded | Back off with jitter (10 min doubling to 60 min, ±20 %), lower that provider's concurrency cap until the reset, and delay the continue run without consuming a continue. |
| `auth` | 401 / login expired | Record `auth`, then apply the same provider policy as credits: `park` (default) parks jobs on that provider and opens an inbox item asking the owner to log in with the harness's own login; `next` advances to the next healthy declared rung. No retries in place. |
| `context` | Context window overflow | Write a capsule, start a fresh native session on the same harness seeded with it, and continue. The continue count is unchanged. |

A crash or error without a limit signal is not a provider limit. It goes through the normal L2 continue
path: two automatic continues, then the keeper. Three consecutive crashes or errors from the same
provider with no limit classification mark that provider `unavailable` for 15 minutes. That row is
temporary: an expired `unavailable` row no longer blocks the rung, and an `ok` run clears it at once.

### Mark lifetimes

L6 records a provider limit in two places: the global `limits` row (kind, reset, detail) and the mark
keys `limit:<provider>`, `probe_back:<provider>`, `probe_back_until:<provider>` and `park:<provider>`.
The marks have different lifetimes:

| Mark | Lifetime |
|---|---|
| `quota`, `rate`, `unavailable` | While `resets_at` is in the future. |
| probe-back | A bounded window starting at the reset (or at the mark when no reset is known): 10 minutes at first, doubling after each failed probe, capped at 60 minutes. When the deadline passes the provider becomes eligible for dispatch and for a probe even if no probe has succeeded. |
| `credits`, `auth` | Sticky until the owner answers the inbox item. They never expire. With `on_credits_exhausted = "next"` the provider also carries the `probe_back` mark; the row itself is still only cleared by the owner. |
| `park:<provider>` | A one-tick request; the supervisor drains it. |

A run on a provider that ends `ok` clears that provider's limit row and its `limit:`, `probe_back:`,
`probe_back_until:`, `probe_pending:` and `park:` keys: a successful turn is proof the provider works.
A `credits` or `auth` row is never cleared this way; only `vowp inbox answer <id> resolved` resolves
it. In L6's `next_rung`, an expired `unavailable` row is ignored, so a residual mark cannot outrank a
healthy sibling rung.

### Quota failover, step by step

1. The adapter emits a `limit` event with `kind=quota` and, when the harness reports it, `resets_at`.
2. L1 records the limit row with `resets_at = reported + 2 min` (no known time: now + 60 min).
3. The session is switched to the next rung whose provider is not limited, using the normal
   [switch protocol](sessions-and-switching.md#switching): freeze, drain, capsule, detach, start, record.
4. A job run that was interrupted continues on the new rung with `StartRun{Continue}`. The switch
   records the target machine, probes it over SSH when it is remote, and only then detaches the source.
5. When the reset arrives, a probe runs a tiny prompt (`Reply with the single word: ok`) on the limited
   provider in a scratch directory, with a 60-second timeout. Its cost is recorded like any other run.
6. A successful probe deletes the limit row and its marks. Planner and keeper sessions switch back to
   rung 0 with a capsule only when no turn is running; ordinary jobs stay where they are until their
   next natural start, avoiding mid-run flip-flops.
7. A failed probe updates `last_probe_at` and is retried every 15 minutes. If it reports a new reset
   time, that replaces the old one. The probe-back window itself grows 10 → 20 → 40 → 60 minutes, so a
   probe that never succeeds cannot block the provider forever: when the deadline passes the rung is
   eligible again even before a probe answers.

If the target probe fails, or every remaining rung is limited, the switch aborts and the job is parked
with an inbox notice. The session stays readable; a human can retry after fixing the provider.

### Credits and auth: park by default, opt-in failover

Credits exhausted and expired auth mean "a human must act". VOWP records the limit row, then applies
`[providers.<name>].on_credits_exhausted`:

| Value | Behaviour |
|---|---|
| `park` | **Default.** Park every job whose **current** rung uses that provider and open exactly one inbox item per provider and kind. Never switch. |
| `next` | Write the same limit row, set the provider's `probe_back` mark, and move the affected session to the next healthy rung declared in the profile's ladder, with the normal capsule switch. If no healthy rung exists, park as under `park`. |

`next` is an explicit owner opt-in and it **spends money on the next declared rung**: the affected
session switches there with a capsule, and later starts that would pick the exhausted provider pick the
next healthy declared rung instead. The fallback provider bills at its own rates. `park` stays the
default because it cannot spend on a provider the owner did not choose for that role. A ladder is a
budget declaration: set `on_credits_exhausted = "next"` only when rung 2 is explicitly configured for
that profile and you accept its cost.

The policy decision belongs to L6, at the moment the run ends. Harvest in L1 is only the backstop for
the default `park`: it applies a pending `park:<provider>` request first, then parks the provider itself
only when the effective policy is `park`. With `next`, harvest leaves the job alone, because the session
has already moved or L6's no-healthy-rung fallback has already parked it. That is why the opt-in is not
undone on the next tick, and why the fallback opens exactly one notice instead of a second provider-wide
park. Harvest does not choose a new action; it only gates this backstop on the recorded policy.

The limit row is written for both policies, so `vowp status`, the ledger and `vowp cost` show why the
rung changed. `vowp inbox answer <id> resolved` removes a parked provider's limit row and returns its
jobs to `ready`; with `next` the affected session has already moved and the sticky `credits` or `auth`
row remains until the owner answers the notice.

## Provider concurrency and spend

```toml
[providers.provider]
max_concurrent = 4
on_credits_exhausted = "park"                  # default: park and ask; "next" spends on the next declared rung

[providers.subscription]
max_concurrent = 1                             # keep a subscription-backed provider at one run
```

Use your own provider keys as the table names. In this build `max_concurrent` caps concurrent runs per
provider **within one project**; the limit rows are global, but the count is project-local. A rate limit
lowers the effective cap until its reset, then restores it.

**Capacity and providers are separate controls.** Capacity admission asks whether the machine can start
another run, and it includes the provider's active limit and smallest `max_concurrent` in its target.
It does not choose a provider or a model. Failover is what moves a session to another rung. Cost is not
an admission input: VOWP reports it but does not enforce a spending budget. See [capacity](capacity.md)
for the measured target, brakes and waiting reasons.

`[node].max_workers` is a per-machine preference, not a hard cap: it seeds the advised target and the
worker-slot bookkeeping, while admission uses the capacity target. `max_verifiers` still bounds the
verifier share of the slots.

If a harness reports usage but no cost receipt, add a price table under the provider:
`[providers.<provider>.prices."<model>"]` with `input_per_m` and `output_per_m`, plus
`cache_read_per_m` and `cache_write_per_m` when the provider charges for cache tokens. Use the
provider's current prices; without a table the cost stays `unknown`.

## Cost is reported, not capped

Every run produces a `Cost`:

| Source | Meaning |
|---|---|
| `receipt` | The harness reported usage and cost (opencode `step_finish`, Claude Code stream-json `result`). |
| `price-table` | Estimated from `[providers.<p>.prices]` because the harness reported usage but no cost. |
| `unknown` | No usable numbers. `usd` is `None`; it is never treated as zero. |

`vowp cost` groups totals by job, role, provider, model or day, shows the source for each row and counts
unknown costs separately. Totals are also written to the ledger and to planner checkpoints. There are no
invented budgets; bounds are on retries, no-progress loops and measured capacity instead.

## What VOWP never does

- It never stores a provider key or a harness login. Keys are environment variables the user already
  has, or secrets injected by name; `env_pass` in `[harnesses.<name>]` contains **names**, never values.
- It never switches to a paid provider that the profile does not list.
- It never advances on credits or auth unless the owner set `on_credits_exhausted = "next"` for that
  provider; the default is `park`.
- It never retries credits/auth failures in a loop.
- It never mid-run flips a job to another provider just because a probe succeeded.
- It never lets capacity overrule a provider limit, or a provider choice overrule the measured target.
- It never changes the capacity target from a model's advice; only the owner applies a target.

Next: [capacity](capacity.md), [Work → Production](work-to-production.md), or the
[operating guide](../guides/operating.md) for `vowp status` and `vowp cost`.
