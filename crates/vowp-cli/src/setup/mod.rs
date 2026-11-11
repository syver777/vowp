//! `vowp setup` support types, the plan builder and the text renderer.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §11. The plan is built from one read-only
//! SSH probe and never contains a secret value: `requires` names a secret only.
//!
//! [`commands`] executes the plan; this module keeps the pure data types so `--json` output
//! and the plan tests do not need a live target.

pub mod commands;
pub mod verify;

#[cfg(test)]
#[path = "../../tests/setup.rs"]
mod integration_tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use vowp_protocol::{NodeName, SecretName, Timestamp};
use vowp_tmux::quote::shell_quote;

use crate::CliError;
use crate::IoStreams;

/// The VOWP release repository used by the install phase.
pub const RELEASE_REPOSITORY: &str = "syver777/vowp";

/// The versions setup knows how to install (the release archive names).
pub const SUPPORTED_OS: &str = "Linux";
/// The CPU names the release archives cover.
pub const SUPPORTED_ARCHES: [&str; 2] = ["x86_64", "aarch64"];

/// Returns the `install.sh` command the plan prints and the executor runs.
///
/// Setup installs the pinned v0.2 release with no service, cron or watchdog; the service
/// phase owns the user unit. v0.2 is the first release whose `install.sh` accepts
/// `--download`/`--version`, so the v0.1 scripts are not installable by setup anyway and
/// there is no older flag set to preserve here. The standalone installer keeps its v0.1
/// defaults for callers that run it directly.
pub fn install_script(version: &str) -> String {
    let url = format!("https://raw.githubusercontent.com/{RELEASE_REPOSITORY}/v{version}/scripts/install.sh");
    format!(
        "curl -fsSL {} | sh -s -- --download --version {} --no-service --no-cron --no-watchdog",
        shell_quote(&url),
        shell_quote(version)
    )
}

/// The answers the setup agent collected in its one question message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupAnswers {
    /// SSH destination of the home.
    pub target: String,
    /// VOWP version to install, without the leading `v`.
    pub version: String,
    /// Home node name; the probe hostname is used when absent.
    pub name: Option<String>,
    /// Repositories on the home to register.
    pub repositories: Vec<String>,
    /// Harnesses workers should use (`opencode`, `claude-code`, ...).
    pub harnesses: Vec<String>,
    /// Provider names the chosen harnesses use; only names, never keys.
    pub providers: Vec<String>,
    /// Secret names to check, never values.
    pub secrets: Vec<String>,
    /// Whether merged work should be pushed to a GitHub remote.
    pub github_remote: bool,
}

/// Rejects an SSH target that ssh would parse as an option instead of a destination.
pub fn validate_ssh_target(target: &str) -> Result<(), CliError> {
    if target.trim().is_empty() {
        return Err(CliError::Invalid("ssh target must not be empty".to_owned()));
    }
    if target.starts_with('-') {
        return Err(CliError::Invalid(
            "ssh target must not start with '-' (it would be an ssh option)".to_owned(),
        ));
    }
    if target.chars().any(char::is_control) {
        return Err(CliError::Invalid("ssh target contains a control character".to_owned()));
    }
    Ok(())
}

impl SetupAnswers {
    /// Validates every value and rejects a value that would not be a plain argv entry.
    pub fn validate(&self) -> Result<(), CliError> {
        validate_ssh_target(&self.target)?;
        validate_version(&self.version)?;
        for repository in &self.repositories {
            if repository.trim().is_empty() || repository.chars().any(char::is_control) {
                return Err(CliError::Invalid("repository path must not be empty".to_owned()));
            }
            if repository.contains("://") || repository.starts_with("git@") {
                return Err(CliError::Invalid(format!(
                    "{repository} is a URL; clone it on the home and pass the checkout path"
                )));
            }
        }
        for harness in &self.harnesses {
            validate_name(harness, "harness")?;
        }
        for provider in &self.providers {
            validate_name(provider, "provider")?;
        }
        for secret in &self.secrets {
            SecretName::parse(secret)?;
        }
        if let Some(name) = &self.name {
            NodeName::parse(name)?;
        }
        Ok(())
    }
}

