# Guides

These guides walk through installing, running and operating VOWP. They describe the v0.1 experience on
`main`: every command shown is implemented.

> **Status: current.** The build-from-source commands and the per-user install script work today. For a
> runnable three-job graph with fake adapters, see the
> [hello-vowp example](../../examples/hello-vowp/README.md).

| Guide | Start here if you want to… |
|---|---|
| [Install](install.md) | Build the binary, install it on your `PATH`, prepare harnesses and tmux. |
| [Getting started](quickstart.md) | Take one repository from `vowp init` to a running supervisor and back down. |
| [Writing jobs](jobs.md) | Write a brief, choose gates, read a report and understand verify/fix. |
| [Operating a node](operating.md) | Run the supervisor, read status and logs, pause loops, back up and reindex. |
| [Sessions](sessions.md) | Chat, switch model or harness, export/import and watch a session. |
| [Running on a VPS](vps.md) | Install on a server, configure harnesses, run under systemd and lock down SSH. |
| [Multiple nodes](multi-node.md) | Register a VPS, use `--node` and move a session across machines. |
| [Schedules](schedules.md) | Run a job, loop or command on a cron or interval schedule. |

The [concepts](../concepts/README.md) explain the design behind these steps.
