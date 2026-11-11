# Setup

> **Status: current — VOWP 0.2.0.** The setup path is implemented and is described by the
> [agent setup contract](agent-setup.md).

VOWP is set up by the copyable prompt at the top of the [root README](../../README.md). The prompt
sends a harness to [agent-setup.md](agent-setup.md), which asks five questions once, shows the whole
plan once and runs it after one approval. The `vowp setup` commands do the work, so the result does
not depend on which harness runs the prompt.

This directory has no separate installation steps. For the manual binary install, release archives,
systemd unit and the on-disk layout, see the [install guide](../guides/install.md).