/// Rejects a version that is not semver-like; the release URL is built from it.
pub fn validate_version(version: &str) -> Result<(), CliError> {
    let (core, suffix) = match version.split_once(['-', '+']) {
        Some((core, suffix)) => (core, Some(suffix)),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
        || suffix.is_some_and(|text| {
            text.is_empty()
                || !text
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
        })
    {
        return Err(CliError::Invalid(format!("version is not semver-like: {version:?}")));
    }
    Ok(())
}

/// Rejects an argv name that is not a plain slug.
fn validate_name(name: &str, kind: &str) -> Result<(), CliError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(CliError::Invalid(format!("invalid {kind} name: {name:?}")));
    }
    Ok(())
}

/// The complete setup plan shown to the owner before anything changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlan {
    /// SSH target of the home.
    pub target: String,
    /// VOWP version the plan installs.
    pub version: String,
    /// Phases in execution order.
    pub phases: Vec<SetupPhase>,
    /// Human hand-offs the agent must stop for.
    pub handoffs: Vec<SetupHandoff>,
}

/// One setup phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPhase {
    /// Stable phase id.
    pub id: String,
    /// Human title.
    pub title: String,
    /// Commands as argv vectors; no shell interpolation.
    pub commands: Vec<Vec<String>>,
    /// Files the phase touches.
    pub files: Vec<String>,
    /// Requirements (`sudo`, `network`, `login:<harness>`, `secret:<NAME>`, ...).
    pub requires: Vec<String>,
    /// Whether re-running the phase is safe.
    pub idempotent: bool,
}

/// One human hand-off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupHandoff {
    /// Phase that needs the hand-off.
    pub phase: String,
    /// Kind (`sudo`, `login`, `secret`, `owner-answer`, `approval`).
    pub kind: String,
    /// Question or instruction for the owner.
    pub prompt: String,
}

/// The outcome of one setup run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupOutcome {
    /// `ok`, `handoff` or `failed`.
    pub status: String,
    /// Phase that ran or stopped.
    pub phase: String,
    /// Paths that changed.
    pub changed: Vec<String>,
    /// Hand-off that stopped the run, when any.
    pub handoff: Option<SetupHandoff>,
    /// Verification checks.
    pub checks: Vec<SetupCheck>,
}

/// One verification check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupCheck {
    /// Check name.
    pub name: String,
    /// `ok`, `warn` or `fail`.
    pub status: String,
    /// Human-readable detail.
    pub detail: String,
}

/// How sudo behaves on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SudoMode {
    /// `sudo -n true` succeeds.
    Passwordless,
    /// `sudo` exists but asks for a password.
    Password,
    /// There is no `sudo` program.
    None,
}

impl SudoMode {
    /// Returns the value the probe prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passwordless => "passwordless",
            Self::Password => "password",
            Self::None => "none",
        }
    }
}

/// Whether a requested harness is installed and appears signed in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HarnessStatus {
    /// The adapter binary was found (`claude` for `claude-code`).
    pub installed: bool,
    /// A known credential file exists in the effective user's home.
    ///
    /// Unknown harnesses report `true` here: setup cannot inspect every vendor's login
    /// store, so the plan still requires `login:<harness>` but does not invent a check.
    pub login: bool,
}

/// The read-only facts `setup plan` and `setup home` collect from the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `uname -s`.
    pub os: String,
    /// `uname -m`.
    pub arch: String,
    /// Login user.
    pub user: String,
    /// Login user's `$HOME`.
    pub home: String,
    /// Short hostname, used for the default node name.
    pub hostname: String,
    /// Sudo behaviour.
    pub sudo: SudoMode,
    /// Whether `vowp` already exists and which version it reports.
    pub vowp_version: Option<String>,
    /// Absolute `vowp` path when the probe found one.
    pub vowp_path: Option<String>,
    /// Home of an existing `vowp` user, when it exists.
    pub vowp_user_home: Option<String>,
    /// Tools the phases need.
    pub tools: BTreeMap<String, bool>,
    /// Required harnesses: installed binary and detected login.
    pub harnesses: BTreeMap<String, HarnessStatus>,
    /// Free disk in KiB on the login home.
    pub disk_free_kb: Option<u64>,
}

