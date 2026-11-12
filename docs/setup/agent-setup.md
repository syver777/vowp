# Agent setup contract — VOWP 0.3.0

> **Status: current — VOWP 0.3.0.** This is the contract the README prompt points to. The setup,
> control, fleet, capacity, secret and verification commands described here are implemented. Nothing on
> this page asks for, prints or stores a secret value.

This page is written for the **setup agent**: the AI harness the owner started with the copyable
prompt at the top of the root README. That prompt sends you here, at the exact release tag, and tells
you to follow this page exactly.

The owner is responsible for their VPS, its data and its bills. Your job is to run the installer they
approved, stop at the human hand-offs and report what happened. You are not the VPS administrator and
you do not improvise around the plan.

## 1. What you are setting up

VOWP 0.3 has one **home** and, usually, one **control machine**:

| Term | Meaning in this contract |
|---|---|
| **home** | One Linux VPS that owns the global database, the project databases, the session logs and the supervisor. |
| **control machine** | The computer where the owner talks to the home: snapshot cache, SSH tunnels, the orchestrator surface. It holds no truth. |
| **project** | One git repository registered on the home. |
| **session** | One conversation with one id and an append-only log on the home. |
| **surface** | Where the owner is talking to a session now: an app, `vowp chat`, or the MCP server an app calls. |
| **orchestrator** | The long-lived session that reads the fleet snapshot and asks the owner before changing anything. |

The owner normally talks through the opencode desktop app or a Claude app. `vowp chat` is the
fallback surface. The setup agent does not replace either app; it installs the home, joins the
control machine and proves both sides can reach each other.

This contract assumes you run on the computer that will be the control machine, with key-only SSH to
the home. If the owner is running you on the home itself, or on a machine that cannot reach the home
with an SSH key, stop and ask before continuing.

## 2. Ground rules

These rules apply before every command in this page.

1. **Ask once.** Ask all the questions in §3 in one message. Do not drip questions phase by phase.
   If an answer is incomplete, ask only for the missing part.
2. **One plan, one yes.** Show the whole plan from §5 once, then ask for one approval. Do not ask for
   a yes per phase. The only change before that approval is the local CLI bootstrap in §4.1, and you
   tell the owner about it in the same question message. If an answer changes, re-run the plan and
   ask again: that is a new plan.
3. **No secret values in chat, ever.** Values never appear in the conversation, in a command
   argument, in an environment variable you set, in a file you write, or in your report. Names are
   fine. The safe paths are in §7.
4. **Do not work around a hand-off.** If setup exits 5 and asks for a human, stop and wait. A
   hand-off is part of the contract, not a failure.
5. **Never guess.** If the plan, the owner's answer and this page disagree, stop and ask. Do not
   invent a flag, a path or a command.
6. **Nothing public or destructive.** Do not push, publish, deploy, change repository visibility,
   open a public port, change DNS or firewall rules, create cloud or provider accounts or SSH keys,
   or delete data. Setup does not need any of those.
7. **Do not touch what you were not asked to set up.** Keep the owner's existing projects, tmux
   sessions, harness logins and data. Do not edit a database, a session log or a repository by hand.
8. **Report honestly.** A warning stays a warning and a failed check stays a failure. Never turn a
   failure into a success in your summary.

## 3. Step 1 — Ask once

### 3.1 Check the basics without changing anything

Confirm these before you ask the owner:

- You can run commands on this computer.
- `ssh` is available.
- VOWP 0.3 documents Linux homes on `x86_64` or `arm64`; macOS is supported as a control machine. If
  the owner's target is something else, stop and ask.

Once the owner gives you the target, key-only login must work:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 <ssh-target> true
```

If this prompts for a password or fails, stop before the plan and show the owner §11, "SSH key in
5 minutes". Do not ask for a password and do not run `ssh-copy-id` yourself; the owner must do that
step.

### 3.2 Ask the five questions

Send one message. Keep the five numbered questions, but write them in the owner's language and
style. Ask for **names**, not values.

```text
I can set up VOWP 0.3.0 for you. Before I show you the plan, I need five answers:

1. Home SSH target. Which `user@host` (or `~/.ssh/config` alias) should I use for the VPS?
   You need to log in with an SSH key, not a password; I will check that without changing anything.
2. Harnesses and providers. Which harness(es) should the workers, verifiers, planner and keeper
   use — `opencode`, `claude-code`, or both — and which provider name does each use? Names only;
   never send me a key.
