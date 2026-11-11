//! Execution of `vowp setup plan|home|control` and the hidden home bootstrap.
//!
//! The commands run on the control machine and reach the home only through `ssh` (or a
//! test runner). Every remote step is an argv vector; the plan prints the same vectors
//! before anything runs. Human hand-offs stop the run with exit code 5.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use serde::Serialize;
use vowp_protocol::{
    Node, NodeConfig, NodeFile, NodeName, OnCreditsExhausted, OnQuota, Profile, Project, ProjectName, ProviderConfig,
    SecretName, Snapshot, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, Store};
use vowp_tmux::quote::shell_quote;

use crate::node_ctx::NodeRole;
use crate::output;
use crate::remote;
use crate::{CliError, Ctx, IoStreams};

use super::{
    CommandOutput, HarnessStatus, LocalSetupState, Probe, SetupAnswers, SetupCheck, SetupHandoff, SetupOutcome,
    SetupRunner, SudoMode, as_user, build_plan, control_plan, install_script, print_outcome, print_plan,
};

/// The read-only probe script; `setup plan` runs only this.
pub const PROBE_SCRIPT: &str = r#"set -u
printf 'os=%s\n' "$(uname -s 2>/dev/null || printf unknown)"
printf 'arch=%s\n' "$(uname -m 2>/dev/null || printf unknown)"
printf 'user=%s\n' "$(id -un 2>/dev/null || printf unknown)"
printf 'home=%s\n' "${HOME:-}"
printf 'hostname=%s\n' "$(hostname -s 2>/dev/null || hostname 2>/dev/null || printf home)"
for tool in git tmux curl systemctl loginctl opencode claude; do
  if command -v "$tool" >/dev/null 2>&1; then printf 'tool.%s=yes\n' "$tool"; else printf 'tool.%s=no\n' "$tool"; fi
done
if command -v sudo >/dev/null 2>&1; then
  if sudo -n true >/dev/null 2>&1; then printf 'sudo=passwordless\n'; else printf 'sudo=password\n'; fi
else
  printf 'sudo=none\n'
fi
if command -v vowp >/dev/null 2>&1; then
  printf 'vowp.version=%s\n' "$(vowp --version 2>/dev/null | sed -n '1p')"
  printf 'vowp.path=%s\n' "$(command -v vowp)"
else
  printf 'vowp.version=\n'
  printf 'vowp.path=\n'
fi
if id -u vowp >/dev/null 2>&1; then
  home=$(getent passwd vowp 2>/dev/null | cut -d: -f6)
  [ -n "$home" ] || home=/home/vowp
  printf 'vowp.user_home=%s\n' "$home"
else
  printf 'vowp.user_home=\n'
fi
if df -Pk "${HOME:-/}" >/dev/null 2>&1; then
  printf 'disk.free_kb=%s\n' "$(df -Pk "${HOME:-/}" | awk 'NR==2 {print $4}')"
else
  printf 'disk.free_kb=\n'
fi
"#;

/// Creates the non-root user and copies the login user's authorized keys.
pub const ENSURE_USER_SCRIPT: &str = r#"set -eu
umask 022
SUDO=
if [ "$(id -u)" != 0 ]; then SUDO=sudo; fi
if ! id -u vowp >/dev/null 2>&1; then
  $SUDO useradd --create-home --shell /bin/bash vowp
fi
if [ -f "$HOME/.ssh/authorized_keys" ]; then
  $SUDO install -d -m 0700 -o vowp -g vowp /home/vowp/.ssh
  $SUDO install -m 0600 -o vowp -g vowp "$HOME/.ssh/authorized_keys" /home/vowp/.ssh/authorized_keys
fi
"#;

/// Returns a `systemctl --user` command with the per-user runtime directory set.
pub fn systemctl_argv(arguments: &str) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "XDG_RUNTIME_DIR=${{XDG_RUNTIME_DIR:-/run/user/$(id -u)}}; export XDG_RUNTIME_DIR; systemctl --user {arguments}"
        ),
    ]
}

/// The embedded unit is the release unit; a test keeps it identical to
/// `scripts/vowp.service`.
const SERVICE_UNIT: &str = include_str!("vowp.service");

/// An `ssh` runner for one target.
pub struct SshRunner {
    target: String,
}

impl SshRunner {
    /// Creates a runner for `target`.
    pub fn new(target: impl Into<String>) -> Self {
        Self { target: target.into() }
    }
}