impl Probe {
    /// Returns the slugified node name used when `--name` is absent.
    pub fn default_node_name(&self) -> String {
        let mut slug = String::new();
        for ch in self.hostname.chars() {
            if ch.is_ascii_alphanumeric() {
                slug.push(ch.to_ascii_lowercase());
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
            if slug.len() >= 63 {
                break;
            }
        }
        let slug = slug.trim_matches('-').to_owned();
        if slug.is_empty() {
            "home".to_owned()
        } else if NodeName::parse(&slug).is_ok() {
            slug
        } else {
            "home".to_owned()
        }
    }

    /// Returns the user setup will run as after the user phase.
    pub fn effective_user(&self) -> &str {
        if self.user == "vowp" || self.sudo == SudoMode::Passwordless {
            "vowp"
        } else {
            &self.user
        }
    }

    /// Returns the home directory setup will use after the user phase.
    pub fn effective_home(&self) -> String {
        if self.effective_user() == self.user {
            return self.home.clone();
        }
        self.vowp_user_home.clone().unwrap_or_else(|| "/home/vowp".to_owned())
    }

    /// Returns whether the target can run a VOWP home.
    pub fn is_supported(&self) -> bool {
        self.os == SUPPORTED_OS && SUPPORTED_ARCHES.contains(&self.arch.as_str())
    }

    /// Returns a check for an unsupported target.
    pub fn unsupported_message(&self) -> String {
        format!(
            "setup needs a Linux x86_64 or arm64 home; {} reports {} {}",
            self.user, self.os, self.arch
        )
    }
}

/// Wraps a remote argv so it runs as `effective`, with `VOWP_HOME` set.
///
/// A command that already runs as the login user is left alone. A sudo transition is an
/// argv prefix, never a shell string, so every argument keeps its exact value.
pub fn as_user(login_user: &str, effective: &str, home: &str, argv: Vec<String>) -> Vec<String> {
    if login_user == effective {
        return argv;
    }
    let mut wrapped = vec![
        "sudo".to_owned(),
        "-u".to_owned(),
        effective.to_owned(),
        "-H".to_owned(),
        "env".to_owned(),
        format!("VOWP_HOME={home}/.vowp"),
    ];
    wrapped.extend(argv);
    wrapped
}

/// Builds the plan from the probe and the owner's answers.
pub fn build_plan(answers: &SetupAnswers, probe: &Probe, approved: bool) -> Result<SetupPlan, CliError> {
    answers.validate()?;
    let node = match &answers.name {
        Some(name) => NodeName::parse(name)?.to_string(),
        None => probe.default_node_name(),
    };
    let effective_user = probe.effective_user().to_owned();
    let effective_home = probe.effective_home();
    let transit = |argv: Vec<String>| as_user(&probe.user, &effective_user, &effective_home, argv);
    let vowp_home = format!("{effective_home}/.vowp");
    // The unit always runs `%h/.local/bin/vowp`; a `vowp` elsewhere on PATH is not
    // enough. The install phase therefore always targets the per-user prefix.
    let vowp_bin = format!("{effective_home}/.local/bin/vowp");

    let mut phases = Vec::new();
    let mut handoffs = Vec::new();

    phases.push(SetupPhase {
        id: "preflight".to_owned(),
        title: "SSH key login, OS/arch, disk, git, tmux and curl".to_owned(),
        commands: vec![vec![
            "sh".to_owned(),
            "-c".to_owned(),
            commands::PROBE_SCRIPT.to_owned(),
        ]],
        files: Vec::new(),
        requires: {
            let mut requires = vec!["network".to_owned(), "ssh-key-login".to_owned()];
            requires.extend(answers.harnesses.iter().map(|harness| format!("login:{harness}")));
            requires.extend(answers.providers.iter().map(|provider| format!("provider:{provider}")));
            requires
        },
        idempotent: true,
    });
    let missing_tools: Vec<&str> = ["git", "tmux", "curl"]
        .into_iter()
        .filter(|tool| !probe.tools.get(*tool).copied().unwrap_or(false))
        .collect();
    if !missing_tools.is_empty() {
        handoffs.push(SetupHandoff {
            phase: "preflight".to_owned(),
            kind: "owner-answer".to_owned(),
            prompt: format!("Install {} on the home, then re-run setup.", missing_tools.join(", ")),
        });
    }
    if let Some(free) = probe.disk_free_kb
        && free < 5 * 1024 * 1024
    {
        handoffs.push(SetupHandoff {
            phase: "preflight".to_owned(),
            kind: "owner-answer".to_owned(),
            prompt: "Free at least 5 GiB on the home (or grow the disk), then re-run setup.".to_owned(),
        });
    }
    for harness in &answers.harnesses {
        let status = probe.harnesses.get(harness).copied().unwrap_or_default();
        if !status.installed {
            handoffs.push(SetupHandoff {
                phase: "preflight".to_owned(),
                kind: "login".to_owned(),
                prompt: format!(
                    "Install {harness} ({}) on the home with its own flow, then re-run setup.",
                    commands::harness_binary(harness)
                ),
            });
        } else if !status.login {
            handoffs.push(SetupHandoff {
                phase: "user".to_owned(),
                kind: "login".to_owned(),
                prompt: format!("Sign in to {harness} on the home with its own login flow, then re-run setup."),
            });
        }
    }

    let user_commands = if effective_user == probe.user {
        Vec::new()
    } else {
        // The user creation must run as the login user; the script itself uses sudo.
        vec![vec![
            "sh".to_owned(),
            "-c".to_owned(),
            commands::ENSURE_USER_SCRIPT.to_owned(),
        ]]
    };
    phases.push(SetupPhase {
        id: "user".to_owned(),
        title: if effective_user == probe.user {
            "Use the no-sudo mode as the login user".to_owned()
        } else {
            "Create the non-root `vowp` user and copy the SSH key".to_owned()
        },
        commands: user_commands,
        files: if effective_user == probe.user {
            Vec::new()
        } else {
            vec![
                "/etc/passwd (user vowp)".to_owned(),
                format!("{effective_home}/.ssh/authorized_keys"),
            ]
        },
        requires: if effective_user == probe.user || probe.user == "root" {
            Vec::new()
        } else {
            vec!["sudo".to_owned()]
        },
        idempotent: true,
    });

    phases.push(SetupPhase {
        id: "install".to_owned(),
        title: format!("Install VOWP {}", answers.version),
        commands: vec![
            transit(vec![vowp_bin.clone(), "--version".to_owned()]),
            transit(vec!["sh".to_owned(), "-c".to_owned(), install_script(&answers.version)]),
        ],
        files: vec![format!("{effective_home}/.local/bin/vowp")],
        requires: {
            let mut requires = vec!["network".to_owned()];
            requires.extend(answers.harnesses.iter().map(|harness| format!("login:{harness}")));
            requires
        },
        idempotent: true,
    });

    phases.push(SetupPhase {
        id: "home".to_owned(),
        title: "Create the home, the global database and the node registry".to_owned(),
        commands: {
            let mut command = vec![
                vowp_bin.clone(),
                "setup".to_owned(),
                "__home-init".to_owned(),
                "--name".to_owned(),
                node.clone(),
                "--root".to_owned(),
                vowp_home.clone(),
            ];
            for harness in &answers.harnesses {
                command.push("--harness".to_owned());
                command.push(harness.clone());
            }
            for provider in &answers.providers {
                command.push("--provider".to_owned());
                command.push(provider.clone());
            }
            vec![
                transit(command),
                transit(vec![
                    vowp_bin.clone(),
                    "--home".to_owned(),
                    vowp_home.clone(),
                    "doctor".to_owned(),
                    "--json".to_owned(),
                ]),
            ]
        },
        files: vec![
            vowp_home.clone(),
            format!("{vowp_home}/vowp.db"),
            format!("{vowp_home}/config.toml"),
            format!("{vowp_home}/vowp.toml"),
            format!("{vowp_home}/projects/_node/project.db"),
        ],
        requires: {
            let mut requires = Vec::new();
            requires.extend(answers.harnesses.iter().map(|harness| format!("login:{harness}")));
            requires.extend(answers.providers.iter().map(|provider| format!("provider:{provider}")));
            requires
        },
        idempotent: true,
    });

    let mut service_commands = vec![transit(vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "umask 077; mkdir -p \"$HOME/.config/systemd/user\" && cat > \"$HOME/.config/systemd/user/vowp.service\""
            .to_owned(),
    ])];
    service_commands.push(transit(commands::systemctl_argv("daemon-reload")));
    service_commands.push(transit(commands::systemctl_argv("is-enabled vowp.service")));
    service_commands.push(transit(commands::systemctl_argv("is-active vowp.service")));
    service_commands.push(transit(commands::systemctl_argv("enable --now vowp.service")));
    service_commands.push(if effective_user == probe.user {
        vec![
            "loginctl".to_owned(),
            "enable-linger".to_owned(),
            effective_user.clone(),
        ]
    } else {
        vec![
            "sudo".to_owned(),
            "loginctl".to_owned(),
            "enable-linger".to_owned(),
            effective_user.clone(),
        ]
    });
    phases.push(SetupPhase {
        id: "service".to_owned(),
        title: "Install the user systemd unit, enable linger and start it".to_owned(),
        commands: service_commands,
        files: vec![format!("{effective_home}/.config/systemd/user/vowp.service")],
        requires: if effective_user == probe.user {
            vec!["systemd".to_owned()]
        } else {
            vec!["sudo".to_owned(), "systemd".to_owned()]
        },
        idempotent: true,
    });