3. Repositories. Which existing checkouts on the home should I register? Give the path on the
   home, not a URL or a path on this computer. Say if a checkout still needs `vowp init`.
4. GitHub remote. Should merged work be pushed to a GitHub remote so it is safe off the VPS?
   Recommended, never required. Setup does not push or create a remote.
5. Control machine. Should this computer be a control machine (snapshot cache, tunnels and the
   orchestrator surface)? Default yes.

If `vowp` is not already installed on this computer, I will install the pinned 0.3.0 binary
under `~/.local/bin` (no service, no cron, no watchdog) so I can show you the plan. That is the
only change before you approve the plan.
```

If the owner has no preference for question 2, offer the harness default `opencode` with the
provider they already use. Do not choose a provider for them and do not ask for a key.

If the owner has no repository yet, accept that answer. The projects phase can finish with no
projects; `setup verify` uses its own temporary fixture project and does not need the owner's
repository.

### 3.3 What not to ask

- Do not ask for a password, a private key, an API key, a token, a `.env` file or a harness login.
  Those are hand-offs (§7), not questions.
- Do not ask for the VPS provider's console password or the root password.
- Do not ask a question that this page already answers.
- Do not ask the owner to choose a flag spelling or a path inside `~/.vowp`; the setup plan owns
  those details.

## 4. Step 2 — Bootstrap the CLI, then check without changing anything

### 4.1 Bootstrap the local CLI

The setup commands run on the control machine. If `vowp` is not already on `PATH`, install the same
pinned version from the release the prompt names. The owner was told about this line in §3.2. It
installs the per-user binary under `~/.local/bin`, and enables no service, no cron and no watchdog.

```sh
if ! command -v vowp >/dev/null 2>&1; then
  curl -fsSL "https://raw.githubusercontent.com/syver777/vowp/v0.3.0/scripts/install.sh" \
    | sh -s -- --download --version 0.3.0 --no-service --no-cron --no-watchdog
fi
command -v vowp >/dev/null 2>&1 || export PATH="$HOME/.local/bin:$PATH"
vowp --version
```

The last command must print `vowp 0.3.0` for the version in the prompt. If it prints a different
version, stop and report the mismatch; do not continue with a mixed install. If the archive for this
OS or architecture does not exist, stop and ask; do not build from an unrelated checkout.

Tell the owner again in the plan summary that this local line is part of the setup, even though the
remote plan prints only the home phases.

### 4.2 Run the plan

Pass the answers as flags. `--target` is the home's SSH destination; it is **not** the global
`--home` directory flag. Repository paths are paths **on the home**; setup does not clone a URL.

```sh
vowp setup plan --target <ssh-target> \
  --version 0.3.0 \
  --name <home-name> \
  --repository <path-on-the-home> \
  --harness opencode \
  --provider <provider-name> \
  --secret <SECRET_NAME> \
  --github-remote
