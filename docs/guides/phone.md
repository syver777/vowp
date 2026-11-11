# Using your phone

The phone is a control surface, not a second home. It needs an SSH app and the owner's key; it does
not need VOWP, a browser, a public port or a copy of the database. The session stays on the VPS home,
so losing the phone loses a window, not the conversation.

> **Status: current.** `vowp chat`, the takeover and follow commands, and the orchestrator join
> instructions are implemented. The phone path uses the same session service as every other surface.

## What you need

- The home's SSH target: `<user>@<your-vps>` or an SSH config alias.
- An SSH app that can hold a private key (Termius, Blink, Prompt and similar apps all work).
- Key-only login to the home. Password login should already be disabled.
- The `vowp` binary installed for the runtime user on the home (the setup does this).
- A session id: the name printed by `vowp orchestrator new`, or a row in
  `vowp session ls --project <project>` for a project-bound session.

The phone never runs the supervisor, opens a database or installs a service. If you want the app
surfaces instead of the terminal, the opencode phone client and the Claude mobile app can join the
same session; the orchestrator prints those instructions.

## Put a key in the SSH app

Use one of these, not a password:

1. **Import an existing key.** Move the private key into the SSH app's key store, or scan a QR code
   the app supports. The private key never leaves the phone and is never sent in chat.
2. **Generate the key on the phone.** The app shows the public key. Add that public line to
   `~/.ssh/authorized_keys` on the home (from the laptop, or with `ssh-copy-id` if the phone app
   exposes a key file). Keep the directory at mode 0700 and the file at 0600.

Prove it before configuring anything else:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 <home-ssh-target> true && echo "key login ok"
```

If this prompts for a password, the key is not installed for that user. Fix the key; do not enable
password login just for the phone.

## Connect and find the session

Open the SSH app, connect to `<home-ssh-target>`, and run:

```sh
vowp orchestrator ls
vowp session ls --project <project>
```

`orchestrator ls` prints `NAME APP STATE SESSION`; the session column is what you pass to `chat`.
`session ls --project <project>` lists that project's sessions, including job sessions. On a home
with several projects and no selected checkout, `--project` is required; without it the command
exits 2 and asks for a project. A project-less orchestrator session appears in `orchestrator ls` and
usually has no row in `session ls`. A session id can be shortened to a unique prefix of at least six
characters; an ambiguous prefix exits 2 and lists the candidates.

If no orchestrator exists yet, create the chat surface on the home (or from the control machine):

```sh
vowp orchestrator new --name main --app chat --executor <executor>
```

The command prints the `vowp chat <session>` line for the phone, plus the desktop and app variants.
The `<home-ssh-target>` in the printed instructions is a placeholder; your SSH app already knows the
real target.

## Talk to the session

```sh
vowp chat <session-id>
```

This is a plain terminal surface. It wraps at 80 columns, needs no colour and prints no wide tables,
so it works in a narrow SSH app. Type a line and press enter for each turn; assistant text streams
and tool calls appear as `· tool: <name>`.

| Slash command | Use it when |
|---|---|
| `/status` | You want the session, executor, node and lease state. |
| `/takeover` | The laptop still holds the session and you want it on the phone (not in follow mode; reopen with `--takeover` instead). |
| `/follow` | You only want to read and not take the lease. |
| `/switch <executor>` | The current provider is limited or you want another model. |
| `/capsule` | You want a handoff document before leaving. |
| `/quit` | You are done; the session stays on the home. |

If the session is already active on the laptop, opening it asks:

```text
Active on laptop since 13:02. Take over? [y/N]
```

Answer `y` to take it. The laptop immediately becomes read-only and prints the takeover notice; a
turn that was running is drained, a capsule is written and the phone continues. Use
`vowp chat <id> --takeover` to skip the prompt, or `vowp chat <id> --follow` to read without taking
the lease.

## Continue the same conversation in an app

The orchestrator's printed `phone` instruction is the exact command for the app it started:

- **opencode:** an SSH app running `opencode --server http://127.0.0.1:<port> --session
  <opencode-session>` against the server on the home. The desktop joins the same server through a
  tunnel and `opencode pair`.
- **Claude:** open the Claude mobile app and choose the same Remote Control session; it runs on the
  home where the claude.ai login exists.

Both are the same VOWP session, so moving between the app and `vowp chat` uses the lease and capsule
rules in [Sessions and surfaces](sessions.md). The phone never needs a public port: the opencode host
binds loopback on the home and the phone reaches it over SSH, while Claude Remote Control connects
outbound to claude.ai from the home and opens no local port.

## Enter a secret from the phone

Use the hidden prompt on the home:

```sh
vowp secret set <NAME>
```

Nothing is echoed, and the value is not in the command or the shell history. When the phone cannot
reach a browser, this is the method; the one-time `vowp secret request <NAME>` page is for a desktop
with a tunnel. Never paste a key into the chat, a report or an issue. The honest limits are in the
[security model](../security.md#secrets).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Permission denied (publickey)` | The key is not in the runtime user's `authorized_keys`, or the app is using another key. Fix the key on the home; do not turn on password login. |
| Host key prompt on every connect | Accept and save the host key in the app; keep the same host name or alias. |
| `vowp: command not found` | Use the absolute path `~/.local/bin/vowp`, or add `~/.local/bin` to the login shell's `PATH`. |
| "vowp home is not initialised" | You are not on the home account. Connect to the runtime user, or pass `--home <dir>` if the home is non-default. |
| `no control configuration` | That is a control-machine command. The phone path runs `vowp chat` **on** the home, not `vowp control`. |
| `session ... not found` | Use a longer unique prefix, or the full id from `vowp session ls` or `vowp orchestrator ls`. |
| `Active on laptop ... Take over?` | Answer `y`, or use `--takeover`. Declining exits 4; use `--follow` to read without taking the lease. |
| `This surface is read-only` | Another surface holds the lease. Reopen with `vowp chat <id> --takeover`; follow mode does not accept `/takeover`. Use `--follow` to stay read-only. |
| Text wraps badly | Keep the app font at a readable size; the surface itself wraps at 80 columns and does not need a wide terminal. |
| SSH drops mid-turn | The session stays on the home. Reconnect and open the same session; events are spooled and replayed without gaps or duplicates. |
| `session is blocked by a provider limit` | Use `/switch <executor>` to move the next turn to another rung, or wait for the reset. |
| The phone is lost | Revoke its key in `~/.ssh/authorized_keys` on the home and remove the app's stored key. The session is unchanged. |

Next: [Sessions and surfaces](sessions.md) for the full lease and switch rules, or
[Control machines and extra nodes](multi-node.md) for tunnels and `--node`.