    let vault_commands = if probe.sudo == SudoMode::Passwordless {
        vec![transit(vec![
            vowp_bin.clone(),
            "--home".to_owned(),
            vowp_home.clone(),
            "secret".to_owned(),
            "upgrade".to_owned(),
        ])]
    } else {
        Vec::new()
    };
    phases.push(SetupPhase {
        id: "vault".to_owned(),
        title: if probe.sudo == SudoMode::Passwordless {
            "Create the vault user and helper".to_owned()
        } else {
            "Use basic secret mode (no sudo)".to_owned()
        },
        commands: vault_commands,
        files: if probe.sudo == SudoMode::Passwordless {
            vec![
                format!("{vowp_home}/secrets/mode"),
                "/home/vowp-vault".to_owned(),
                "/usr/local/lib/vowp/__vault-admin".to_owned(),
                "/etc/sudoers.d/vowp-vault".to_owned(),
            ]
        } else {
            // Basic mode has no marker file: an absent `secrets/mode` means basic, so the
            // only files this phase can touch are the secret values the owner saves.
            vec![format!("{vowp_home}/secrets/<NAME>")]
        },
        requires: {
            let mut requires = if probe.sudo == SudoMode::Passwordless {
                vec!["sudo".to_owned()]
            } else {
                Vec::new()
            };
            requires.extend(answers.secrets.iter().map(|secret| format!("secret:{secret}")));
            requires
        },
        idempotent: true,
    });
    for secret in &answers.secrets {
        handoffs.push(SetupHandoff {
            phase: "vault".to_owned(),
            kind: "secret".to_owned(),
            prompt: format!(
                "Save {secret} with `vowp secret set {secret}` on the home (hidden prompt) or `vowp secret request {secret}` through a tunnel, then re-run setup. Never paste the value into the chat."
            ),
        });
    }