impl SetupRunner for SshRunner {
    fn run(&mut self, argv: &[String], stdin: Option<&[u8]>) -> Result<CommandOutput, CliError> {
        let quoted: Vec<String> = argv.iter().map(|arg| shell_quote(arg)).collect();
        let remote = quoted.join(" ");
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", &self.target, &remote]);
        command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| CliError::Remote(format!("ssh {}: {error}", self.target)))?;
        if let Some(bytes) = stdin {
            let mut input = child
                .stdin
                .take()
                .ok_or_else(|| CliError::Remote("ssh stdin was not available".to_owned()))?;
            input.write_all(bytes)?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| CliError::Remote(format!("ssh {}: {error}", self.target)))?;
        if output.status.code() == Some(255) {
            return Err(CliError::Remote(format!(
                "ssh {} failed with exit 255: {}",
                self.target,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Runs the read-only probe, including the requested harness checks.
pub fn probe(runner: &mut dyn SetupRunner, harnesses: &[String]) -> Result<Probe, CliError> {
    let output = runner.run(&["sh".to_owned(), "-c".to_owned(), PROBE_SCRIPT.to_owned()], None)?;
    if !output.success() {
        return Err(CliError::Remote(format!("the setup probe failed: {}", output.detail())));
    }
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for line in output.stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        fields.insert(key.to_owned(), value.to_owned());
    }
    let field = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let os = field("os");
    let arch = field("arch");
    let user = field("user");
    let home = field("home");
    if os.is_empty() || arch.is_empty() || user.is_empty() || home.is_empty() {
        return Err(CliError::Remote(format!(
            "the setup probe returned an incomplete document: {}",
            output.stdout.trim()
        )));
    }
    let sudo = match field("sudo").as_str() {
        "passwordless" => SudoMode::Passwordless,
        "password" => SudoMode::Password,
        _ => SudoMode::None,
    };
    let mut tools = BTreeMap::new();
    for (key, value) in &fields {
        if let Some(tool) = key.strip_prefix("tool.") {
            tools.insert(tool.to_owned(), value == "yes");
        }
    }
    let vowp_version = {
        let raw = field("vowp.version");
        raw.strip_prefix("vowp ")
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    };
    let vowp_path = {
        let raw = field("vowp.path");
        (!raw.is_empty()).then_some(raw)
    };
    let vowp_user_home = {
        let raw = field("vowp.user_home");
        (!raw.is_empty()).then_some(raw)
    };
    let disk_free_kb = field("disk.free_kb").parse::<u64>().ok();
    let mut probe = Probe {
        os,
        arch,
        user,
        home,
        hostname: field("hostname"),
        sudo,
        vowp_version,
        vowp_path,
        vowp_user_home,
        tools,
        harnesses: BTreeMap::new(),
        disk_free_kb,
    };
    // The credential check must look in the home setup will use after the user phase,
    // not necessarily the SSH login user's home.
    let effective_home = probe.effective_home();
    for harness in harnesses {
        let binary = harness_binary(harness);
        let installed = match probe.tools.get(binary) {
            Some(found) => *found,
            None => runner
                .run(
                    &[
                        "sh".to_owned(),
                        "-c".to_owned(),
                        format!("command -v {} >/dev/null 2>&1", shell_quote(binary)),
                    ],
                    None,
                )?
                .success(),
        };
        let login = harness_login(runner, harness, &effective_home)?;
        probe
            .harnesses
            .insert(harness.clone(), HarnessStatus { installed, login });
    }
    Ok(probe)
}

/// Maps a documented harness name to the binary its adapter runs.
pub(crate) fn harness_binary(harness: &str) -> &str {
    match harness {
        "claude-code" => "claude",
        other => other,
    }
}

/// Returns the credential-file check for a known harness.
fn harness_login_script(harness: &str) -> Option<&'static str> {
    match harness {
        "opencode" => {
            Some("[ -s \"$HOME/.local/share/opencode/auth.json\" ] || [ -s \"$HOME/.config/opencode/auth.json\" ]")
        }
        "claude-code" => Some("[ -s \"$HOME/.claude/.credentials.json\" ] || [ -s \"$HOME/.claude.json\" ]"),
        _ => None,
    }
}

/// Returns whether `harness` looks signed in for `home`; unknown harnesses are not guessed.
fn harness_login(runner: &mut dyn SetupRunner, harness: &str, home: &str) -> Result<bool, CliError> {
    let Some(script) = harness_login_script(harness) else {
        return Ok(true);
    };
    let command = format!("HOME={}; export HOME; {script}", shell_quote(home));
    Ok(runner
        .run(&["sh".to_owned(), "-c".to_owned(), command], None)?
        .success())
}

/// Runs `vowp setup plan`.
pub fn run_plan(ctx: &Ctx, io: &mut dyn IoStreams, answers: &SetupAnswers) -> Result<i32, CliError> {
    let answers = answers.clone();
    answers.validate()?;
    let mut runner = SshRunner::new(answers.target.clone());
    let probe = probe(&mut runner, &answers.harnesses)?;
    if !probe.is_supported() {
        return Err(CliError::Guard(probe.unsupported_message()));
    }
    let plan = build_plan(&answers, &probe, false)?;
    if ctx.json() {
        output::print_json(io, &plan)?;
    } else {
        print_plan(io, &plan)?;
    }
    Ok(0)
}

/// Runs `vowp setup home`.
pub fn run_home(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    args: &SetupAnswers,
    apply: bool,
    yes: bool,
) -> Result<i32, CliError> {
    let answers = args.clone();
    answers.validate()?;
    // Missing approval is a local guard; do not open SSH or probe a target the owner has
    // not approved yet.
    if apply && !yes {
        let outcome = SetupOutcome {
            status: "handoff".to_owned(),
            phase: "preflight".to_owned(),
            changed: Vec::new(),
            handoff: Some(SetupHandoff {
                phase: "preflight".to_owned(),
                kind: "approval".to_owned(),
                prompt: "Show this plan to the owner; re-run with --yes after they approve it.".to_owned(),
            }),
            checks: Vec::new(),
        };
        print_outcome_document(ctx, io, &outcome)?;
        return Ok(5);
    }
    let mut runner = SshRunner::new(answers.target.clone());
    let probe = probe(&mut runner, &answers.harnesses)?;
    if !probe.is_supported() {
        return Err(CliError::Guard(probe.unsupported_message()));
    }
    let plan = build_plan(&answers, &probe, yes)?;
    if !apply {
        if ctx.json() {
            output::print_json(io, &plan)?;
        } else {
            print_plan(io, &plan)?;
        }
        return Ok(0);
    }

    let mut executor = Executor::new(answers.clone(), probe.clone());
    let mut outcome = executor.run()?;
    let state_home = format!("{}/.vowp", executor.remote_home.trim_end_matches('/'));
    if write_setup_state(&answers, &probe, &state_home)? {
        outcome.changed.push("~/.vowp-control/setup.json".to_owned());
    }
    print_outcome_document(ctx, io, &outcome)?;
    Ok(if outcome.status == "handoff" { 5 } else { 0 })
}

/// Runs `vowp setup control`.
pub fn run_control(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    args: &super::super::cmd::setup::SetupControlArgs,
) -> Result<i32, CliError> {
    if ctx.role() == NodeRole::Home {
        return Err(CliError::Guard(
            "this is a home; run `vowp setup control` on the machine that talks to it".to_owned(),
        ));
    }
    let root = remote::control_root()?;
    let existing = remote::read_control_config(&root)?;
    let state = read_setup_state(&root)?;
    let target = args
        .target
        .clone()
        .or_else(|| existing.as_ref().map(|config| config.ssh_target.clone()))
        .or_else(|| state.as_ref().map(|state| state.target.clone()))
        .ok_or_else(|| {
            CliError::Guard(
                "no home target; run `vowp setup home --target <ssh-target> --apply --yes` first or pass --target"
                    .to_owned(),
            )
        })?;
    super::validate_ssh_target(&target)?;
    let name = args
        .name
        .clone()
        .or_else(|| existing.as_ref().map(|config| config.home.clone()))
        .or_else(|| state.as_ref().map(|state| state.name.clone()))
        .unwrap_or_else(|| "home".to_owned());
    let name = NodeName::parse(&name)?.to_string();

    let mut runner = SshRunner::new(target.clone());
    let mut probe = probe(&mut runner, &[])?;
    // `setup home` may have created the home under the vowp user, not the SSH login
    // user. A recorded state or an existing control config is authoritative; the probe
    // fallback is only for the first control run without a home record, and it uses the
    // effective home (the vowp user after passwordless sudo) rather than the login home.
    let remote_home = state
        .as_ref()
        .filter(|state| state.target == target && !state.remote_home.is_empty())
        .map(|state| state.remote_home.clone())
        .or_else(|| {
            existing
                .as_ref()
                .filter(|config| config.ssh_target == target && !config.remote_home.is_empty())
                .map(|config| config.remote_home.clone())
        })
        .unwrap_or_else(|| format!("{}/.vowp", probe.effective_home().trim_end_matches('/')));
    // The per-user binary is authoritative; a non-login SSH PATH often omits
    // `~/.local/bin`, which must not make an installed home look uninstalled.
    let trimmed_home = remote_home.trim_end_matches('/');
    let home_dir = trimmed_home.strip_suffix("/.vowp").unwrap_or(trimmed_home);
    let installed_binary = format!("{home_dir}/.local/bin/vowp");
    let version_argv = as_user(
        &probe.user,
        probe.effective_user(),
        &probe.effective_home(),
        vec![installed_binary.clone(), "--version".to_owned()],
    );
    let version_output = runner.run(&version_argv, None)?;
    if version_output.success()
        && let Some(version) = version_output
            .stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .and_then(|line| line.trim().strip_prefix("vowp "))
    {
        probe.vowp_version = Some(version.to_owned());
        probe.vowp_path = Some(installed_binary);
    }
    let plan = control_plan(&target, &name, &probe, &remote_home);
    if !args.apply {
        if ctx.json() {
            output::print_json(io, &plan)?;
        } else {
            print_plan(io, &plan)?;
        }
        return Ok(0);
    }
    if let Some(existing) = &existing
        && existing.ssh_target != target
        && !args.force
    {
        return Err(CliError::Conflict(format!(
            "this control machine already points at {}; pass --force to retarget it to {target}",
            existing.ssh_target
        )));
    }

    let mut changed = Vec::new();
    let mut checks = Vec::new();
    remote::ensure_private_dir(&root)?;
    remote::ensure_private_dir(&root.join("spool"))?;
    checks.push(SetupCheck {
        name: "control.spool".to_owned(),
        status: "ok".to_owned(),
        detail: root.join("spool").display().to_string(),
    });

    let created_at = existing
        .as_ref()
        .filter(|config| config.ssh_target == target && config.home == name)
        .map(|config| config.created_at)
        .unwrap_or_else(Timestamp::now);
    let config = crate::node_ctx::ControlConfig {
        role: "control".to_owned(),
        home: name.clone(),
        ssh_target: target.clone(),
        remote_home: remote_home.clone(),
        created_at,
    };
    let config_bytes = serde_json::to_vec_pretty(&config).map_err(vowp_protocol::ProtocolError::from)?;
    if write_if_changed(&root.join("control.json"), &config_bytes)? {
        changed.push(root.join("control.json").display().to_string());
    }
    checks.push(SetupCheck {
        name: "control.config".to_owned(),
        status: "ok".to_owned(),
        detail: format!("home {} ({})", config.home, config.ssh_target),
    });

    for (path, bytes) in [
        (root.join("surfaces.json"), b"[]\n".as_slice()),
        (root.join("tunnels.json"), b"[]\n".as_slice()),
    ] {
        if !path.is_file() {
            remote::write_private(&path, bytes)?;
            changed.push(path.display().to_string());
        }
    }

    let home_changed = existing
        .as_ref()
        .is_some_and(|old| old.home != config.home || old.ssh_target != config.ssh_target);
    let mut nodes = if home_changed {
        Vec::new()
    } else {
        remote::read_node_registry(&root)?
    };
    let previous = nodes.iter().find(|node| node.name.as_str() == name).cloned();
    let now = Timestamp::now();
    let node = Node {
        name: NodeName::parse(&name)?,
        ssh_target: Some(target.clone()),
        is_local: false,
        capabilities: serde_json::json!({
            "vowp": probe.vowp_version.as_ref().map(|version| format!("vowp {version}")),
            "probe_ok": probe.vowp_version.is_some(),
            "probe": probe.os.clone() + "/" + &probe.arch,
        }),
        last_seen_at: previous
            .as_ref()
            .and_then(|node| node.last_seen_at)
            .or_else(|| probe.vowp_version.as_ref().map(|_| now)),
        created_at: previous.as_ref().map(|node| node.created_at).unwrap_or(now),
    };
    nodes.retain(|existing| existing.name != node.name);
    nodes.push(node);
    nodes.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    let nodes_bytes = serde_json::to_vec_pretty(&nodes).map_err(vowp_protocol::ProtocolError::from)?;
    if write_if_changed(&root.join("nodes.json"), &nodes_bytes)? {
        changed.push(root.join("nodes.json").display().to_string());
    }
    checks.push(SetupCheck {
        name: "control.node".to_owned(),
        status: "ok".to_owned(),
        detail: format!("{} ({})", name, target),
    });

    if !state.as_ref().is_some_and(|state| {
        state.target == target && state.name == config.home && state.remote_home == config.remote_home
    }) {
        let state = LocalSetupState {
            target: target.clone(),
            name: config.home.clone(),
            version: probe.vowp_version.clone().unwrap_or_else(|| "unknown".to_owned()),
            remote_home: config.remote_home.clone(),
            updated_at: Timestamp::now(),
        };
        let bytes = serde_json::to_vec_pretty(&state).map_err(vowp_protocol::ProtocolError::from)?;
        remote::write_private(&root.join("setup.json"), &bytes)?;
        changed.push(root.join("setup.json").display().to_string());
    }
    checks.push(SetupCheck {
        name: "control.supervisor".to_owned(),
        status: "ok".to_owned(),
        detail: "no supervisor is installed on a control machine".to_owned(),
    });
    checks.push(update_control_snapshot(
        &mut runner,
        &probe,
        &root,
        &remote_home,
        &mut changed,
    )?);
    checks.push(SetupCheck {
        name: "control.home".to_owned(),
        status: if probe.vowp_version.is_some() { "ok" } else { "warn" }.to_owned(),
        detail: if probe.vowp_version.is_some() {
            format!(
                "{} reports vowp {} at {}",
                target,
                probe.vowp_version.as_deref().unwrap_or("unknown"),
                probe.vowp_path.as_deref().unwrap_or(&probe.home)
            )
        } else {
            format!("{target} does not report a vowp version yet; finish `setup home` first")
        },
    });

    let outcome = SetupOutcome {
        status: "ok".to_owned(),
        phase: "control".to_owned(),
        changed,
        handoff: None,
        checks,
    };
    print_outcome_document(ctx, io, &outcome)?;
    Ok(0)
}

/// Runs the hidden `vowp setup __home-init` on the home.
pub fn run_home_init(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    args: &super::super::cmd::setup::SetupHomeInitArgs,
) -> Result<i32, CliError> {
    let paths = match &args.root {
        Some(root) => NodePaths::new(root),
        None => ctx.paths().clone(),
    };
    let paths = &paths;
    let before_db = paths.db_path().is_file();
    paths.ensure()?;
    let store = Store::open_global(paths)?;
    let now = store.clock().now();
    let node_name = match &args.name {
        Some(name) => NodeName::parse(name)?,
        None => ctx.config().node.name.clone(),
    };

    let node_before = store.node(&node_name)?.is_some();
    if !node_before {
        store.upsert_node(&Node {
            name: node_name.clone(),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({
                "vowp": env!("CARGO_PKG_VERSION"),
                "role": "home",
                "setup": true,
            }),
            last_seen_at: Some(now),
            created_at: now,
        })?;
    }
    ensure_home_config(paths, &node_name, &args.harnesses, &args.providers)?;

    let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    let project_before = store.project(&node_project)?.is_some();
    if !project_before {
        store.upsert_project(&Project {
            name: node_project.clone(),
            path: paths.root().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        })?;
    }
    Store::create_project_db(paths, &node_project)?;

    let report = HomeInitReport {
        home: paths.root().display().to_string(),
        node: node_name.to_string(),
        db_created: !before_db,
        node_registered: !node_before,
        node_project_created: !project_before,
    };
    if ctx.json() {
        output::print_json(io, &report)?;
    } else {
        writeln!(
            io.out(),
            "home {} ready (node {}, database {})",
            report.home,
            report.node,
            if report.db_created { "created" } else { "present" }
        )?;
    }
    Ok(0)
}

/// The hidden home bootstrap report.
#[derive(Debug, Serialize)]
struct HomeInitReport {
    /// Home directory.
    home: String,
    /// Local node name.
    node: String,
    /// Whether the global database was created.
    db_created: bool,
    /// Whether the node row was created.
    node_registered: bool,
    /// Whether the reserved `_node` project row was created.
    node_project_created: bool,
}

/// Writes the home's node defaults and the reserved `_node` project config when absent.
///
/// `~/.vowp/config.toml` is the node file every command merges; `~/.vowp/vowp.toml` is the
/// config of the reserved `_node` project, which is how home-level commands such as
/// `vowp secret upgrade` resolve a project. Both carry the owner's harness/provider answer;
/// neither is overwritten once the owner has one.
fn ensure_home_config(
    paths: &NodePaths,
    node: &NodeName,
    harnesses: &[String],
    providers: &[String],
) -> Result<(), CliError> {
    let mut config = home_config(harnesses, providers)?;
    config.node.name = node.clone();
    config.validate()?;
    std::fs::create_dir_all(paths.root())?;

    let project_path = paths.root().join("vowp.toml");
    if !project_path.is_file() {
        let mut project = config.clone();
        project.project.path = Some(paths.root().display().to_string());
        let text = toml::to_string_pretty(&project)
            .map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;
        write_absent(&project_path, ".vowp.toml.setup.tmp", &text)?;
    }

    let node_path = paths.root().join("config.toml");
    if !node_path.is_file() {
        let node_file = NodeFile {
            node: NodeConfig {
                name: node.clone(),
                ..config.node
            },
            profiles: config.profiles,
            providers: config.providers,
            harnesses: config.harnesses,
            capsule: config.capsule,
        };
        let text = toml::to_string_pretty(&node_file)
            .map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;
        write_absent(&node_path, ".config.toml.setup.tmp", &text)?;
    }
    Ok(())
}

/// Writes `text` beside `path` and renames it into place; the caller checked absence.
fn write_absent(path: &std::path::Path, temp_name: &str, text: &str) -> Result<(), CliError> {
    let temp = path
        .parent()
        .ok_or_else(|| CliError::Invalid(format!("{} has no parent directory", path.display())))?
        .join(temp_name);
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Builds the home config from the owner's harness/provider answers.
fn home_config(harnesses: &[String], providers: &[String]) -> Result<VowpConfig, CliError> {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    config.project.path = None;
    config.project.push = false;
    if !harnesses.is_empty() {
        let mut ladder = Vec::new();
        for (index, harness) in harnesses.iter().enumerate() {
            let provider = providers.get(index).or_else(|| providers.first()).map(String::as_str);
            ladder.push(default_executor(harness, provider)?);
        }
        for role in ["planner", "keeper", "worker", "verifier"] {
            config.profiles.insert(
                role.to_owned(),
                Profile {
                    ladder: ladder.clone(),
                    on_quota: OnQuota::NextThenProbeBack,
                    node: None,
                },
            );
        }
    }
    for provider in providers {
        config.providers.entry(provider.clone()).or_insert(ProviderConfig {
            max_concurrent: 4,
            on_credits_exhausted: OnCreditsExhausted::Park,
            prices: BTreeMap::new(),
        });
    }
    Ok(config)
}

/// Returns the default executor for one harness/provider answer.
fn default_executor(harness: &str, provider: Option<&str>) -> Result<vowp_protocol::Executor, CliError> {
    let text = match (harness, provider) {
        ("opencode", None | Some("deepseek")) => "opencode:deepseek/deepseek-flash#max".to_owned(),
        ("opencode", Some(provider)) => format!("opencode:{provider}/default"),
        ("claude-code", None | Some("anthropic")) => "claude-code:claude-sonnet-5-5@medium".to_owned(),
        ("claude-code", Some(provider)) => format!("claude-code:{provider}/default"),
        (harness, _) => format!("{harness}:default"),
    };
    Ok(vowp_protocol::Executor::parse(&text)?)
}

/// Executes the home phases and collects checks and hand-offs.
struct Executor {
    runner: SshRunner,
    answers: SetupAnswers,
    probe: Probe,
    login_user: String,
    effective_user: String,
    remote_home: String,
    node_name: String,
    vowp_bin: String,
    phase: String,
    changed: Vec<String>,
    checks: Vec<SetupCheck>,
}

impl Executor {
    /// Builds the executor after the probe and the local state write.
    fn new(answers: SetupAnswers, probe: Probe) -> Self {
        let login_user = probe.user.clone();
        // Start as the login user. `user_phase` switches to vowp after the account exists.
        let effective_user = login_user.clone();
        let remote_home = probe.home.clone();
        let node_name = match &answers.name {
            Some(name) => name.clone(),
            None => probe.default_node_name(),
        };
        let vowp_bin = format!("{remote_home}/.local/bin/vowp");
        let changed = Vec::new();
        Self {
            runner: SshRunner::new(answers.target.clone()),
            answers,
            probe,
            login_user,
            effective_user,
            remote_home,
            node_name,
            vowp_bin,
            phase: "preflight".to_owned(),
            changed,
            checks: Vec::new(),
        }
    }

    /// Runs every phase in order.
    fn run(&mut self) -> Result<SetupOutcome, CliError> {
        if let Some(handoff) = self.preflight()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.user_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.harness_login_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.install_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.home_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.service_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.vault_phase()? {
            return Ok(self.handoff(handoff));
        }
        if let Some(handoff) = self.projects_phase()? {
            return Ok(self.handoff(handoff));
        }
        self.phase = "tunnels".to_owned();
        self.check("tunnels", "ok", "no tunnels requested");
        Ok(SetupOutcome {
            status: "ok".to_owned(),
            phase: self.phase.clone(),
            changed: self.changed.clone(),
            handoff: None,
            checks: self.checks.clone(),
        })
    }

    /// Builds a hand-off outcome from the work done so far.
    fn handoff(&self, handoff: SetupHandoff) -> SetupOutcome {
        SetupOutcome {
            status: "handoff".to_owned(),
            phase: handoff.phase.clone(),
            changed: self.changed.clone(),
            handoff: Some(handoff),
            checks: self.checks.clone(),
        }
    }

    /// Adds one check.
    fn check(&mut self, name: &str, status: &str, detail: impl Into<String>) {
        self.checks.push(SetupCheck {
            name: name.to_owned(),
            status: status.to_owned(),
            detail: detail.into(),
        });
    }

    /// Runs one remote command as the effective user.
    fn remote(&mut self, argv: Vec<String>, stdin: Option<&[u8]>) -> Result<CommandOutput, CliError> {
        let argv = as_user(&self.login_user, &self.effective_user, &self.remote_home, argv);
        self.runner.run(&argv, stdin)
    }

    /// Returns a `Remote` error carrying the command's detail.
    fn failed(&self, what: &str, output: &CommandOutput) -> CliError {
        CliError::Remote(format!("{what} failed: {}", output.detail()))
    }

    /// Reads the installed version.
    fn installed_version(&mut self) -> Result<Option<String>, CliError> {
        let output = self.remote(vec![self.vowp_bin.clone(), "--version".to_owned()], None)?;
        if !output.success() {
            return Ok(None);
        }
        Ok(output
            .stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(str::trim)
            .and_then(|line| line.strip_prefix("vowp "))
            .map(str::to_owned))
    }

    /// Phase 1: read-only facts and hand-offs.
    fn preflight(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "preflight".to_owned();
        self.check(
            "preflight.ssh",
            "ok",
            format!(
                "{} {} as {} (home {})",
                self.probe.os, self.probe.arch, self.login_user, self.probe.home
            ),
        );
        if !self.probe.is_supported() {
            return Err(CliError::Guard(self.probe.unsupported_message()));
        }
        let missing: Vec<&str> = ["git", "tmux", "curl"]
            .into_iter()
            .filter(|tool| !self.probe.tools.get(*tool).copied().unwrap_or(false))
            .collect();
        if !missing.is_empty() {
            self.check(
                "preflight.tools",
                "fail",
                format!("missing on the home: {}", missing.join(", ")),
            );
            return Ok(Some(SetupHandoff {
                phase: "preflight".to_owned(),
                kind: "owner-answer".to_owned(),
                prompt: format!("Install {} on the home, then re-run setup.", missing.join(", ")),
            }));
        }
        self.check("preflight.tools", "ok", "git, tmux and curl are present");
        match self.probe.disk_free_kb {
            Some(free) if free >= 5 * 1024 * 1024 => self.check(
                "preflight.disk",
                "ok",
                format!("{:.1} GiB free on the home", free as f64 / 1024.0 / 1024.0),
            ),
            Some(free) => {
                self.check(
                    "preflight.disk",
                    "fail",
                    format!(
                        "only {:.1} GiB free; VOWP needs at least 5 GiB",
                        free as f64 / 1024.0 / 1024.0
                    ),
                );
                return Ok(Some(SetupHandoff {
                    phase: "preflight".to_owned(),
                    kind: "owner-answer".to_owned(),
                    prompt: "Free at least 5 GiB on the home (or grow the disk), then re-run setup.".to_owned(),
                }));
            }
            None => self.check("preflight.disk", "warn", "could not read free disk space"),
        }
        let missing_harnesses: Vec<&String> = self
            .answers
            .harnesses
            .iter()
            .filter(|harness| {
                !self
                    .probe
                    .harnesses
                    .get(*harness)
                    .copied()
                    .unwrap_or_default()
                    .installed
            })
            .collect();
        if !missing_harnesses.is_empty() {
            let names = missing_harnesses
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.check("preflight.harnesses", "fail", format!("not found: {names}"));
            return Ok(Some(SetupHandoff {
                phase: "preflight".to_owned(),
                kind: "login".to_owned(),
                prompt: format!(
                    "Install and sign in to {names} on the home with its own login flow, then re-run setup."
                ),
            }));
        }
        Ok(None)
    }

    /// Phase 2: create or choose the non-root user.
    fn user_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "user".to_owned();
        if self.login_user == "vowp" {
            self.check("user.mode", "ok", "working as the dedicated vowp user");
            return Ok(None);
        }
        if self.probe.sudo != SudoMode::Passwordless {
            self.effective_user = self.login_user.clone();
            self.remote_home = self.probe.home.clone();
            let mode = if self.probe.sudo == SudoMode::Password {
                "sudo asks for a password"
            } else {
                "sudo is not installed"
            };
            self.check(
                "user.mode",
                "ok",
                format!(
                    "no-sudo mode: running as {} (sudo {}; {mode})",
                    self.login_user,
                    self.probe.sudo.as_str()
                ),
            );
            return Ok(None);
        }

        let existed = self
            .remote(
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "id -u vowp >/dev/null 2>&1".to_owned(),
                ],
                None,
            )?
            .success();
        let output = self.remote(
            vec!["sh".to_owned(), "-c".to_owned(), ENSURE_USER_SCRIPT.to_owned()],
            None,
        )?;
        if !output.success() {
            return Err(self.failed("creating the vowp user", &output));
        }
        let home_output = self.runner.run(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "getent passwd vowp 2>/dev/null | cut -d: -f6".to_owned(),
            ],
            None,
        )?;
        let home = if home_output.success() {
            let value = home_output.stdout.trim();
            if value.starts_with('/') {
                value.to_owned()
            } else {
                "/home/vowp".to_owned()
            }
        } else {
            "/home/vowp".to_owned()
        };
        self.effective_user = "vowp".to_owned();
        self.remote_home = home;
        self.vowp_bin = format!("{}/.local/bin/vowp", self.remote_home);
        if existed {
            self.check("user.mode", "ok", "the vowp user already exists");
        } else {
            self.changed.push("user:vowp".to_owned());
            self.check("user.mode", "ok", "created the vowp user and copied the SSH key");
        }
        Ok(None)
    }

    /// Checks the chosen harnesses' sign-in state as the effective user.
    fn harness_login_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "user".to_owned();
        let harnesses = self.answers.harnesses.clone();
        for harness in &harnesses {
            let Some(script) = harness_login_script(harness) else {
                continue;
            };
            let output = self.remote(vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()], None)?;
            if !output.success() {
                self.check(
                    "user.harness_login",
                    "fail",
                    format!("{harness} is installed but not signed in for {}", self.effective_user),
                );
                return Ok(Some(SetupHandoff {
                    phase: "user".to_owned(),
                    kind: "login".to_owned(),
                    prompt: format!(
                        "Sign in to {harness} on the home as {} with its own login flow, then re-run setup.",
                        self.effective_user
                    ),
                }));
            }
        }
        if !harnesses.is_empty() {
            self.check("user.harness_login", "ok", "the chosen harnesses are signed in");
        }
        Ok(None)
    }

    /// Phase 3: install the pinned binary and verify its version.
    fn install_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "install".to_owned();
        if self.installed_version()?.as_deref() == Some(self.answers.version.as_str()) {
            self.check(
                "install.version",
                "ok",
                format!("vowp {} is already installed", self.answers.version),
            );
            return Ok(None);
        }
        let script = install_script(&self.answers.version);
        // The SSH login user's cwd may be a home the target user cannot
        // traverse, and released installers may `cd .` before they start.
        // Start from `/`, which every user can search; every setup path is
        // absolute.
        let script = format!("cd / && {script}");
        let output = self.remote(vec!["sh".to_owned(), "-c".to_owned(), script], None)?;
        if !output.success() {
            return Err(self.failed("installing vowp", &output));
        }
        let version = self.installed_version()?;
        if version.as_deref() != Some(self.answers.version.as_str()) {
            return Err(CliError::Remote(format!(
                "installed vowp reports {:?}, expected {}",
                version, self.answers.version
            )));
        }
        self.changed.push(format!("{}/.local/bin/vowp", self.remote_home));
        self.check(
            "install.version",
            "ok",
            format!("installed vowp {}", self.answers.version),
        );
        Ok(None)
    }

    /// Phase 4: the home directory, global database and node registry.
    fn home_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "home".to_owned();
        let vowp_home = format!("{}/.vowp", self.remote_home);
        let db = format!("{vowp_home}/vowp.db");
        let config_path = format!("{vowp_home}/config.toml");
        let project_config_path = format!("{vowp_home}/vowp.toml");
        let node_db = format!("{vowp_home}/projects/_node/project.db");
        let existed = self
            .remote(
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("test -f {}", shell_quote(&db)),
                ],
                None,
            )?
            .success();
        let config_existed = self
            .remote(vec!["test".to_owned(), "-f".to_owned(), config_path.clone()], None)?
            .success();
        let project_config_existed = self
            .remote(
                vec!["test".to_owned(), "-f".to_owned(), project_config_path.clone()],
                None,
            )?
            .success();
        let node_db_existed = self
            .remote(vec!["test".to_owned(), "-f".to_owned(), node_db.clone()], None)?
            .success();
        let mut init = vec![
            self.vowp_bin.clone(),
            "setup".to_owned(),
            "__home-init".to_owned(),
            "--name".to_owned(),
            self.node_name.clone(),
            "--root".to_owned(),
            vowp_home.clone(),
        ];
        for harness in &self.answers.harnesses {
            init.push("--harness".to_owned());
            init.push(harness.clone());
        }
        for provider in &self.answers.providers {
            init.push("--provider".to_owned());
            init.push(provider.clone());
        }
        let output = self.remote(init, None)?;
        if !output.success() {
            return Err(self.failed("creating the home", &output));
        }
        let present = self
            .remote(
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("test -f {}", shell_quote(&db)),
                ],
                None,
            )?
            .success();
        if !present {
            return Err(CliError::Remote(format!("the home database {db} was not created")));
        }
        if !existed {
            self.changed.push(db);
            self.changed.push(vowp_home.clone());
            self.check("home.database", "ok", "created the global database");
        } else {
            self.check("home.database", "ok", "the global database is present");
        }
        for (path, existed) in [
            (config_path, config_existed),
            (project_config_path, project_config_existed),
            (node_db, node_db_existed),
        ] {
            if !existed {
                self.changed.push(path);
            }
        }

        let doctor = self.remote(
            vec![
                self.vowp_bin.clone(),
                "--home".to_owned(),
                vowp_home,
                "doctor".to_owned(),
                "--json".to_owned(),
            ],
            None,
        )?;
        if doctor.success() {
            self.check("home.doctor", "ok", "vowp doctor reports no failures");
        } else {
            self.check(
                "home.doctor",
                "warn",
                format!("vowp doctor has warnings or failures: {}", doctor_detail(&doctor)),
            );
        }
        Ok(None)
    }

    /// Phase 5: the user systemd unit, linger and start.
    fn service_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "service".to_owned();
        let unit_path = format!("{}/.config/systemd/user/vowp.service", self.remote_home);
        let existing = self.remote(vec!["cat".to_owned(), unit_path.clone()], None)?;
        if !existing.success() || existing.stdout != SERVICE_UNIT {
            let output = self.remote(
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "umask 077; mkdir -p \"$HOME/.config/systemd/user\" && cat > \"$HOME/.config/systemd/user/vowp.service\""
                        .to_owned(),
                ],
                Some(SERVICE_UNIT.as_bytes()),
            )?;
            if !output.success() {
                return Err(self.failed("installing the systemd unit", &output));
            }
            self.changed.push(unit_path);
            self.check("service.unit", "ok", "installed the user systemd unit");
        } else {
            self.check("service.unit", "ok", "the user systemd unit is current");
        }

        if !self.probe.tools.get("systemctl").copied().unwrap_or(false) {
            self.check(
                "service.enabled",
                "warn",
                "systemd is not available; start the home with `vowp up --foreground`",
            );
        } else {
            let reload = self.remote(systemctl_argv("daemon-reload"), None)?;
            if !reload.success() {
                self.check(
                    "service.enabled",
                    "warn",
                    format!("systemctl --user daemon-reload failed: {}", reload.detail()),
                );
            } else {
                let enabled = self.remote(systemctl_argv("is-enabled vowp.service"), None)?;
                let active = self.remote(systemctl_argv("is-active vowp.service"), None)?;
                if !enabled.success() || !active.success() {
                    // `is-enabled` is not enough: after `vowp down` the unit stays enabled
                    // but inactive, so a rerun must start it again.
                    let started = self.remote(systemctl_argv("enable --now vowp.service"), None)?;
                    if started.success() {
                        self.changed.push("systemd:vowp.service".to_owned());
                        self.check("service.enabled", "ok", "enabled and started vowp.service");
                    } else {
                        self.check(
                            "service.enabled",
                            "warn",
                            format!("could not enable vowp.service: {}", started.detail()),
                        );
                    }
                } else {
                    self.check("service.enabled", "ok", "vowp.service is enabled and active");
                }
            }
        }

        if self.probe.tools.get("loginctl").copied().unwrap_or(false) {
            let argv = if self.effective_user == self.login_user {
                vec![
                    "loginctl".to_owned(),
                    "enable-linger".to_owned(),
                    self.effective_user.clone(),
                ]
            } else {
                vec![
                    "sudo".to_owned(),
                    "loginctl".to_owned(),
                    "enable-linger".to_owned(),
                    self.effective_user.clone(),
                ]
            };
            let linger = self.runner.run(&argv, None)?;
            if linger.success() {
                self.check("service.linger", "ok", "linger is enabled");
            } else {
                self.check(
                    "service.linger",
                    "warn",
                    format!("loginctl enable-linger failed: {}", linger.detail()),
                );
            }
        }
        Ok(None)
    }

    /// Phase 6: vault user or basic mode, then the secret-name hand-offs.
    fn vault_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "vault".to_owned();
        let vowp_home = format!("{}/.vowp", self.remote_home);
        let mode_path = format!("{vowp_home}/secrets/mode");
        let mode = self.remote(vec!["cat".to_owned(), mode_path], None)?;
        let vault_ready = mode.success() && mode.stdout.trim() == "vault";
        if !vault_ready && self.probe.sudo == SudoMode::Passwordless {
            let output = self.remote(
                vec![
                    self.vowp_bin.clone(),
                    "--home".to_owned(),
                    vowp_home.clone(),
                    "secret".to_owned(),
                    "upgrade".to_owned(),
                ],
                None,
            )?;
            if output.status == Some(5) {
                // `vowp secret upgrade` prints the one sudo command it needs. With
                // passwordless sudo the approved plan can run that exact command itself;
                // otherwise the owner gets it as the documented hand-off.
                let command = output
                    .stderr
                    .split_once('\n')
                    .map(|(_, command)| command.trim())
                    .filter(|command| !command.is_empty());
                let installed = match command {
                    Some(command) => {
                        let ran = self
                            .runner
                            .run(&["sh".to_owned(), "-c".to_owned(), command.to_owned()], None)?;
                        if !ran.success() {
                            return Err(self.failed("running the vault setup command", &ran));
                        }
                        let retried = self.remote(
                            vec![
                                self.vowp_bin.clone(),
                                "--home".to_owned(),
                                vowp_home.clone(),
                                "secret".to_owned(),
                                "upgrade".to_owned(),
                            ],
                            None,
                        )?;
                        retried.success()
                    }
                    None => false,
                };
                if !installed {
                    let prompt = if output.stderr.trim().is_empty() {
                        "The vault needs one sudo command on the home; run it there, then re-run setup.".to_owned()
                    } else {
                        output.stderr.trim().to_owned()
                    };
                    return Ok(Some(SetupHandoff {
                        phase: "vault".to_owned(),
                        kind: "sudo".to_owned(),
                        prompt,
                    }));
                }
            } else if !output.success() {
                return Err(self.failed("setting up the vault", &output));
            }
            let mode = self.remote(vec!["cat".to_owned(), format!("{vowp_home}/secrets/mode")], None)?;
            if mode.success() && mode.stdout.trim() == "vault" {
                self.changed.push("vault".to_owned());
                self.check("vault.mode", "ok", "vault user mode");
            } else {
                self.check(
                    "vault.mode",
                    "warn",
                    "`vowp secret upgrade` finished but the vault mode was not recorded",
                );
            }
        } else if vault_ready {
            self.check("vault.mode", "ok", "vault user mode");
        } else {
            self.check(
                "vault.mode",
                "ok",
                format!(
                    "basic mode (same user); {}",
                    if self.probe.sudo == SudoMode::Password {
                        "sudo needs a password"
                    } else {
                        "sudo is not installed"
                    }
                ),
            );
        }

        let secrets = self.answers.secrets.clone();
        for secret in &secrets {
            let name = SecretName::parse(secret)?;
            let listed = self.remote(
                vec![
                    self.vowp_bin.clone(),
                    "--home".to_owned(),
                    vowp_home.clone(),
                    "secret".to_owned(),
                    "ls".to_owned(),
                    "--json".to_owned(),
                ],
                None,
            )?;
            let stored = listed.success()
                && serde_json::from_str::<serde_json::Value>(&listed.stdout)
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item.get("name").and_then(|value| value.as_str()) == Some(name.as_str()))
                    });
            if !stored {
                return Ok(Some(SetupHandoff {
                    phase: "vault".to_owned(),
                    kind: "secret".to_owned(),
                    prompt: format!(
                        "Save {name} with `vowp secret set {name}` on the home (hidden prompt) or `vowp secret request {name}` through a tunnel, then re-run setup. Never paste the value into the chat."
                    ),
                }));
            }
            self.check("vault.secret", "ok", format!("{name} is saved"));
        }
        Ok(None)
    }

    /// Phase 7: register the owner's repositories.
    fn projects_phase(&mut self) -> Result<Option<SetupHandoff>, CliError> {
        self.phase = "projects".to_owned();
        if self.answers.repositories.is_empty() {
            self.check("projects", "ok", "no repositories requested");
            return Ok(None);
        }
        let vowp_home = format!("{}/.vowp", self.remote_home);
        let repositories = self.answers.repositories.clone();
        for repository in &repositories {
            let repo = repository.trim_end_matches('/');
            let directory = self.remote(vec!["test".to_owned(), "-d".to_owned(), repo.to_owned()], None)?;
            let manifest = self.remote(
                vec!["test".to_owned(), "-f".to_owned(), format!("{repo}/vowp.toml")],
                None,
            )?;
            if !directory.success() || !manifest.success() {
                self.check("projects", "fail", format!("{repo} is not a checkout with vowp.toml"));
                return Ok(Some(SetupHandoff {
                    phase: "projects".to_owned(),
                    kind: "owner-answer".to_owned(),
                    prompt: format!(
                        "No vowp.toml in {repo}; run `vowp init` there (it is offered, never forced) or choose another repository, then re-run setup."
                    ),
                }));
            }
            let output = self.remote(
                vec![
                    self.vowp_bin.clone(),
                    "--home".to_owned(),
                    vowp_home.clone(),
                    "project".to_owned(),
                    "add".to_owned(),
                    repo.to_owned(),
                ],
                None,
            )?;
            if !output.success() {
                return Err(CliError::Remote(format!(
                    "registering {repo} failed: {}",
                    output.detail()
                )));
            }
            if output.stdout.contains("already registered") {
                self.check("projects", "ok", format!("{repo} is already registered"));
            } else {
                self.changed.push(repo.to_owned());
                self.check("projects", "ok", format!("registered {repo}"));
            }
        }
        Ok(None)
    }
}