```

Every answer flag is optional except `--target`:

| Flag | Meaning |
|---|---|
| `--target <ssh-target>` | The home's SSH destination (`user@host` or an SSH config alias). Required. |
| `--version <version>` | The release to install. Pass the version from the prompt; the default is this binary's version. |
| `--name <name>` | The home node name; defaults to the target's short hostname. |
| `--repository <path>` | A repository checkout on the home to register. Repeatable. Alias: `--repo`. |
| `--harness <name>` | A harness the workers should use (`opencode`, `claude-code`). Repeatable. |
| `--provider <name>` | A provider name the chosen harnesses use. Repeatable, names only. |
| `--secret <NAME>` | A secret name the home should have. Repeatable, names only. |
| `--github-remote` | Add the recommendation to push merged work to a GitHub remote. Setup never pushes. |

Run both forms; the command is read-only and safe to repeat:

```sh
vowp setup plan --target <ssh-target> ...      # text for the owner
vowp setup plan --target <ssh-target> ... --json
```

`plan` opens read-only SSH probes on the home. The probes check the OS and architecture, the login
user and home, free disk, whether `git`, `tmux`, `curl`, `systemctl`, `loginctl`, the harnesses and
`vowp` are present, the sudo mode, and whether the chosen harnesses appear signed in. The command
prints only the plan: it does not print a separate probe report. A failed requirement becomes a
hand-off in that plan (for example install `git`, sign in to a harness or free disk space); the probe
values are not otherwise echoed. It writes nothing. A missing `vowp` on the home is the install
phase, not an error.

What the plan contains:

- the target and the VOWP version;
- every phase, every command (argv, no shell interpolation) and every file it will touch;
- the `requires` entries: `sudo`, `network`, `systemd`, `login:<harness>`, `provider:<name>`,
  `secret:<NAME>` (name only), `ssh-key-login` and `owner-answer:<question>`;
- whether each phase is idempotent;
- the hand-offs it expects, including the approval hand-off.

The `--json` document has exactly four top-level fields: `target`, `version`, `phases` and
`handoffs`. The text form prints the same content, phase by phase.

### 4.3 Check the plan against the owner's answers

Before you show the plan, compare it with §3:

- Do the target and version match the prompt?
- Does the projects phase name the repositories the owner gave, and no others?
- Does the harness/provider list match the owner's answer?
- Are the hand-offs the ones the owner should expect?
- Does the plan contain any secret value, token or private key? It must not; only names.
- Does any command touch something outside the home, the control home, the installed binary and
  service, the system packages the plan lists, and the project repositories the owner named? If so,
  stop and ask. Setup does not change firewalls, DNS, public ports or other repositories.

If the plan and the answers disagree, show the owner the difference and stop. Do not run a plan that
does something they did not ask for.

## 5. Step 3 — Show the plan once, get one yes

Show the full text plan, not a summary that hides a phase. Then add a short plain-language summary:

```text
This is the whole plan for <ssh-target>. It has <n> phases and will:
- install VOWP 0.3.0 on the home;
- create the non-root `vowp` user if passwordless sudo is available, or use no-sudo mode;
- create the home, the global database, the service and the vault (or basic secret mode);
- register <repositories>;
- set up this computer as a control machine.
It writes <n> files and runs <n> commands. It needs: <hand-offs>.
No secret values are in the plan; it names <secret names> only.

Run it? (yes/no)
```

Rules for the approval:

- One yes covers the whole plan. Do not ask "phase 1 ok? phase 2 ok?".
- If the owner says no, stop. Do not run a subset unless the plan itself has a documented way to do
  that; this contract does not.
- If the owner changes an answer, go back to §4, run the plan again and ask for a new yes.
- If the owner asks a question the plan does not answer, stop and say the plan is incomplete. Do not
  guess and do not continue on a maybe.

Do not run `setup home --apply` without `--yes`: that is a local guard, exits 5 with an `approval`
hand-off and does not open SSH.

## 6. Step 4 — Run the plan

### 6.1 Run the home phase

Run this from the control machine. Do not wrap it in `ssh`; the command opens the connection itself.
Do not prefix it with `sudo`; setup creates the non-root user only when passwordless sudo is
available, and otherwise deliberately runs in no-sudo mode.

```sh
vowp setup home --target <ssh-target> --apply --yes \
  --version 0.3.0 \
  --name <home-name> \
  --repository <path-on-the-home> \
  --harness opencode \
  --provider <provider-name> \
  --secret <SECRET_NAME> \
  --github-remote
