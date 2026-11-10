//! `vowp init`: write the project config, framework files, node state and default loops.
//!
//! Spec: `docs/design/cli.md` §2 and `framework/README.md`. Existing files are skipped unless
//! `--force`; a second init without `--force` is a conflict (exit 4).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use vowp_protocol::{
    CapsuleConfig, Executor, Loop, LoopBounds, LoopKind, MergeConfig, Node, NodeConfig, OnQuota, Profile, Project,
    ProjectConfig, ProjectName, VowpConfig,
};
use vowp_store::Store;

use crate::cli::{Global, InitArgs};
use crate::{CliError, IoStreams};

/// JSON shape printed by `vowp init --json`.
#[derive(Debug, Serialize)]
struct InitOutput {
    project: String,
    path: String,
    written: Vec<String>,
    skipped: Vec<String>,
}

/// Runs `vowp init`.
pub fn run(global: &Global, io: &mut dyn IoStreams, args: &InitArgs) -> Result<i32, CliError> {
    let cwd = std::env::current_dir()?;
    let root = crate::ctx::find_git_root(&cwd)
        .ok_or_else(|| CliError::Invalid("init must run inside a git repository".to_owned()))?;
    let config_path = root.join("vowp.toml");
    if config_path.exists() && !args.force {
        return Err(CliError::Conflict(format!(
            "project already initialised at {}; use --force",
            root.display()
        )));
    }

    let name = match &args.name {
        Some(name) => ProjectName::parse(name)?,
        None => {
            let directory = root
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_owned());
            ProjectName::parse(&slugify(&directory))?
        }
    };
    let config = default_config(name, local_offset_minutes())?;
    config.validate()?;
    let config_text =
        toml::to_string_pretty(&config).map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;

    let mut written = Vec::new();
    let mut skipped = Vec::new();
    write_relative(&root, "vowp.toml", &config_text, args.force, &mut written, &mut skipped)?;

    std::fs::create_dir_all(root.join(".vowp/jobs"))?;
    std::fs::create_dir_all(root.join(".vowp/checkpoints"))?;

    if !args.no_framework {
        for file in crate::framework::FILES {
            if file.target == "CLAUDE.md" {
                write_claude_md(&root, file.content, args.force, &mut written, &mut skipped)?;
            } else {
                write_relative(&root, file.target, file.content, args.force, &mut written, &mut skipped)?;
            }
        }
    }

    if ensure_gitignore(&root)? {
        written.push(".gitignore".to_owned());
    }

    let paths = crate::ctx::node_paths(global)?;
    let store = Store::open(&paths)?;
    let now = store.clock().now();
    store.upsert_node(&Node {
        name: config.node.name.clone(),
        ssh_target: None,
        is_local: true,
        capabilities: serde_json::json!({ "vowp": env!("CARGO_PKG_VERSION") }),
        last_seen_at: Some(now),
        created_at: now,
    })?;
    let project_config = serde_json::to_value(&config).map_err(vowp_protocol::ProtocolError::from)?;
    store.upsert_project(&Project {
        name: config.project.name.clone(),
        path: root.display().to_string(),
        remote_url: None,
        default_branch: config.project.default_branch.clone(),
        config: project_config,
        created_at: now,
        updated_at: now,
    })?;
    for loop_ in default_loops(&config.project.name, now) {
        store.upsert_loop(&loop_)?;
    }

    if global.json {
        crate::output::print_json(
            io,
            &InitOutput {
                project: config.project.name.to_string(),
                path: root.display().to_string(),
                written,
                skipped,
            },
        )?;
    } else {
        writeln!(
            io.out(),
            "initialised project {} at {} ({} files written, {} skipped)",
            config.project.name,
            root.display(),
            written.len(),
            skipped.len()
        )?;
        if !skipped.is_empty() {
            writeln!(io.out(), "skipped: {}", skipped.join(", "))?;
        }
    }
    Ok(0)
}

