# Capacity

Capacity is how VOWP decides **how many runs the home may start right now**, and when to stop starting
more. It is measured from the machine, admitted deterministically, protected by brakes, and advised by
a model that never changes the target itself. The owner sets the number when the advice and the machine
disagree.

Capacity is not a spending budget and it never kills a running job. Cost is reported separately; see
[providers and failover](providers-and-failover.md).

> **Status: current.** The sampler, admission, brakes, targets, advisor and the `capacity` commands are
> implemented on `main`. The binding specification is
> [`docs/design/v0.2/home-and-surfaces.md`](../design/v0.2/home-and-surfaces.md) §6.

## The three layers

```text
measure ──────────────► admit ──────────────► brake
 each tick              before each run        stops new runs
 host + per-run         target, RAM, CPU,      memory pressure,
 usage + estimates      disk, provider         swap growth, disk,
                                               provider limits
        │
        └──────────────► advise ──────────────► owner decides
                         every 30 min and       capacity set --target N
                         on events              (the AI only proposes)
```

The default mode is `auto`: the supervisor derives a target from the latest measurements and the p90
estimates. The owner can switch to `fixed N` at any time. An override above the advice is honoured, but
it is shown as an override and the brakes still apply.

## Measure

### Host samples

While the supervisor runs, each node tick reads the host once and writes a `host_samples` row. The
node tick is 60 seconds by default; the row's fixed `interval_secs=5` label is not a separate 5-second
thread. The sample contains:

- memory total, available and swap used, from `/proc/meminfo` on Linux;
- load averages and CPU count; the cgroup v2 CPU quota when one is present;
- free disk on the home and worktree filesystems;
- memory, CPU and I/O pressure (`avg10`) when `/proc/pressure/*` exists;
- the sample source, `proc` or `fallback`.

On macOS or a minimal container without `/proc`, the sampler marks the sample `fallback`: memory and
load stay zero, CPU count comes from `available_parallelism()`, disk free from the portable `df -Pk`
read, and cgroup values are absent unless those files exist. Admission then skips the RAM and CPU
checks and uses the owner's fixed target when one is set, otherwise 1. The disk check still runs, and
vowp `capacity show` prints `capacity: fallback (no /proc), target <n>` so the degradation is visible.
Auto capacity never silently claims a measured number on such a host.

Samples are written once per node tick (60 seconds by default) and kept for 24 hours. The pruner can
also retain one-minute rows for 30 days if they exist, but the sampler does not create them in this
build; a minute rollup is still a follow-up. The GC pass prunes expired rows (see
[data lifecycle](data-lifecycle.md)).

### Per-run usage

Every adapter run and every command gate gets a scope, because a worker's memory is usually dominated
by what it runs — `cargo build`, `npm test`, a gate — not by the agent process itself. VOWP prefers a
systemd user scope (`systemd-run --user --scope`) and reads cgroup v2 `memory.peak`, `memory.stat`,
`cpu.stat` and `memory.events`. If user cgroups are not delegated, it walks the process tree and sums
RSS and CPU deltas. If neither works, it records a labelled default.

The fallback is honest but less exact: the process-tree walk overcounts warm shared RSS by about
31 % and undercounts short-lived CPU by 15–36 % in measurement. It is for keeping a node limping,
not for precise estimates.

One `run_usage` row is written per finished run with the peak and sampled memory, CPU use, duration,
build mode (`cold`, `warm`, `incremental` or `unknown`), source, OOM flag and exit code. Build mode
matters: a cold Rust build peaked at about 2.45 GiB and 300 s CPU in the capacity measurements,
while warm runs peaked at about 0.37 GiB and 56–60 s CPU.

### Estimates

A `usage_estimates` row is computed per exact `(project, job kind, harness, model, build mode)` from the
last 50 runs. With fewer than five samples, VOWP pools in this order until a group has five:

1. the exact key;
2. the same project, job kind, harness and build mode;
3. the same project, job kind and build mode;
4. the same job kind and build mode across projects;
5. the constant `('*','*','','','unknown')` default.

A `cold` estimate never pools into a warm or incremental group before the constant, because the gap is
too large. Each row records p50 and p90 RAM and CPU, p50 and p90 duration, the sample count and whether
the source is `measured`, `pooled` or `default`.

## Admit