```

Pass the same answer flags you passed to `plan`. Add `--json` if you need to parse the result instead
of the text output. `--yes` is the single approval from §5; never pass it before the owner has said
yes.

`setup home` is idempotent. Re-running it after a hand-off continues from the phase that stopped and
does not redo finished work. Do not run `vowp up` or `vowp down` yourself; setup starts the service
when systemd is available.

The phases are:

| Phase | What it does | Changes |
|---|---|---|
| **preflight** | SSH key login, OS/arch, disk (at least 5 GiB), `git`, `tmux`, `curl` and harness presence. Never reads a secret. | Nothing. |
| **user** | Creates the non-root `vowp` user and copies the login user's authorized keys when sudo is passwordless; otherwise runs as the login user and records no-sudo mode. Then checks the harness sign-in as that user. | A Unix user and its SSH key, or a recorded mode. |
| **install** | Runs the pinned release installer (`--download --version <version> --no-service --no-cron --no-watchdog`) and verifies the installed version. | The `vowp` binary under the effective user's `~/.local/bin`. |
| **home** | Creates `~/.vowp/`, the global database, `config.toml`, `vowp.toml` and the reserved `_node` project database; runs `vowp doctor`. | The home directory and database files. Existing config files are not overwritten. |
| **service** | Installs the user unit `vowp.service`, enables linger and starts it when systemd is available. | A user service. A missing systemd is a warning, not a silent success. |
| **vault** | Creates the vault user and helper when sudo is passwordless; otherwise uses basic secret mode and writes no `secrets/mode` marker. Never stores a secret value. | A vault user/helper, or basic mode. |
| **projects** | Registers the repositories the owner named. Each must already exist on the home and contain `vowp.toml`. | Project registry rows and project databases. |
| **tunnels** | None by default. Tunnels are opened from the control machine when needed. | Nothing by default. |

The result is a `SetupOutcome` with `status`, `phase`, `changed`, `handoff` and `checks`. Treat any
`handoff` as a stop; go to §7. `vowp doctor` is reported as a check: a warning or failure does not
stop the phase, but you must report it and not call it a success.

`setup home` writes `~/.vowp-control/setup.json` on the control machine so the next step knows the
target and the home path it actually used.

### 6.2 Run the control phase

Run this only after the home phase exits 0 and only if the owner answered yes to question 5. Show its
plan once; `--apply` is the approval.

```sh
vowp setup control --apply
vowp setup control --apply --json
```

It reuses the target from the home run or an existing `~/.vowp-control/control.json`; pass
`--target <ssh-target>` if this is a different control machine or there is no saved state. `--name`
sets the home node name. `--force` retargets an existing control configuration and is never used
without the owner's explicit yes.

It writes `~/.vowp-control/` (`control.json` at mode 0600, the spool, the node mirror, the surface
and tunnel tables, the setup record) and, when the home has one, fetches `snapshot.json` into the
snapshot cache with an `as of` time. It installs no supervisor. If the control configuration already
points at another home, the command exits 4 unless `--force` is given. Never point it at a different
machine.

If the owner declined a control machine, skip this step and say so in the report; the control-side
checks in §8 are then not verified.

### 6.3 Read the result

- Exit 0: the phase or the whole command succeeded. Continue.
- Exit 5 with `{"status":"handoff",...}`: stop and do §7.
- Exit 5 without a hand-off: a guard refused, or the target is unsupported. Report the message and
  hint; do not work around it.
- Exit 7: SSH or remote failure. Check the target and the key, but do not retry blindly. Report it.
- Any other non-zero exit: stop. Show the error text or JSON (`error.message`, `error.hint`) and do
  not hand-fix the phase with your own commands.

Do not hide warnings. They belong in the report exactly as printed.

## 7. Hand-offs the human must do

The setup stops at a hand-off and exits 5 with a `handoff` object (`phase`, `kind`, `prompt`). Relay
the prompt to the owner in plain words, wait for an explicit confirmation, then re-run the **same**
command. Never start a new plan unless the hand-off changed one of the five answers.

| Hand-off | What the owner does | What you do | Never |
|---|---|---|---|
| **approval** | Reads the whole plan and says yes. | Show the plan; run `setup home --apply --yes` only after the yes. | Pass `--yes` before approval; run a subset. |
| **sudo command** | Runs the exact command setup printed in their own terminal on the home. | Give the command; wait for "done"; re-run setup. | Ask for the password in chat; store it; echo it; put it in a file. |
| **harness login** | Opens the harness's device-code URL or signs in on the home with the harness's own flow. | Relay the URL or code if the plan asks you to; wait; re-run setup. | Ask for the account password or token; copy `~/.claude` or opencode credential files. |
| **secret value** | Enters it with `vowp secret set NAME` (hidden prompt) or `vowp secret request NAME` (one-time link). | Tell them the name and the safe method; wait for "saved"; check the name and last-4 only. | Ask them to paste it in chat; put it in argv, an environment variable or a file; read `~/.vowp/secrets`. |
| **owner answer** | Answers the `owner-answer:<question>` from the plan, for example a repository without `vowp.toml`. | Use the answer; re-run the plan if it changes the plan. | Guess; pick a default the plan did not state; edit the repository yourself. |
| **GitHub remote** | Adds the remote and pushes `main` after setup. | Report the recommendation. | Push, create a repository or change visibility. |

Setup never asks for the sudo password. When sudo asks for a password, it runs in no-sudo mode and
uses basic secret mode. A sudo hand-off appears only if a printed vault command fails even under
passwordless sudo. A harness login is not a request for the owner's account password; the
device-code flow is enough. A secret is entered only through the hidden prompt or the one-time page;
you see `saved, ends in …last4`, never the value.

If the owner pastes a secret value into the chat anyway, do not repeat it, do not store it, and tell
them to rotate it and use the safe path.

## 8. Step 5 — Verify

Run this on the control machine after the home and control phases succeed:

```sh
vowp setup verify
vowp setup verify --json
```

`verify` runs the checks on the home over SSH when it is run on a control machine, and directly when
it is run on the home. It prints each check as `ok`, `warn` or `fail`, and exits 1 if any check
fails; warnings do not fail the command. It uses a temporary fixture project on the home and does not
touch the owner's real projects.

The checks:

- **`gc.dry_run`** — a read-only gc report; nothing is deleted.
- **`fixture.job_merge`** — a fixture job runs one fake harness turn, writes its report, passes its
  ownership gate and merges through the real merge queue. The fixture project, its database and its
  session directories are removed afterwards.
- **`control.snapshot`** — on a control machine, fetch the home's fresh `snapshot.json` and write the
  control cache; the check validates the cache and its `as of` time. On the home, the check writes a
  fresh snapshot and reports a `warn` that readability from a control machine was not verified,
  because the home cannot prove that side.
- **`orchestrator.chat`** — create a project-bound orchestrator with the chat app, send it a turn
  through `vowp chat`, and check the assistant event in the session log. The orchestrator directory
  is removed afterwards.

If a check fails, stop and report it. Do not edit the result, re-run until it passes without
understanding why, or call it a success because the warning is small. If the owner declined a control
machine, the snapshot check cannot be run from this computer; say that it was not verified instead of
claiming it passed.

## 9. Step 6 — Report

Give the owner a short report in the chat. If they ask for a file, write it outside any git
repository; never commit it.

```text
VOWP 0.3.0 setup — <home name or ssh target>

