# Guides

These guides take one VPS **home** and the machines that talk to it from installation to a running
fleet. They describe the v0.2 behaviour on `main`: every command shown is implemented and every path
is the one the binary uses.

> **Status: current.** The home, control machine, sessions over surfaces, capacity, secrets,
> lifecycle and backup commands in these guides are implemented. For the shortest install path,
> start with [Install](install.md), which uses the same `vowp setup` commands as the README prompt.

| Guide | Start here if you want to… |
|---|---|
| [Install](install.md) | Set up a home and a control machine, or build the binary by hand. |
| [Getting started](quickstart.md) | Register the first project, see the fleet and talk to an orchestrator. |
| [Writing jobs](jobs.md) | Write a brief, choose gates, read a report and understand verify/fix. |
| [Operating a home](operating.md) | Run one supervisor for every project, read the fleet, manage capacity, back up and collect garbage. |
| [Sessions and surfaces](sessions.md) | Chat, take a session over from another device, switch model or harness, export/import. |
| [Using your phone](phone.md) | Reach the home from an SSH app and continue the same session. |
| [Running a home on a VPS](vps.md) | Prepare the server, register projects, run under systemd and lock down SSH. |
| [Control machines and extra nodes](multi-node.md) | Set up the laptop or phone side, open tunnels and use `--node`. |
| [Schedules](schedules.md) | Run a job, loop or command on a cron or interval schedule. |

The vocabulary is the one in the [v0.2 specification](../design/v0.2/home-and-surfaces.md): one
**home** owns the databases, sessions and supervisor; a **control machine** holds a snapshot cache
and tunnels but no truth; a **surface** is where you are talking to a session. The
[concepts](../concepts/README.md) explain the design, the [troubleshooting guide](../troubleshooting.md)
covers failures, and the [security model](../security.md) states the limits.
