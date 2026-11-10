# Providers and failover

VOWP treats a model as a setting, not an assumption. A role has a **profile**: an ordered ladder of
executors. The first rung whose provider is available runs the work; when a provider is limited, VOWP
either moves to the next rung with a capsule or parks the affected jobs and asks the owner. It never
silently moves work to a paid provider that the profile does not list.

> **Status: current.** Adapters, provider classification and the `vowp status`, `vowp session`,
> `vowp inbox` and `vowp cost` commands are implemented.

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

The four role names `planner`, `keeper`, `worker` and `verifier` must exist after project and node
configuration are merged. A job's `profile` field selects its ladder; by default it is its role name.

`on_quota` controls what a quota limit means:

| Value | Behaviour |
|---|---|
| `next_then_probe_back` | Move to the next unlisted-by-limit rung now; probe the limited provider after its reset and switch back when it succeeds. Default. |
| `next` | Move to the next rung and stay there. No probe-back. |
| `park` | Park the affected jobs and open an inbox item. |

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
| `credits` | 402 / insufficient balance | Record `credits` with no reset time, park every job whose **current** rung uses that provider, open one inbox item, stop dispatching to it. Never switch to another paid provider. |
| `rate` | 429 / overloaded | Back off with jitter (10 min doubling to 60 min, ±20 %), lower that provider's concurrency cap until the reset, and delay the continue run without consuming a continue. |
| `auth` | 401 / login expired | Record `auth`, park jobs on that provider, open an inbox item asking the owner to log in with the harness's own login. No retries. |
| `context` | Context window overflow | Write a capsule, start a fresh native session on the same harness seeded with it, and continue. The continue count is unchanged. |

A crash or error without a limit signal is not a provider limit. It goes through the normal L2 continue
path: two automatic continues, then the keeper. Three consecutive crashes or errors from the same
provider with no limit classification mark that provider `unavailable` for 15 minutes.

### Quota failover, step by step

1. The adapter emits a `limit` event with `kind=quota` and, when the harness reports it, `resets_at`.
2. L1 records the limit row with `resets_at = reported + 2 min` (no known time: now + 60 min).
3. The session is switched to the next rung whose provider is not limited, using the normal
   [switch protocol](sessions-and-switching.md#switching): freeze, drain, capsule, detach, start, record.
4. A job run that was interrupted continues on the new rung with `StartRun{Continue}`. The job never
   changes node during failover in v0.1.
5. When the reset arrives, a probe runs a tiny prompt (`Reply with the single word: ok`) on the limited
   provider in a scratch directory, with a 60-second timeout. Its cost is recorded like any other run.
6. A successful probe deletes the limit row. Planner and keeper sessions switch back to rung 0 with a
   capsule only when no turn is running; ordinary jobs stay where they are until their next natural
   start, avoiding mid-run flip-flops.
7. A failed probe updates `last_probe_at` and is retried every 15 minutes. If it reports a new reset
   time, that replaces the old one.

If the target probe fails, or every remaining rung is limited, the switch aborts and the job is parked
with an inbox notice. The session stays readable; a human can retry after fixing the provider.

### Credits and auth are not failover

Credits exhausted and expired auth mean "a human must act". VOWP records the limit, parks every job
whose **current** rung uses that provider, and opens exactly one inbox item per provider and kind. A job
whose ladder happens to contain another provider is still parked — the owner decides after topping up or
logging in. `vowp inbox answer <id> resolved` removes the limit row and returns the parked jobs to
`ready`. The only v0.1 credits policy is `on_credits_exhausted = "park"`.

## Provider concurrency and spend

```toml
[providers.provider]
max_concurrent = 4
on_credits_exhausted = "park"

[providers.subscription]
max_concurrent = 1                             # keep a subscription-backed provider at one run
```

Use your own provider keys as the table names. `max_concurrent` caps concurrent runs per provider. A
rate limit lowers the effective cap until its reset, then restores it. Slots are separate:
`[node].max_workers` and `max_verifiers` bound how many runs a node starts at once.

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
invented budgets; bounds are on retries and no-progress loops instead.

## What VOWP never does

- It never stores a provider key or a harness login. Keys are environment variables the user already
  has; `env_pass` in `[harnesses.<name>]` contains **names**, never values.
- It never switches to a paid provider that the profile does not list.
- It never retries credits/auth failures in a loop.
- It never mid-run flips a job to another provider just because a probe succeeded.

Next: [Work → Production](work-to-production.md), or the [operating guide](../guides/operating.md) for
`vowp status` and `vowp cost`.