Before a job is dispatched — including its worker, verifier or fix run — the capacity manager answers
`Admit` or `Wait(reason)`. Planner and keeper turns start when due and are not admitted through this
path. The merge queue also bypasses it; it starts when a job reaches `merging`. The checks are
deterministic and run in this order:

1. **Brake.** An active brake returns its first reason.
2. **Provider.** If the run's provider has an active limit, wait.
3. **Target.** If the number of running runs is already at the worker target, wait.
4. **RAM.** After adding the run's p90, available memory must stay above the reserve:
   `max(1 GiB, 10 % of MemTotal)`.
5. **CPU.** Running plus estimated CPU, divided by the CPU quota, must stay at or below 0.85.
6. **Disk.** The home and, when known, the worktree filesystem must have at least 5 GiB free.

A `Wait` leaves the job exactly where it was — `ready` or `pending` — writes the reason to the job's
note (`waiting: RAM 1.2 GB short`) and to the snapshot's queue card. It never changes job state and
never starts a partial run.

Admission is **node-wide**. The manager opens each active project database in turn, sums the open runs
and their estimated CPU, and compares the total with the node target. A project handle never decides
the node's capacity from its own runs alone.

### Fairness

Every active project gets one L1 tick per node tick. The node supervisor orders projects by **fairness
weight**, highest first. Inside one weight the project admitted last moves to the back of its group, so
the next project in the rotation goes first; when no project in a group was admitted last, a per-tick
rotating offset starts that group. A registry read failure falls back to plain name order with the same
offset. Every active project still gets its turn. Within a project, ready jobs dispatch by priority and
then age. `project weight` is shown in project and snapshot output and now changes the order; `vowp
project weight <name> <1-100>` sets it. `max_workers` in
`vowp.toml` is a preference, not a cap: it is the bootstrap before the first host sample and sizes the
worker-slot bookkeeping, but admission uses the capacity target and the measured checks. When the target rises
above the configured preference, the supervisor grows the worker rows so the board still shows a slot
per run.

## Brake

A brake stops **new admissions**. It never signals, kills, parks or throttles a running process. It
trips on measured conditions and clears after two clean samples:

| Brake | Trips when |
|---|---|
| `memory_pressure` | `/proc/pressure/memory` `avg10` is above 10.0 for two consecutive samples. |
| `swap_growth` | Swap used grew by more than 64 MiB per minute over the last three samples. |
| `disk_low` | Free disk on the home or worktree filesystem is below 5 GiB in the latest sample. One low sample trips it; the brake clears after two clean samples. |
| `provider` | Every configured provider has an active limit. |

vowp `capacity show` prints the active reason and its start time, for example
`brake: memory pressure (avg10 14.2)`. A kernel OOM kill is recorded on the run, the job is parked with
a node-inbox notice, and its estimate's p90 is raised for the next admission. The run itself is not
retried by the brake; the normal job retry path decides.

## Targets and advice

### `auto`

The advised target is:

```text
advised = clamp(min(ram_fit, cpu_fit, provider_cap), 1, 64)
ram_fit = floor((mem_available - reserve) / max_measured_ram_p90)
cpu_fit = floor((cpu_quota * 0.85) / max_measured_cpu_p90)
provider_cap = smallest max_concurrent among the configured providers
```

The RAM and CPU terms use the largest p90 across stored estimates that have samples, not the all-runs
pooled row: the target must fit the biggest common run. With no measured estimate the constant default
is used. Before the first host sample, the configured `max_workers` is the bootstrap. A fallback host
(no `/proc`) proposes 1 rather than pretending to measure. The target is written as a history row when it
moves by more than one worker, with `set_by='supervisor'`, and appears as `advised_target` in the
snapshot.

### `fixed`

vowp `capacity set --target N` writes the owner's number. `N` above the advice requires `--override`; without
it the command exits 5 and prints the advised number. A fixed target is shown as
`owner override: 8 (advised 5)` and the RAM, CPU, disk and brake checks still apply.

### Advice

Every 30 minutes, and on events (a new project, a brake trip, an OOM kill, sustained idle), the capacity
advisor reads the same report as vowp `capacity report --json` and writes:

- one `capacity_advice` row with the current and proposed target, mode, reasons and expected effect;
- one node-inbox `decision` item titled `Capacity: <n> workers (advised <a>)` with the options
  `apply` and `keep`.