/// Returns a useful failing-check summary from `vowp doctor --json`.
fn doctor_detail(output: &CommandOutput) -> String {
    if !output.stderr.trim().is_empty() {
        return output.detail();
    }
    if let Ok(document) = serde_json::from_str::<serde_json::Value>(&output.stdout)
        && let Some(checks) = document.get("checks").and_then(serde_json::Value::as_array)
    {
        let failures = checks
            .iter()
            .filter(|check| check.get("status").and_then(serde_json::Value::as_str) != Some("ok"))
            .filter_map(|check| {
                let name = check.get("name")?.as_str()?;
                let status = check.get("status")?.as_str()?;
                let detail = check.get("detail").and_then(serde_json::Value::as_str).unwrap_or("");
                Some(format!("{name} ({status}): {detail}"))
            })
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            return failures.join("; ");
        }
    }
    output.detail()
}

/// Fetches the home snapshot and writes the control cache when it changed.
///
/// The cache is intentionally left alone when the home has no snapshot yet or when the
/// fetched document equals the cached one; that keeps a second `setup control --apply`
/// idempotent.
fn update_control_snapshot(
    runner: &mut dyn SetupRunner,
    probe: &Probe,
    root: &std::path::Path,
    remote_home: &str,
    changed: &mut Vec<String>,
) -> Result<SetupCheck, CliError> {
    let existing = remote::cached_snapshot(root)?;
    let snapshot_path = format!("{}/snapshot.json", remote_home.trim_end_matches('/'));
    let argv = as_user(
        &probe.user,
        probe.effective_user(),
        &probe.effective_home(),
        vec!["cat".to_owned(), snapshot_path],
    );
    let output = runner.run(&argv, None)?;
    if !output.success() {
        return Ok(SetupCheck {
            name: "control.snapshot".to_owned(),
            status: if existing.is_some() { "ok" } else { "warn" }.to_owned(),
            detail: match existing {
                Some(cached) => format!(
                    "kept the cache as of {}; the home has no snapshot.json yet",
                    cached.fetched_at.as_rfc3339()
                ),
                None => "empty until the home writes snapshot.json".to_owned(),
            },
        });
    }
    let snapshot: Snapshot = match serde_json::from_str(&output.stdout) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(SetupCheck {
                name: "control.snapshot".to_owned(),
                status: if existing.is_some() { "ok" } else { "warn" }.to_owned(),
                detail: match existing {
                    Some(cached) => format!(
                        "kept the cache as of {}; the home snapshot is invalid: {error}",
                        cached.fetched_at.as_rfc3339()
                    ),
                    None => format!("the home snapshot is invalid: {error}"),
                },
            });
        }
    };
    if let Some(cached) = &existing
        && cached.snapshot == snapshot
        && root.join("snapshot.meta.json").is_file()
    {
        return Ok(SetupCheck {
            name: "control.snapshot".to_owned(),
            status: "ok".to_owned(),
            detail: format!("snapshot cache as of {}", cached.fetched_at.as_rfc3339()),
        });
    }
    let fetched_at = Timestamp::now();
    let snapshot_bytes = serde_json::to_vec_pretty(&snapshot).map_err(vowp_protocol::ProtocolError::from)?;
    if write_if_changed(&root.join("snapshot.json"), &snapshot_bytes)? {
        changed.push(root.join("snapshot.json").display().to_string());
    }
    let meta = remote::SnapshotMeta {
        fetched_at: Some(fetched_at),
        source: Some("remote".to_owned()),
    };
    let meta_bytes = serde_json::to_vec_pretty(&meta).map_err(vowp_protocol::ProtocolError::from)?;
    if write_if_changed(&root.join("snapshot.meta.json"), &meta_bytes)? {
        changed.push(root.join("snapshot.meta.json").display().to_string());
    }
    Ok(SetupCheck {
        name: "control.snapshot".to_owned(),
        status: "ok".to_owned(),
        detail: format!("snapshot cache as of {}", fetched_at.as_rfc3339()),
    })
}