Home: <host> (<os>/<arch>), home at <path>, supervisor: <running/stopped>
Control: <this computer>, ~/.vowp-control, snapshot as of <time or "not verified">
Projects: <names and paths, or "none yet">
Harnesses/providers: <names only>
Secrets: <names and mode (vault user or basic), last-4 only if shown>
Verify: <n> ok, <n> warn, <n> fail; exit <code>
Running now: <supervisor, orchestrator session, tunnels>
Warnings: <exact text, or "none">
Next for you: <join the orchestrator from the desktop or phone; add a project; set a provider key;
             configure a GitHub remote; anything that needs the owner>
```

Rules for the report:

- Give versions of the local `vowp`, the home `vowp`, `git` and `tmux`, and the OS/arch of the home.
- Count the checks; do not paste a wall of JSON.
- Say what is running and how the owner reaches it.
- Put the owner's next actions last and make them concrete.
- Keep secret values, tokens, private keys, full `.env` contents and harness credentials out of the
  report. Names and last-4 only.
- If something was skipped, say it was skipped and why.
- If a warning or failure remains, say so in the first paragraph, not only in a table.

## 10. Stop conditions and refusals

Stop and ask the owner when:

- the SSH key login does not work in batch mode;
- the local CLI cannot be installed, or its version does not match the prompt;
- the target is not a Linux VPS, or its architecture is unsupported;
- `setup plan` exits 5 because it cannot proceed, or 7 because SSH fails;
- the plan differs from the owner's answers, or contains something they did not ask for;
- a phase exits with an error other than a hand-off;
- `setup verify` has a failure;
- the owner asks for a command outside the plan, or the next step is ambiguous.

Refuse, and explain why, when asked to:

- put a secret, password, token or private key in the chat, an argument, an environment variable or
  a file in the repository;
- read `~/.vowp/secrets`, `.env`, SSH private keys or harness credential stores;
- edit a database, a session log, a capsule, a report or the control cache by hand;
- run as root, change the firewall or DNS, open a public port, or change SSH server hardening;
- push, publish, deploy, change repository visibility, or create cloud or provider accounts or SSH
  keys;
- kill a tmux server, a supervisor or another owner's process;
- install software outside the plan (including extra harnesses or system packages).

A refusal is a stop, not a detour. Put the reason in the report and let the owner decide.

## 11. SSH key in 5 minutes

Give this section to the owner if the batch-mode check fails. The owner runs these commands; the
setup agent does not handle private keys.

```sh
# 1. Create a key if you do not already have one. Skip if you do.
ssh-keygen -t ed25519 -C "vowp-home"