The advisor **never changes the target itself**. Applying a proposal is the owner's
vowp `capacity set --target N`, through the CLI or the `capacity_set` MCP tool. The tool records the actor.
This is a tested property, not a convention.

## Commands

| Command | What it does |
|---|---|
| vowp `capacity show [--json]` | The current report: host, running, target/advice, brake, reserves and top estimates. |
| vowp `capacity report [--json] [--since <duration\|date>] [--limit <n>]` | The same report, filtered for the advisor or a human review. |
| vowp `capacity set --target <n> [--mode auto\|fixed] [--override] [--reason <text>] [--by <actor>]` | Writes one target history row. `--mode fixed` is the default and requires `--target`; `--mode auto` ignores the number and recomputes. |
| vowp `fleet capacity [--json]` | The snapshot's smaller capacity card: target, advised target, run count, brake and `measured` (currently none). Use vowp `capacity show` for host, estimates, waiting jobs and limits. |

vowp `capacity show` prints `host`, `target`, `running`, `brake`, `reserve`, `estimates` and `waiting`. The
JSON document is the same `CapacityReport` the advisor reads, so a script does not have to scrape the
human output.

The `capacity` tmux window on `vowp-home` renders the same body and redraws on a TTY.

## Reading a wait

A job that is not starting is not necessarily broken. Read vowp `fleet queue` or vowp `fleet status`:

| Reason | What it means | What to do |
|---|---|---|
| `RAM 1.2 GB short` | The next p90 would eat the reserve. | Wait, reduce the target, or add memory. |
| `CPU 1.20 core(s) short` | The CPU quota would exceed 85 %. | Wait or add CPU. |
| `disk 3.4 GB free; minimum is 5.0 GB` | The home or worktrees are nearly full. | Run vowp `gc`, clear space, or move worktrees. |
| `provider <name> limited: …` | The provider has a quota, rate or auth limit. | Wait for the reset, or fix auth/credits. |
| `N worker(s) running; target is M` | The target is reached. | Wait, or raise the target if the machine can take it. |
| `memory pressure (avg10 14.2)` | The machine is under pressure. The job note has no `brake:` prefix; `capacity show` adds it. | Let it clear; do not force more runs. |

The waiting reason is the same sentence stored on the job and in the snapshot; it is not a separate
guess.

## Providers and cost are separate

Capacity and provider failover answer different questions:

- **Capacity** asks whether the machine can start another run. It uses RAM, CPU, disk, the worker
  target and the provider's active limit. It does not choose a model or a provider.
- **Failover** asks which executor a session should use after a provider limit. It follows the profile
  ladder and writes capsules; see [providers and failover](providers-and-failover.md).
- **Cost** reports receipts, price-table estimates and unknowns. It is not fed into admission as a
  budget, and there is no hard spending cap in v0.2.

The provider's `max_concurrent` is part of the advised target, and a limited provider produces a
`provider` wait or brake. Credits exhausted and expired auth remain human actions: VOWP parks the
affected jobs and opens one inbox item; it does not switch to an unlisted paid provider.

## Degraded and unusual hosts

- **No `/proc` (macOS, minimal container).** Auto proposes 1, admission skips RAM and CPU, the disk
  check still runs, and the UI says `fallback`. Use a fixed target if the machine can take more.
- **No systemd user cgroups.** Per-run usage falls back to the process tree and its `run_usage` rows
  carry source `proc_tree`; the estimates built from them are still labelled `measured`, `pooled` or
  `default` by sample count, never `proc_tree`. Admission still works but is less precise.
- **A cold build with no history.** The default estimate is deliberately conservative. The first real
  runs replace it quickly; the capacity measurements show why a blanket 1.5 GiB default is too low
  for a cold Rust build.
- **One project is idle.** The advisor can propose a smaller target after sustained idle; it still does
  not apply it.

## What capacity never does

- It never kills or signals a running job. Only an explicit switch/takeover or `vowp down` interrupts.
- It never changes the target from a model's answer. The advisor writes a proposal and an inbox item;
  the owner applies it.
- It never treats an unknown estimate as zero. Missing measurements become a labelled default.
- It never admits a run through a provider that is limited, even when RAM and CPU are free.
- It never uses cost as a hard cap. Cost is reported; retries and no-progress loops are what is bounded.

Next: [data lifecycle](data-lifecycle.md), or [home and surfaces](home-and-surfaces.md).