/// Prints one outcome document; JSON is one object, text is the owner summary.
fn print_outcome_document(ctx: &Ctx, io: &mut dyn IoStreams, outcome: &SetupOutcome) -> Result<(), CliError> {
    if ctx.json() {
        output::print_json(io, outcome)?;
    } else {
        print_outcome(io, outcome)?;
    }
    Ok(())
}

/// Reads the local setup state, when present.
fn read_setup_state(root: &std::path::Path) -> Result<Option<LocalSetupState>, CliError> {
    let path = root.join("setup.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))
}

/// Writes the local setup state `setup control` reads; returns whether it changed.
fn write_setup_state(answers: &SetupAnswers, probe: &Probe, remote_home: &str) -> Result<bool, CliError> {
    let root = remote::control_root()?;
    remote::ensure_private_dir(&root)?;
    let name = match &answers.name {
        Some(name) => NodeName::parse(name)?.to_string(),
        None => probe.default_node_name(),
    };
    let path = root.join("setup.json");
    let existing = read_setup_state(&root)?;
    if existing.as_ref().is_some_and(|state| {
        state.target == answers.target
            && state.name == name
            && state.version == answers.version
            && state.remote_home == remote_home
    }) {
        return Ok(false);
    }
    let state = LocalSetupState {
        target: answers.target.clone(),
        name,
        version: answers.version.clone(),
        remote_home: remote_home.to_owned(),
        updated_at: Timestamp::now(),
    };
    let bytes = serde_json::to_vec_pretty(&state).map_err(vowp_protocol::ProtocolError::from)?;
    remote::write_private(&path, &bytes)?;
    Ok(true)
}

/// Writes `bytes` only when the file's content differs; returns whether it wrote.
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<bool, CliError> {
    if path.is_file() {
        let existing = std::fs::read(path)?;
        if existing == bytes {
            return Ok(false);
        }
    }
    remote::write_private(path, bytes)?;
    Ok(true)
}