/// Builds the config `init` writes: four default profiles and no schedules.
fn default_config(name: ProjectName, tz_offset_minutes: i32) -> Result<VowpConfig, CliError> {
    let opencode = Executor::parse("opencode:deepseek/deepseek-flash#max")?;
    let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium")?;
    let profile = |ladder: Vec<Executor>| Profile {
        ladder,
        on_quota: OnQuota::NextThenProbeBack,
        node: None,
    };
    let mut profiles = BTreeMap::new();
    profiles.insert("planner".to_owned(), profile(vec![claude, opencode.clone()]));
    profiles.insert("keeper".to_owned(), profile(vec![opencode.clone()]));
    profiles.insert("worker".to_owned(), profile(vec![opencode.clone()]));
    profiles.insert("verifier".to_owned(), profile(vec![opencode]));

    Ok(VowpConfig {
        node: NodeConfig {
            tz_offset_minutes,
            ..NodeConfig::default()
        },
        project: ProjectConfig {
            name,
            path: None,
            remote: "origin".to_owned(),
            default_branch: "main".to_owned(),
            check_cmds: Vec::new(),
            lock_files: vec!["Cargo.lock".to_owned()],
            worktree_root: None,
            always_allowed: vec!["Cargo.lock".to_owned()],
            push: true,
        },
        profiles,
        providers: BTreeMap::new(),
        harnesses: BTreeMap::new(),
        env: BTreeMap::new(),
        gates: BTreeMap::new(),
        loops: BTreeMap::new(),
        schedules: Vec::new(),
        capsule: CapsuleConfig::default(),
        merge: MergeConfig::default(),
    })
}

/// The nine loop rows created on first init, with the documented default intervals.
fn default_loops(project: &ProjectName, now: vowp_protocol::Timestamp) -> Vec<Loop> {
    let definitions = [
        ("l1-supervisor", LoopKind::L1, 60_u64),
        ("l2-worker", LoopKind::L2, 0),
        ("l3-verify", LoopKind::L3, 0),
        ("l4-planner", LoopKind::L4, 1800),
        ("l5-keeper", LoopKind::L5, 1200),
        ("l6-failover", LoopKind::L6, 0),
        ("l7-inbox", LoopKind::L7, 0),
        ("l8-watchdog", LoopKind::L8, 300),
        ("l9-schedules", LoopKind::L9, 60),
    ];
    definitions
        .into_iter()
        .map(|(name, kind, interval_secs)| Loop {
            name: name.to_owned(),
            kind,
            project: Some(project.clone()),
            interval_secs,
            state: vowp_protocol::ActiveState::Active,
            bounds: LoopBounds::default(),
            last_run_at: None,
            next_run_at: None,
            last_status: String::new(),
            fail_streak: 0,
            updated_at: now,
        })
        .collect()
}

/// Writes one file relative to the project root, skipping an existing file unless forced.
fn write_relative(
    root: &Path,
    target: &str,
    content: &str,
    force: bool,
    written: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<(), CliError> {
    let path = root.join(target);
    if path.exists() && !force {
        skipped.push(target.to_owned());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    written.push(target.to_owned());
    Ok(())
}

/// Writes `CLAUDE.md`; an existing file keeps its content and gains `@AGENTS.md` when missing.
fn write_claude_md(
    root: &Path,
    content: &str,
    force: bool,
    written: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<(), CliError> {
    let path = root.join("CLAUDE.md");
    if path.exists() && !force {
        let existing = std::fs::read_to_string(&path)?;
        if existing.lines().any(|line| line.trim() == "@AGENTS.md") {
            skipped.push("CLAUDE.md".to_owned());
            return Ok(());
        }
        let mut merged = existing;
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str("@AGENTS.md\n");
        std::fs::write(&path, merged)?;
        written.push("CLAUDE.md".to_owned());
        return Ok(());
    }
    write_relative(root, "CLAUDE.md", content, force, written, skipped)
}

/// Adds `.vowp/tmp/` to `.gitignore`; returns whether the file changed.
fn ensure_gitignore(root: &Path) -> Result<bool, CliError> {
    let path = root.join(".gitignore");
    let existing = if path.is_file() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    if existing.lines().any(|line| line.trim() == ".vowp/tmp/") {
        return Ok(false);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".vowp/tmp/\n");
    std::fs::write(&path, updated)?;
    Ok(true)
}

/// Returns the machine's current UTC offset in minutes, or 0 when `date` is unavailable.
fn local_offset_minutes() -> i32 {
    let Ok(output) = std::process::Command::new("date").arg("+%z").output() else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    parse_tz_offset(String::from_utf8_lossy(&output.stdout).trim()).unwrap_or(0)
}

/// Parses `+HHMM` or `-HHMM`.
fn parse_tz_offset(text: &str) -> Option<i32> {
    let sign = match text.get(..1)? {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let rest = text.get(1..)?;
    if rest.len() != 4 || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = rest.get(..2)?.parse().ok()?;
    let minutes: i32 = rest.get(2..)?.parse().ok()?;
    Some(sign * (hours * 60 + minutes))
}

/// Converts a directory name into a valid project slug.
fn slugify(value: &str) -> String {
    let mut slug = String::new();
    for character in value.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
        } else if character.is_ascii_uppercase() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "project".to_owned() } else { slug }
}
