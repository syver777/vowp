# Schedules

A schedule fires an action on a cron expression or a fixed interval: create a job from a template,
trigger a loop (for example the planner), or run a command. Schedules are rows in the database, so
`vowp schedule ls` shows their last and next run, and you can pause one without editing
`vowp.toml`.

> **Status: current.** The schedule engine, `vowp doctor` validation and the `vowp schedule` commands
> are implemented. Schedules are per project; with several projects on the home, select a checkout,
> pass `--project <name>`, or use `--all` where the command supports it.

## Add a schedule

```sh
vowp schedule add "0 7 * * 1-5" loop l4-planner --name morning-report --tz +02:00 --missed run_once
vowp schedule add "every 30m" command --name heartbeat -- vowp status --check
vowp schedule add "0 1 * * *" job-template --name dep-update --template-file deps.json
```

The first argument is a 5-field cron expression (`min hour day-of-month month day-of-week`) or an
interval `every <n><s|m|h|d>` (minimum 60 seconds). The second is the action kind:

| Action | Form | Effect |
|---|---|---|
| `loop <name>` | Trigger one run of a loop now. | Marks the loop due; the next tick runs it. |
| `command -- <argv…>` | Run a command without a shell. | Captures the last 200 lines into the ledger; a non-zero exit records `failed: exit N`. |
| `job-template --template-file <json>` | Insert a job from a `NewJob` JSON. | The id gets a `-<yyyymmddhhmm>` suffix and the job enters the normal graph. |

Options:

| Flag | Meaning |
|---|---|
| `--name <name>` | Stable schedule name; unique per project. |
| `--tz <UTC\|+HH:MM>` | Evaluation timezone. VOWP supports `UTC` and fixed offsets only. |
| `--missed run_once\|skip` | What to do when a run was missed while the node was down. |
| `--template-file <path>` | `NewJob` JSON for `job-template`. |

The command validates the cron/interval, computes and prints the next run, and stores the row. A bad
cron or an interval under 60 seconds exits 2.

## List, pause and remove

```sh
vowp schedule ls
vowp schedule --all ls
vowp schedule pause daily-backup
vowp schedule resume daily-backup
vowp schedule rm daily-backup
```

`ls` prints `NAME SPEC ACTION STATE LAST NEXT STATUS`; `--all` adds every project's rows on the home.
A paused schedule keeps its row and next-run calculation but does not fire. `rm` deletes it.

## Or declare schedules in `vowp.toml`

The same schedules can be committed with the project:

```toml
[[schedules]]
name = "morning-report"
cron = "0 7 * * 1-5"
tz = "+02:00"
missed = "run_once"
action = { kind = "loop", loop_name = "l4-planner" }

[[schedules]]
name = "daily-backup"
cron = "0 1 * * *"
action = { kind = "command", argv = ["vowp", "backup"], timeout_secs = 600 }
```

Validation follows the same rules: exactly one of `cron` or `every`, a 5-field cron, an interval of at
least 60 seconds, `tz` as `UTC` or `±HH:MM`, and unique names. The full cron parse runs in `vowp doctor`,
`vowp schedule add` and at supervisor startup; a string that has five fields but is not a
valid expression is a configuration error there.

`vowp init` writes `schedules = []`, so it installs **no** schedules. Entries declared in `vowp.toml` are
created active when the supervisor loads them; pause one with `vowp schedule pause`. The
planner heartbeat is not a schedule; it is L4's own 30-minute heartbeat.

## Missed runs and idempotency

The node may be down at the scheduled time. When a schedule is due:

- If it is at most two ticks late, it fires normally.
- With `run_once`, it fires **once** for the earliest missed occurrence, never N times.
- With `skip`, it records "skipped missed run" and advances.

Each firing is keyed by `<schedule_id>@<due>` in the ledger. The key is written before the action's
effects, so a crash between the two cannot double-fire. At-most-once is the deliberate trade: a lost
firing is logged on restart rather than silently repeating a deploy or a job.

After a firing, `last_run_at` is set to now and `next_run_at` is the first match strictly after now, so
all missed occurrences collapse into the one run.

## Examples

**Daily database backup.** A command action keeps the SQLite backup fresh without a shell:

```toml
[[schedules]]
name = "daily-backup"
cron = "0 1 * * *"
action = { kind = "command", argv = ["vowp", "backup"], timeout_secs = 600 }
```

**Weekday planner heartbeat.** L4 already has a heartbeat, but an explicit morning trigger is useful
when you want a checkpoint at a fixed time. A loop action marks `l4-planner` due.

**Dependency update job.** Write a `NewJob` JSON file and point `job-template` at it. The template is
validated like `vowp job add`; the generated id is suffixed with the due time so repeated firings do not
collide.

**Health check.** A command action can run `vowp status --check` every 30 minutes. A non-zero
exit is recorded in the schedule status and opens one inbox notice per schedule per day.

## Relationship to loops

Loops are the supervisor's own periodic work (L1–L9); schedules are user-defined rows handled by L9.
Pause a loop with `vowp loops pause <name>` when you want to stop that category of work; pause
a schedule when you want to stop one cron entry. Neither cancels a run that is already in progress.

Next: [operating a home](operating.md) for the loop table and backups.
