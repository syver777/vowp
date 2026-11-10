# Support

VOWP is an early, source-first project. Support comes from the maintainer and the community on a
**best-effort** basis: there is no service-level agreement, paid support or guaranteed response time.

## Where to ask what

| I want to… | Use… |
|---|---|
| Report a reproducible bug | [Open an issue](https://github.com/syver777/vowp/issues/new/choose) and choose **Bug report**. |
| Request a feature or an improvement | Open an issue and choose **Feature request**. |
| Ask how to use VOWP or whether it fits my setup | Open an issue and choose **Question**. |
| Read the documentation | The [guides](docs/guides/README.md), [concepts](docs/concepts/README.md) and [troubleshooting](docs/troubleshooting.md) pages. Start with the [install guide](docs/guides/install.md) and [quickstart](docs/guides/quickstart.md). |
| Report a security vulnerability | Follow [SECURITY.md](SECURITY.md) and use the repository's [private vulnerability reporting](https://github.com/syver777/vowp/security/advisories/new). Do not open a public issue. |
| Contribute code, tests or documentation | Read [CONTRIBUTING.md](CONTRIBUTING.md) and the [code of conduct](CODE_OF_CONDUCT.md). |

GitHub Issues is the support channel. The issue tracker keeps answers searchable for the next person,
and it lets several maintainers and contributors help. Please do not send support requests to personal
accounts.

## What to include

A useful report has:

- the output of `vowp --version` and how you installed VOWP (release archive, `cargo install` or a
  source build with the commit);
- your operating system and architecture;
- the harness and its version, for example Claude Code or opencode;
- the complete `vowp doctor --no-color` output;
- the smallest reproduction: commands, `vowp.toml` keys that matter, and the exact error or exit code;
- relevant logs or `vowp status` output, trimmed to the interesting part.

**Never paste secrets.** Remove API keys, tokens, `.env` contents, private hostnames, home paths and any
log line that may contain them. Refer to a secret by its environment-variable name only, never by
value. A maintainer may edit or delete a comment that contains a secret.

## Expectations

- **Best effort.** The maintainer answers when time allows. A clear reproduction with the evidence
  above is the fastest route to an answer.
- **0.1.x is early software.** Fixes land on `main` and in the latest `0.1.x` build. Older pre-release
  builds are not supported, and configuration or interfaces may still change before 1.0.
- **Security first.** Reports sent through private vulnerability reporting are handled privately and
  take priority over normal issues. See [SECURITY.md](SECURITY.md).
- **Harness and provider bugs go upstream.** If a problem is in Claude Code, opencode, a model provider
  or another third-party tool, report it to that project. VOWP can only fix what it controls.
- **Issues may be closed without a reproduction.** If a report cannot be reproduced or lacks the
  version and doctor output, it is hard to act on; you are welcome to reopen it with more detail.
- **Conduct.** All project spaces follow the [code of conduct](CODE_OF_CONDUCT.md). Harassment or abuse
  in an issue is not a support problem and is handled under that policy.
