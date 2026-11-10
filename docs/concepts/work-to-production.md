# Work → Production

VOWP separates "the code is merged" from "the change is live". A change that passed every gate still goes
through environment guards, a snapshot of what is actually serving, a diff of the intended change, a
human approval by default, a deploy and a post-deploy probe. Every step is recorded, and every successful
deploy leaves behind the command that can undo it.

> **Status: current.** Promotion, rollback and release listing are implemented; the commands on this
> page run against the code on `main`.

## Environments

An environment is a checkout plus the commands that make it live. `[env.work]` is staging;
`[env.prod]` is production. Both are optional, but a promotion needs the target environment defined.

```toml
[env.work]
checkout = "~/myapp"
deploy = ["./scripts/deploy-work.sh"]

[env.prod]
checkout = "~/myapp-prod"                 # fast-forward-only clone of main
deploy = ["./scripts/deploy-prod.sh"]
guards = ["clean_tree", "head_equals_remote", "build", "no_active_jobs:./scripts/guard.sh"]
approval = "human"                        # human | policy
serving_bank = "./scripts/bank-serving.sh {target}"   # snapshot what is live BEFORE deploying
probe = "./scripts/probe-prod.sh"
rollback = "./scripts/rollback.sh"                    # optional explicit command
```

| Key | Meaning |
|---|---|
| `checkout` | Directory the deploy runs in. Required. |
| `deploy` | One or more commands that ship the change. |
| `guards` | Built-in or custom checks that must pass before anything is deployed. |
| `approval` | `human` (default) creates an inbox approval item; `policy` allows `--yes` to approve automatically. |
| `serving_bank` | Command that snapshots the currently live artifact before the deploy. `{target}` is expanded. |
| `probe` | Command that proves the deployed version is live and correct. |
| `rollback` | Optional explicit rollback command. With `serving_bank` set, omitting it restores the banked snapshot with `tar`; if both are omitted, the release has no rollback command. |

## The promotion pipeline

`vowp promote <change> --to <env>` accepts a commit, ref or job id and runs:

1. **Guards.** Built-in checks and custom scripts run in the target checkout. A failure refuses the
   release; nothing is deployed and nothing is retried with a workaround.
2. **Serving bank.** The current live artifact is snapshotted with `serving_bank`. This is what rollback
   will redeploy.
3. **Diff.** The candidate is compared with the banked serving copy. The diff must be exactly the files
   of the intended change. An unexpected file refuses the release — live is not always git.
4. **Approval.** With `approval = "human"`, an inbox `approval` item is created and the release waits in
   `Banked`. With `approval = "policy"`, `--yes` approves; `--yes` alone never bypasses a human approval.
5. **Deploy.** The environment's `deploy` commands run in its checkout.
6. **Probe.** The post-deploy probe runs and must pass.
7. **Record.** The release row stores the from/to commits, guard results, snapshot path, rollback command,
   probe output and approver, and a line is appended to `~/.vowp/releases.log`.

A guard or diff failure exits 5 and records the release as `refused`. A deploy or probe failure records
`failed` and leaves the rollback available. A release waiting for approval exits 0 and prints the inbox
id; the supervisor retries the same promotion when the approval is answered.

`--dry-run` runs the guards and the diff only. It never deploys and never asks for approval.

## Guards

Built-in guards:

| Guard | Passes when |
|---|---|
| `clean_tree` | The target checkout has no uncommitted or untracked changes. |
| `head_equals_remote` | The checkout's HEAD equals the remote default branch (fast-forward-only clone). |
| `build` | The target's build command succeeds. |
| `no_active_jobs` | No job is currently running in the project, or the named custom script allows it. |
| `upload_set` | The configured upload-set check passes: the files the deploy would upload match the intended build (for example an include/ignore list). |

A custom guard is a command after the colon (`no_active_jobs:./scripts/guard.sh`). It must execute and
check a real result; a file-existence check is not a guard. The same anti-pattern ban as gates applies.
`upload_set` has no built-in check: configure it as `upload_set:<command>`, for example an
include/ignore-list comparison. It exists because a local build is not necessarily the deploy build.

## Release states

| State | Meaning |
|---|---|
| `planned` | The release row exists. |
| `guarded` | Guards passed. |
| `banked` | The serving snapshot is stored; waiting for approval if required. |
| `approved` | Approval is recorded. |
| `deploying` | The deploy command is running. |
| `probing` | The deploy finished; the probe is running. |
| `live` | Deploy and probe passed. |
| `failed` | Deploy or probe failed; rollback is available. |
| `rolled_back` | This release has been rolled back; its `rolled_back_by` field holds the rollback release id. |
| `refused` | A guard or the diff refused the release; nothing was deployed. |

The point of the state machine is that a refusal is a recorded outcome, not an error to route around.
A refused deploy is staged for a human: fix the guard condition, then promote again. It is never retried
automatically with a weakened check.

## Rollback

`vowp rollback <release> [--yes]` redeploys the banked serving copy that was captured before the original
deploy. This requires a rollback command: configure `serving_bank` (the banked snapshot is restored with
`tar` when `rollback` is omitted), or configure `rollback` directly. If neither is set, the release has
no rollback command and `vowp rollback` fails with `has no rollback command`. Rollback creates a new
release row and sets the original release's `rolled_back_by` field to the new release id, runs the
rollback command, and appends the same release-log line format. Rollback does not re-run the promotion
gates: the artifact being restored is the one that was live before.

## Why this is stricter than "deploy on merge"

The design is stricter for three reasons:

- **Live is not always git.** A manual change, a generated asset or a different branch can be serving.
  Banking the live copy and diffing against it catches that.
- **A local build is not the deploy build.** Upload-set rules (ignore lists, include lists, generated
  files) can make the deployed artifact differ from the local checkout. The `upload_set` guard, the diff
  and the probe catch it.
- **A refused deploy must stay refused.** Working around a guard during an unattended run is how a bad
  deploy becomes a worse next day. VOWP records the refusal and asks the owner.

## Commands

```sh
vowp promote <change> --to prod --dry-run
vowp promote <change> --to prod
vowp releases --env prod
vowp rollback <release> --yes
```

`vowp releases` lists `ID ENV CHANGE STATE FROM→TO CREATED`. With `--json` the release objects are the
protocol types. A deployment is a public action: nothing in VOWP deploys, publishes or flips visibility
without an approval item answered by a human, unless the environment is explicitly configured with
`approval = "policy"` and the operator passes `--yes`.

Next: the [security model](../security.md) for the rules behind these guardrails.
