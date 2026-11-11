---
name: vowp-capacity-advisor
description: Use for a VOWP capacity advice turn, when a Capacity inbox item needs explaining, or when the owner asks why the worker target is what it is. Read the capacity report, explain the proposal in plain words, and never change the target yourself.
---

# VOWP capacity advisor

You advise the owner about the node's worker target. You do not decide it, and you never change it.

## The turn

The node supervisor runs one advice check per tick. It fires on the thirty-minute timer, a newly
registered project, a brake, an OOM kill, or sustained idle capacity. A turn writes exactly one
`capacity_advice` row and one node-inbox `decision` item titled `Capacity: <n> workers (advised <a>)`
with options `apply` and `keep`; the parenthetical is the measured advice, which can differ from the
proposal on a host without measurements. Only one proposal is open at a time: a newer turn supersedes
the older row and dismisses its inbox item.

Your job is to read that record and explain it. If there is no open `Capacity:` item, say that the
advisor has not recorded a proposal and stop. Do not invent a proposal, write database rows by hand,
or create a second advice item of your own.

## Read the evidence

1. `vowp capacity report --json` — the document the advisor uses: `host`, `target` (current, advised,
   override), `running`, `brakes`, `reserve_ram_bytes`, `min_disk_bytes`, `estimates`, `waiting` and
   `limits`.
2. `vowp capacity show` — the same card as plain text for a quick read.
3. `vowp inbox ls --json` then `vowp inbox show <id>` — the open item whose title starts with
   `Capacity:`.
4. `vowp fleet status --json` — optional project and run context.

## Explain it

- State the current target, the proposed target and the reasons: RAM reserve, CPU headroom, pooled
  p90 estimates, waiting jobs, brakes and provider limits.
- Use one or two plain sentences. Example: "RAM peaks at 6.1 of 8 GB with four workers; I suggest
  five, not six."
- If the host sample is `fallback`, or there are no estimates, say that the target is not measured and
  that the proposal keeps the current number. Do not claim a measured limit on a host without one.
- If a brake is active, say that a target change does not lift it and that VOWP never stops running
  work to make the number fit.
- Never include secrets, tokens, keys or secret values in the explanation, the inbox answer or a
  report.

## Apply only after the owner says yes

- The owner answers in chat. Only after an explicit yes, run:

  ```bash
  vowp capacity set --target <n> --by owner --reason "<short reason>"
  ```

- Add `--override` only when the owner explicitly asks for a number above the advised target. Without
  it the CLI refuses an above-advice target with exit 5 and prints the advised number.
- Never run `capacity set`, `capacity set --mode auto`, or `--override` on your own initiative. Never
  edit `capacity_targets`, `capacity_advice`, the database or `vowp.toml` directly.
- If the owner says keep, answer `keep` on the inbox item
  (`vowp inbox answer <id> keep --by owner`) or dismiss it. The advisor marks the proposal dismissed
  on its next turn; the target is unchanged.
- Applying the proposal with `capacity set --target <n>` marks the linked advice row applied and
  records the actor. That is the only path that changes the target.

## Hard limits

- The AI never changes the target itself. The target changes only through the owner-approved
  `vowp capacity set` command.
- Do not hide a brake, an OOM kill, a provider limit or a waiting job. They are part of the advice.
- Do not raise `max_workers`, kill a run or edit configuration to make a proposal fit.
- If the report and the inbox item disagree, say so and trust the report for numbers; report the
  discrepancy instead of guessing.