# 2. Copy the public key to the home. This asks for the login password once.
ssh-copy-id <user>@<host>

# 3. Prove key-only login works. This must not ask for a password.
ssh -o BatchMode=yes -o ConnectTimeout=10 <user>@<host> true && echo "key login ok"
```

If `ssh-copy-id` is not installed, print the public key with `cat ~/.ssh/id_ed25519.pub` and append
it to `/home/<user>/.ssh/authorized_keys` on the home. On the home, the directory should be mode
0700 and the file mode 0600. On a cloud provider, the public key can be added in the provider's
console when the instance is created. If the instance already exists and password login is disabled,
use the provider's console to add the public key; do not re-enable password login just for setup.

An SSH config alias keeps the target short and makes the key explicit:

```sshconfig
Host vowp-home
  HostName <host>
  User <user>
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
```

Then use `vowp-home` as `<ssh-target>`. Do not use `sshpass`, do not store a password in a script,
and do not send the private key anywhere.

## 12. Command reference

```sh
vowp setup plan --target <ssh-target> [answer flags] [--json]
vowp setup home --target <ssh-target> --apply --yes [answer flags] [--json]
vowp setup control --apply [--target <ssh-target>] [--name <name>] [--force] [--json]
vowp setup verify [--json]
```

The answer flags are `--version`, `--name`, `--repository` (`--repo`), `--harness`, `--provider`,
`--secret` and `--github-remote`; they are described in §4.2. All are repeatable except `--version`,
`--name` and `--github-remote`.

| Command | Changes anything? | Notes |
|---|---|---|
| `setup plan` | No. | Prints every phase, command and file; the only remote actions are read-only probes. A missing remote `vowp` is the install phase, not an error. |
| `setup home --apply` | Yes. | Idempotent. Runs the eight phases in §6.1. A hand-off exits 5. |
| `setup control --apply` | Yes, on this computer. | Writes `~/.vowp-control/` and fetches the snapshot cache; no supervisor. Run after the home succeeds. |
| `setup verify` | Runs a temporary fixture on the home. | Prints `ok|warn|fail` per check; exits 1 on a fail, 0 otherwise. |

The JSON shapes are the spec's `SetupPlan` (`target`, `version`, `phases`, `handoffs`),
`SetupPhase` (`id`, `title`, `commands`, `files`, `requires`, `idempotent`), `SetupHandoff`
(`phase`, `kind`, `prompt`), `SetupOutcome` (`status`, `phase`, `changed`, `handoff`, `checks`) and
`SetupCheck` (`name`, `status`, `detail`). A plan never contains a secret value, a token or a private
key; `requires` names a secret by `<NAME>` only. Errors are the usual VOWP JSON error document:
`{"error":{"kind","message","hint"}}`.

Relevant exit codes:

| Code | Meaning here |
|---|---|
| 0 | Success. |
| 1 | `setup verify` has a failed check. |
| 2 | Bad arguments, a missing `--target`, an invalid name, a repository URL instead of a home path, or an option-shaped target. |
| 4 | A control configuration already points at another home; pass `--force` only after the owner's yes. |
| 5 | Hand-off needed, `--yes` missing, an unsupported target, a plan that cannot proceed, or `setup control` with no target. Stop and follow the message. |
| 7 | SSH or remote failure. |

The full CLI contract is in the [v0.2 specification](../design/v0.2/home-and-surfaces.md) §11 and
§12. This page is the user-facing contract; it follows the shipped commands, and a future spec
amendment will align the two.

## 13. What setup does not do

- It does not clone a repository, run `vowp init` for you, or change a project's `vowp.toml`. It
  registers an existing checkout; `vowp init` is offered as a hand-off, never forced.
- It does not push, create a GitHub repository or change visibility. `--github-remote` only adds
  the recommendation.
- It does not install a harness or a provider account, and it never reads a harness login.
- It does not open a public port, change a firewall or DNS record, or install a system package.
- It does not run `vowp up` or `vowp down` for you; the service phase starts the user unit where
  systemd is available.
- It does not edit the owner's databases, session logs or repositories. It never reads or prints a
  secret value; secret files move only through the documented secret store and upgrade path.
- It does not create a second home. VOWP 0.3 has one home, and a control machine holds no truth.