    let mut project_commands = Vec::new();
    let mut project_files = Vec::new();
    for repository in &answers.repositories {
        let repo = repository.trim_end_matches('/');
        // Each path is one argv element; no shell sees it, so spaces and metacharacters
        // cannot change the command that runs on the home.
        project_commands.push(transit(vec!["test".to_owned(), "-d".to_owned(), repo.to_owned()]));
        project_commands.push(transit(vec![
            "test".to_owned(),
            "-f".to_owned(),
            format!("{repo}/vowp.toml"),
        ]));
        project_commands.push(transit(vec![
            vowp_bin.clone(),
            "--home".to_owned(),
            vowp_home.clone(),
            "project".to_owned(),
            "add".to_owned(),
            repo.to_owned(),
        ]));
        project_files.push(format!("{repo}/vowp.toml"));
        project_files.push(format!("{vowp_home}/projects/<name>/project.db"));
    }
    phases.push(SetupPhase {
        id: "projects".to_owned(),
        title: if answers.repositories.is_empty() {
            "No repositories requested".to_owned()
        } else {
            "Register the owner's repositories".to_owned()
        },
        commands: project_commands,
        files: project_files,
        requires: answers
            .repositories
            .iter()
            .map(|repository| format!("owner-answer:repository {repository} exists on the home with vowp.toml"))
            .collect(),
        idempotent: true,
    });

