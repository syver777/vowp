//! `vowp gc` — lifecycle classes and garbage collection.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10.2. The dry run is the default; `--apply`
//! deletes only the disposable classes and compacts closed sessions. `--now` exists for tests.

use clap::Args;
use vowp_core::lifecycle::{GcOptions, class_name, gc_with_window, parse_class, parse_older_than};
use vowp_protocol::Timestamp;

use crate::output::{Table, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp gc`.
#[derive(Debug, Clone, Default, Args)]
pub struct GcArgs {
    /// Delete eligible files instead of only reporting.
    #[arg(long)]
    pub apply: bool,
    /// Restrict to one lifecycle class.
    #[arg(long, value_name = "CLASS")]
    pub class: Option<String>,
    /// Collect closed sessions and run output older than this window (for example 7d, 36h, 2w).
    ///
    /// Applies only to `closed_session` and `run_output`; every other class keeps its default.
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,
    /// Clock override for tests (RFC 3339).
    #[arg(long, value_name = "RFC3339")]
    pub now: Option<String>,
}

/// Runs `vowp gc`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &GcArgs) -> Result<i32, CliError> {
    let class = args.class.as_deref().map(parse_class).transpose()?;
    let older_than = args.older_than.as_deref().map(parse_older_than).transpose()?;
    let now = args.now.as_deref().map(Timestamp::parse).transpose()?;
    let opts = GcOptions {
        apply: args.apply,
        class,
        now,
    };
    let store = ctx.store()?;
    let report = gc_with_window(ctx.deps(), &store, &opts, older_than)?;

    if ctx.json() {
        print_json(io, &report)?;
        for error in &report.errors {
            writeln!(io.err(), "warning: {error}")?;
        }
        return Ok(0);
    }

    if report.classes.is_empty() {
        writeln!(io.out(), "no lifecycle files to collect")?;
    } else {
        let mut table = Table::new(&["CLASS", "FILES", "BYTES", "ELIGIBLE", "ELIGIBLE BYTES"]);
        for class in &report.classes {
            table.row(vec![
                class_name(class.class).to_owned(),
                class.files.to_string(),
                class.bytes.to_string(),
                class.eligible_files.to_string(),
                class.eligible_bytes.to_string(),
            ]);
        }
        writeln!(io.out(), "{}", table.render(ctx.color()))?;
        for class in &report.classes {
            if class.sample_paths.is_empty() {
                continue;
            }
            writeln!(
                io.out(),
                "{}: {}",
                class_name(class.class),
                class.sample_paths.join(", ")
            )?;
        }
    }

    if !report.refused.is_empty() {
        writeln!(io.out(), "refused {} candidate(s):", report.refused.len())?;
        for entry in &report.refused {
            let class = class_name(entry.class);
            match &entry.session_id {
                Some(session) => writeln!(io.out(), "  {class} session {session}: {}", entry.reason)?,
                None => writeln!(io.out(), "  {class} {}: {}", entry.path, entry.reason)?,
            }
        }
    }

    if report.dry_run {
        writeln!(
            io.out(),
            "dry run: nothing deleted; pass --apply to collect eligible files"
        )?;
    } else {
        writeln!(
            io.out(),
            "applied: deleted {} files, freed {} bytes, compacted {} sessions, closed {} job windows",
            report.deleted_files,
            report.freed_bytes,
            report.compacted_sessions,
            report.closed_windows
        )?;
    }
    for error in &report.errors {
        writeln!(io.err(), "warning: {error}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use tempfile::TempDir;
    use vowp_protocol::{
        Executor, Node, NodeName, Project, ProjectName, Session, SessionId, SessionState, Timestamp, VowpConfig,
    };
    use vowp_store::NodePaths;
    use vowp_store::Store;

    use crate::BufferIo;

    const NOW: &str = "2026-11-10T12:00:00Z";

    fn ts_now() -> Timestamp {
        Timestamp::parse(NOW).expect("timestamp")
    }

    fn days_ago(days: u64) -> Timestamp {
        ts_now().minus(Duration::from_secs(days * 24 * 60 * 60))
    }

    fn set_mtime(path: &std::path::Path, at: Timestamp) {
        let time = UNIX_EPOCH + Duration::from_millis(at.unix_millis() as u64);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(time)
            .expect("set mtime");
    }

    struct TestEnv {
        home: TempDir,
        project: TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            let home = tempfile::tempdir().expect("temp home");
            let project = tempfile::tempdir().expect("temp project");
            let mut config = VowpConfig::with_defaults_for_tests();
            config.project.name = ProjectName::parse("test").expect("project");
            config.project.path = Some(project.path().display().to_string());
            std::fs::write(
                project.path().join("vowp.toml"),
                toml::to_string_pretty(&config).expect("config"),
            )
            .expect("vowp.toml");

            let paths = NodePaths::new(home.path());
            let store = Store::open(&paths).expect("store");
            let now = ts_now();
            store
                .upsert_node(&Node {
                    name: NodeName::parse("local").expect("node"),
                    ssh_target: None,
                    is_local: true,
                    capabilities: serde_json::json!({}),
                    last_seen_at: Some(now),
                    created_at: now,
                })
                .expect("node row");
            store
                .upsert_project(&Project {
                    name: config.project.name.clone(),
                    path: project.path().display().to_string(),
                    remote_url: None,
                    default_branch: "main".to_owned(),
                    config: serde_json::to_value(&config).expect("config json"),
                    created_at: now,
                    updated_at: now,
                })
                .expect("project row");
            Self { home, project }
        }

        fn run(&self, args: &[&str]) -> (i32, String, String) {
            self.run_with(args, false)
        }

        fn run_json(&self, args: &[&str]) -> (i32, serde_json::Value, String) {
            let (code, stdout, stderr) = self.run_with(args, true);
            let value = serde_json::from_str(&stdout)
                .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
            (code, value, stderr)
        }

        fn run_with(&self, args: &[&str], json: bool) -> (i32, String, String) {
            let mut full = vec![
                "vowp".to_owned(),
                "--home".to_owned(),
                self.home.path().display().to_string(),
                "--project".to_owned(),
                self.project.path().display().to_string(),
            ];
            if json {
                full.push("--json".to_owned());
            }
            full.extend(args.iter().map(|value| (*value).to_owned()));
            let mut io = BufferIo::new("");
            let code = crate::run(full, &mut io);
            (code, io.stdout(), io.stderr())
        }

        fn store(&self) -> Store {
            Store::open(&NodePaths::new(self.home.path())).expect("store")
        }

        fn closed_session(&self, id: &SessionId, updated_at: Timestamp) {
            let paths = NodePaths::new(self.home.path());
            std::fs::create_dir_all(paths.blobs_dir(id)).expect("blobs");
            std::fs::create_dir_all(paths.capsules_dir(id)).expect("capsules");
            std::fs::write(paths.log_path(id), b"{\"seq\":1}\n").expect("log");
            self.store()
                .upsert_session(&Session {
                    id: id.clone(),
                    project: Some(ProjectName::parse("test").expect("project")),
                    job_id: None,
                    role: Some(vowp_protocol::SessionRole::Worker),
                    title: format!("session {id}"),
                    parent_session_id: None,
                    node: NodeName::parse("local").expect("node"),
                    cwd: self.project.path().display().to_string(),
                    state: SessionState::Closed,
                    executor: Some(Executor::parse("opencode:test/model").expect("executor")),
                    last_seq: 1,
                    last_capsule_id: None,
                    log_bytes: 11,
                    created_at: updated_at,
                    updated_at,
                })
                .expect("session row");
        }

        fn old_run(&self, name: &str) -> std::path::PathBuf {
            let paths = NodePaths::new(self.home.path());
            std::fs::create_dir_all(paths.runs_dir()).expect("runs dir");
            let path = paths.runs_dir().join(name);
            std::fs::write(&path, b"old run output\n").expect("run output");
            set_mtime(&path, days_ago(10));
            path
        }
    }

    #[test]
    fn older_than_bad_values_exit_two() {
        let env = TestEnv::new();
        for args in [
            vec!["gc", "--older-than", "0d"],
            vec!["gc", "--older-than=0s"],
            vec!["gc", "--older-than=-7d"],
            vec!["gc", "--older-than=7"],
            vec!["gc", "--older-than=7x"],
            vec!["gc", "--older-than=3651d"],
            vec!["gc", "--older-than", "7d", "--class", "cache"],
        ] {
            let (code, stdout, stderr) = env.run(&args);
            assert_eq!(code, 2, "`{}`: {stderr}", args.join(" "));
            assert!(
                stderr.contains("older-than"),
                "`{}` did not explain the flag: {stdout}{stderr}",
                args.join(" ")
            );
        }
    }

    #[test]
    fn older_than_dry_run_and_apply_show_class_count_bytes_and_refusals() {
        let env = TestEnv::new();
        let session = SessionId::new();
        env.closed_session(&session, days_ago(10));
        let log = NodePaths::new(env.home.path()).log_path(&session);
        let run = env.old_run("old.jsonl");

        let (code, value, stderr) = env.run_json(&["gc", "--older-than", "7d", "--now", NOW]);
        assert_eq!(code, 0, "{stderr}");
        assert_eq!(value["dry_run"], true, "{value}");
        assert!(value["refused"].is_array(), "{value}");
        let classes = value["classes"].as_array().expect("classes");
        let closed = classes
            .iter()
            .find(|class| class["class"] == "closed_session")
            .unwrap_or_else(|| panic!("closed_session missing: {value}"));
        assert_eq!(closed["eligible_files"], 1, "{closed}");
        assert!(closed["eligible_bytes"].as_u64().unwrap_or(0) >= 10, "{closed}");
        let output = classes
            .iter()
            .find(|class| class["class"] == "run_output")
            .unwrap_or_else(|| panic!("run_output missing: {value}"));
        assert_eq!(output["eligible_files"], 1, "{output}");
        assert!(log.exists(), "dry run deleted the session log");
        assert!(run.exists(), "dry run deleted the run output");

        let (code, stdout, stderr) = env.run(&["gc", "--older-than", "7d", "--apply", "--now", NOW]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("applied:"), "{stdout}");
        assert!(stdout.contains("compacted 1 sessions"), "{stdout}");
        assert!(!log.exists(), "apply did not compact the old session");
        assert!(!run.exists(), "apply did not delete the old run output");

        let (code, stdout, stderr) = env.run(&["session", "show", session.as_str()]);
        assert_eq!(code, 0, "session show failed after gc: {stderr}\n{stdout}");
        assert!(stdout.contains(&format!("id: {session}")), "{stdout}");

        let (code, stdout, stderr) = env.run(&["reindex"]);
        assert_eq!(code, 0, "reindex failed after gc: {stderr}");
        assert!(stdout.contains("reindexed"), "{stdout}");
    }

    #[test]
    fn older_than_help_lists_the_flag_and_its_classes() {
        let env = TestEnv::new();
        let (code, stdout, stderr) = env.run(&["gc", "--help"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("--older-than <DURATION>"), "{stdout}");
        assert!(stdout.contains("closed_session"), "{stdout}");
    }
}
