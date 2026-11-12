//! `vowp setup` — AI-first setup. Spec: `docs/design/v0.2/home-and-surfaces.md` §11.
//!
//! The setup agent asks its five questions once, then runs `setup plan`, shows the plan
//! once, gets one yes, and runs `setup home --apply --yes` followed by `setup control
//! --apply`. A hand-off stops the run and exits 5; secrets are named, never read.

use clap::{Args, Subcommand};

use crate::setup::SetupAnswers;
use crate::setup::commands;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp setup`.
#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// The setup subcommand.
    #[command(subcommand)]
    pub command: SetupCommand,
}

/// The `vowp setup` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum SetupCommand {
    /// Print every phase, command and file without changing anything.
    Plan(SetupPlanArgs),
    /// Set up the home on a fresh machine.
    Home(SetupHomeArgs),
    /// Set up this machine as the control machine.
    Control(SetupControlArgs),
    /// Run the fixture verification on the home.
    Verify(SetupVerifyArgs),
    /// Internal home bootstrap used by `setup home`; not part of the public contract.
    #[command(name = "__home-init", hide = true)]
    HomeInit(SetupHomeInitArgs),
}

/// The answer flags shared by `setup plan` and `setup home`.
#[derive(Debug, Clone, Default, Args)]
pub struct SetupAnswerArgs {
    /// VOWP version to install (default: this binary's version).
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Home node name (default: the target's short hostname).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// A repository path on the home to register; repeatable.
    #[arg(long = "repository", visible_alias = "repo", value_name = "PATH")]
    pub repositories: Vec<String>,
    /// A harness the workers should use (`opencode`, `claude-code`, `acp`); repeatable.
    #[arg(long = "harness", value_name = "NAME")]
    pub harnesses: Vec<String>,
    /// A provider name the chosen harnesses use; repeatable, never a key.
    #[arg(long = "provider", value_name = "NAME")]
    pub providers: Vec<String>,
    /// A secret name to check for; repeatable, never a value.
    #[arg(long = "secret", value_name = "NAME")]
    pub secrets: Vec<String>,
    /// Recommend a GitHub remote for merged work; setup never pushes.
    #[arg(long)]
    pub github_remote: bool,
}

impl SetupAnswerArgs {
    /// Builds the validated answer document for `target`.
    fn answers(&self, target: &str) -> Result<SetupAnswers, CliError> {
        let answers = SetupAnswers {
            target: target.to_owned(),
            version: self
                .version
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
            name: self.name.clone(),
            repositories: self.repositories.clone(),
            harnesses: self.harnesses.clone(),
            providers: self.providers.clone(),
            secrets: self.secrets.clone(),
            github_remote: self.github_remote,
        };
        answers.validate()?;
        Ok(answers)
    }
}

/// Arguments for `vowp setup plan`.
#[derive(Debug, Clone, Args)]
pub struct SetupPlanArgs {
    /// SSH target of the home to set up.
    #[arg(long, value_name = "SSH-TARGET")]
    pub target: String,
    /// The owner's answers.
    #[command(flatten)]
    pub answers: SetupAnswerArgs,
}

/// Arguments for `vowp setup home`.
#[derive(Debug, Clone, Args)]
pub struct SetupHomeArgs {
    /// SSH target of the home to set up.
    #[arg(long, value_name = "SSH-TARGET")]
    pub target: String,
    /// Apply the plan instead of only printing it.
    #[arg(long)]
    pub apply: bool,
    /// Do not ask before applying.
    #[arg(long)]
    pub yes: bool,
    /// The owner's answers.
    #[command(flatten)]
    pub answers: SetupAnswerArgs,
}

/// Arguments for `vowp setup control`.
#[derive(Debug, Clone, Default, Args)]
pub struct SetupControlArgs {
    /// Apply the plan instead of only printing it.
    #[arg(long)]
    pub apply: bool,
    /// SSH target of the home; defaults to the last `setup home` target.
    #[arg(long, value_name = "SSH-TARGET")]
    pub target: Option<String>,
    /// Home node name; defaults to the configured or last-used name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Replace a control configuration that points at another home.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `vowp setup verify`.
#[derive(Debug, Clone, Default, Args)]
pub struct SetupVerifyArgs {}

/// Arguments for the hidden `vowp setup __home-init`.
#[derive(Debug, Clone, Default, Args)]
pub struct SetupHomeInitArgs {
    /// Local node name; defaults to this home's configured name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Home directory to create; the process context may not know it yet.
    #[arg(long, value_name = "DIR")]
    pub root: Option<std::path::PathBuf>,
    /// Harness to write into the new home profiles; repeatable.
    #[arg(long = "harness", value_name = "NAME")]
    pub harnesses: Vec<String>,
    /// Provider name to write into the new home config; repeatable.
    #[arg(long = "provider", value_name = "NAME")]
    pub providers: Vec<String>,
}

/// Runs `vowp setup`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &SetupArgs) -> Result<i32, CliError> {
    match &args.command {
        SetupCommand::Plan(plan) => {
            let answers = plan.answers.answers(&plan.target)?;
            commands::run_plan(ctx, io, &answers)
        }
        SetupCommand::Home(home) => {
            let answers = home.answers.answers(&home.target)?;
            commands::run_home(ctx, io, &answers, home.apply, home.yes)
        }
        SetupCommand::Control(control) => commands::run_control(ctx, io, control),
        SetupCommand::Verify(args) => crate::setup::verify::run(ctx, io, args),
        SetupCommand::HomeInit(init) => commands::run_home_init(ctx, io, init),
    }
}