    phases.push(SetupPhase {
        id: "tunnels".to_owned(),
        title: "No tunnels by default".to_owned(),
        commands: Vec::new(),
        files: Vec::new(),
        requires: Vec::new(),
        idempotent: true,
    });

    if !approved {
        handoffs.insert(
            0,
            SetupHandoff {
                phase: "preflight".to_owned(),
                kind: "approval".to_owned(),
                prompt: "Show this plan to the owner; re-run with --yes after they approve it.".to_owned(),
            },
        );
    }
    if answers.github_remote {
        handoffs.push(SetupHandoff {
            phase: "projects".to_owned(),
            kind: "owner-answer".to_owned(),
            prompt:
                "Add a GitHub remote and push `main` for each repository after setup; VOWP never pushes on its own."
                    .to_owned(),
        });
    }

    Ok(SetupPlan {
        target: answers.target.clone(),
        version: answers.version.clone(),
        phases,
        handoffs,
    })
}

/// Builds the plan `setup control` prints.
pub fn control_plan(target: &str, name: &str, probe: &Probe, remote_home: &str) -> SetupPlan {
    let snapshot_path = format!("{}/snapshot.json", remote_home.trim_end_matches('/'));
    SetupPlan {
        target: target.to_owned(),
        version: probe.vowp_version.clone().unwrap_or_else(|| "unknown".to_owned()),
        phases: vec![SetupPhase {
            id: "control".to_owned(),
            title: "Write the control home, spool, node mirror, snapshot cache and tunnel table".to_owned(),
            commands: vec![
                vec!["sh".to_owned(), "-c".to_owned(), commands::PROBE_SCRIPT.to_owned()],
                as_user(
                    &probe.user,
                    probe.effective_user(),
                    &probe.effective_home(),
                    vec!["cat".to_owned(), snapshot_path],
                ),
            ],
            files: vec![
                "~/.vowp-control/control.json".to_owned(),
                "~/.vowp-control/spool/".to_owned(),
                "~/.vowp-control/nodes.json".to_owned(),
                "~/.vowp-control/surfaces.json".to_owned(),
                "~/.vowp-control/tunnels.json".to_owned(),
                "~/.vowp-control/setup.json".to_owned(),
                "~/.vowp-control/snapshot.json".to_owned(),
                "~/.vowp-control/snapshot.meta.json".to_owned(),
                format!("{remote_home} (on the home)"),
            ],
            requires: {
                let mut requires = vec!["network".to_owned(), "ssh-key-login".to_owned()];
                if probe.effective_user() != probe.user {
                    requires.push("sudo".to_owned());
                }
                requires
            },
            idempotent: true,
        }],
        handoffs: vec![SetupHandoff {
            phase: "control".to_owned(),
            kind: "approval".to_owned(),
            prompt: format!(
                "This writes ~/.vowp-control for home {name} ({target}) and installs no supervisor; run again with --apply after the owner's yes."
            ),
        }],
    }
}

/// The local record `setup home` leaves for `setup control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSetupState {
    /// SSH target of the home.
    pub target: String,
    /// Home node name.
    pub name: String,
    /// VOWP version the home run installed.
    pub version: String,
    /// Absolute `~/.vowp` path of the home the run actually used.
    #[serde(default)]
    pub remote_home: String,
    /// When the record was last written.
    pub updated_at: Timestamp,
}

/// A remote command's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code; `None` means the process was killed by a signal.
    pub status: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

impl CommandOutput {
    /// Returns whether the command exited 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// Returns a short detail line for a check or an error.
    pub fn detail(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        if text.is_empty() {
            format!("exit {:?}", self.status)
        } else {
            text.lines().next().unwrap_or_default().chars().take(300).collect()
        }
    }
}

/// Runs one remote argv for setup.
pub trait SetupRunner {
    /// Runs `argv` on the target, optionally feeding `stdin`.
    fn run(&mut self, argv: &[String], stdin: Option<&[u8]>) -> Result<CommandOutput, CliError>;
}

/// Renders the plan for the owner.
pub fn print_plan(io: &mut dyn IoStreams, plan: &SetupPlan) -> Result<(), CliError> {
    writeln!(io.out(), "setup plan for {} (vowp {})", plan.target, plan.version)?;
    for (index, phase) in plan.phases.iter().enumerate() {
        writeln!(io.out(), "\n{}. {} — {}", index + 1, phase.id, phase.title)?;
        writeln!(
            io.out(),
            "   requires: {}",
            if phase.requires.is_empty() {
                "none".to_owned()
            } else {
                phase.requires.join(", ")
            }
        )?;
        writeln!(
            io.out(),
            "   idempotent: {}",
            if phase.idempotent { "yes" } else { "no" }
        )?;
        if phase.files.is_empty() {
            writeln!(io.out(), "   files: none")?;
        } else {
            writeln!(io.out(), "   files: {}", phase.files.join(", "))?;
        }
        if phase.commands.is_empty() {
            writeln!(io.out(), "   commands: none")?;
        } else {
            writeln!(io.out(), "   commands:")?;
            for command in &phase.commands {
                writeln!(io.out(), "     - {}", render_argv(command))?;
            }
        }
    }
    if plan.handoffs.is_empty() {
        writeln!(io.out(), "\nhand-offs: none")?;
    } else {
        writeln!(io.out(), "\nhand-offs:")?;
        for handoff in &plan.handoffs {
            writeln!(io.out(), "  - {}/{}: {}", handoff.phase, handoff.kind, handoff.prompt)?;
        }
    }
    Ok(())
}

/// Renders the outcome for the owner.
pub fn print_outcome(io: &mut dyn IoStreams, outcome: &SetupOutcome) -> Result<(), CliError> {
    writeln!(io.out(), "setup: {} (phase {})", outcome.status, outcome.phase)?;
    if outcome.changed.is_empty() {
        writeln!(io.out(), "changed: nothing")?;
    } else {
        writeln!(io.out(), "changed:")?;
        for path in &outcome.changed {
            writeln!(io.out(), "  - {path}")?;
        }
    }
    if let Some(handoff) = &outcome.handoff {
        writeln!(
            io.out(),
            "handoff ({}/{}): {}",
            handoff.phase,
            handoff.kind,
            handoff.prompt
        )?;
    }
    if !outcome.checks.is_empty() {
        writeln!(io.out(), "checks:")?;
        for check in &outcome.checks {
            writeln!(io.out(), "  {:<4} {}: {}", check.status, check.name, check.detail)?;
        }
    }
    Ok(())
}

/// Renders one argv with the quoting the remote shell will see.
pub fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
            {
                arg.clone()
            } else {
                shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
