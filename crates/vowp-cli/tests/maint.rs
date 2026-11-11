//! `vowp reindex`, `backup`, `schema`, `version`, and the CLI-wide listing and command-coverage tests.

mod common;

use vowp_cli::cli::V0_1_COMMANDS;

#[test]
fn backup_creates_openable_db() {
    let env = common::TestEnv::new();
    let path = env.home_path().join("manual-backup.db");
    let (code, stdout, stderr) = env.run(&["backup", "--to", path.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(path.to_str().expect("path")), "{stdout}");
    assert!(path.is_file());
    let store = vowp_store::Store::open_path(&path).expect("backup opens");
    assert_eq!(store.schema_version().expect("schema"), 1);
    assert_eq!(store.list_projects().expect("projects").len(), 1);
    store.integrity_check().expect("integrity");
}

#[test]
fn reindex_after_db_delete_via_cli() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["session", "new", "--title", "reindex me"]);
    assert_eq!(code, 0, "{stderr}");
    let id = stdout.trim().to_owned();
    assert!(!id.is_empty());

    // Remove the index database and call the CLI directly. `vowp reindex` must open/create
    // the empty index itself and rebuild it from `sessions/*/log.jsonl`.
    std::fs::remove_file(env.home_path().join("vowp.db")).expect("delete db");
    let (code, value, stderr) = env.run_json(&["reindex"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["sessions"], 1);
    assert!(value["events"].as_u64().unwrap_or(0) >= 1);

    let store = env.store();
    let session = store
        .require_session(&vowp_protocol::SessionId::parse(&id).expect("id"))
        .expect("session restored");
    assert_eq!(session.title, "reindex me");
    assert!(session.last_seq >= 1);
}

#[test]
fn version_prints_binary_git_and_schema() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["version"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("vowp "), "{stdout}");
    assert!(stdout.contains("schema 1"), "{stdout}");

    let (code, value, stderr) = env.run_json(&["version"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["schema"], 1);
    assert!(value["version"].as_str().unwrap_or_default().starts_with("0.1"));
}

#[test]
fn schema_lists_names_and_prints_one_document() {
    let env = common::TestEnv::new();
    let (code, value, stderr) = env.run_json(&["schema"]);
    assert_eq!(code, 0, "{stderr}");
    let names = value.as_array().expect("names");
    assert!(names.iter().any(|name| name == "Job"), "{value}");
    assert!(names.iter().any(|name| name == "Session"), "{value}");

    let (code, value, stderr) = env.run_json(&["schema", "Job"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(value["properties"].is_object(), "{value}");

    let (code, _, _) = env.run(&["schema", "NoSuchType"]);
    assert_eq!(code, 3);
}

#[test]
fn json_flag_outputs_valid_json_for_every_listing_command() {
    let env = common::TestEnv::new();
    for args in [
        vec!["job", "ls"],
        vec!["session", "ls"],
        vec!["inbox", "ls"],
        vec!["node", "ls"],
        vec!["loops"],
        vec!["schedule", "ls"],
        vec!["releases"],
        vec!["schema"],
    ] {
        let (code, value, stderr) = env.run_json(&args);
        assert_eq!(code, 0, "`{}` failed: {stderr}\nstdout: {value}", args.join(" "));
        assert!(
            value.is_array() || value.is_object(),
            "`{}` printed {value}",
            args.join(" ")
        );
    }

    let (code, value, stderr) = env.run_json(&["cost"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["by"], "job");
    assert!(value["rows"].is_array());
    assert!(value["total"].is_object());
}

#[test]
fn no_command_returns_not_implemented() {
    let mut env = common::TestEnv::new();
    // Point every harness probe at a fake script so `doctor` and `plan` never run the real
    // claude/opencode binaries installed on this machine.
    let fake = env.home_path().join("fake-harness.sh");
    std::fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.0.28'; exit 0; fi\nexit 0\n",
    )
    .expect("fake harness");
    let mut permissions = std::fs::metadata(&fake).expect("stat").permissions();
    use std::os::unix::fs::PermissionsExt as _;
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake, permissions).expect("chmod");
    let mut config = env.config.clone();
    for name in ["opencode", "claude-code"] {
        config.harnesses.insert(
            name.to_owned(),
            vowp_protocol::HarnessConfig {
                binary: Some(fake.display().to_string()),
                ..vowp_protocol::HarnessConfig::default()
            },
        );
    }
    std::fs::write(
        env.project_path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("write config");
    env.config = config;

    let backup = env.home_path().join("cli-smoke-backup.db");
    let backup_text = backup.display().to_string();
    // `init` stays local and walks the cwd for a git repository; run it in a throwaway
    // repository so the smoke test can never write into this checkout.
    let init_dir = tempfile::tempdir().expect("temp init dir");
    std::fs::create_dir(init_dir.path().join(".git")).expect("git dir");
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(init_dir.path())
        .arg("--home")
        .arg(env.home_path())
        .args(["init", "--name", "smoke"])
        .output()
        .expect("run vowp init");
    let init_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!init_text.contains("not implemented"), "{init_text}");

    let commands: Vec<Vec<&str>> = vec![
        vec!["doctor"],
        vec!["up", "--once", "--no-tmux"],
        vec!["down"],
        vec!["status", "--check"],
        vec!["board", "--once"],
        vec!["plan", "smoke", "--dry-run"],
        vec!["job", "ls"],
        vec!["graph"],
        vec!["attach"],
        vec!["session", "ls"],
        vec!["chat", "00000000000000000000000000"],
        vec!["inbox", "ls"],
        vec!["cost"],
        vec!["node", "ls"],
        vec!["loops"],
        vec!["schedule", "ls"],
        vec!["promote", "HEAD", "--to", "missing-env"],
        vec!["rollback", "00000000000000000000000000"],
        vec!["releases"],
        vec!["reindex"],
        vec!["backup", "--to", &backup_text],
        vec!["schema"],
        vec!["version"],
        vec!["__window", "board"],
    ];
    assert_eq!(
        commands.len() + 1,
        V0_1_COMMANDS.len(),
        "every v0.1 command needs a dry invocation"
    );
    for args in commands {
        let (code, stdout, stderr) = env.run(&args);
        let text = format!("{stdout}\n{stderr}");
        if text.contains("not implemented") {
            // A command may answer NotImplemented while its own handler source still returns
            // that error. Once the handler is implemented, a NotImplemented return fails.
            let source = source_for(args[0]);
            assert!(
                source.contains("Err(CliError::NotImplemented("),
                "`{}` returned NotImplemented (exit {code}) but its handler is implemented: {text}",
                args.join(" ")
            );
        }
    }
}

/// Returns the source file for one top-level command.
fn source_for(command: &str) -> String {
    let module = match command {
        "chat" | "session" => "session",
        "up" | "down" => "up",
        "job" => "job",
        "graph" => "graph",
        "inbox" => "inbox",
        "cost" => "cost",
        "node" => "node",
        "loops" => "loops",
        "schedule" => "schedule",
        "promote" | "rollback" | "releases" => "promote",
        "reindex" | "backup" | "schema" | "version" => "maint",
        "__window" => "window",
        other => other,
    };
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/cmd")
            .join(format!("{module}.rs")),
    )
    .unwrap_or_default()
}

#[test]
fn node_add_and_rm_probe_with_fake_ssh() {
    let env = common::TestEnv::new();
    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' 'vowp 0.1.0-dev'\nexit 0\n").expect("fake ssh");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!("{}:/usr/bin:/bin", bin.path().display());

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", path)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["node", "add", "vps", "vps.example", "--strict"])
        .output()
        .expect("node add");
    assert!(
        output.status.success(),
        "node add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (code, value, stderr) = env.run_json(&["node", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let node = value
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["name"] == "vps")
        .expect("vps row");
    assert_eq!(node["ssh_target"], "vps.example");
    assert_eq!(node["is_local"], false);
    assert_eq!(node["capabilities"]["vowp"], "vowp 0.1.0-dev");

    let (code, _, stderr) = env.run(&["node", "rm", "vps"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        env.store()
            .node(&vowp_protocol::NodeName::parse("vps").expect("node"))
            .expect("lookup")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Backup and restore
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vowp_core::lifecycle::backup::{BackupOptions, backup as core_backup, restore as core_restore};
use vowp_core::{
    AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, CoreError, Deps, FixedRng, SystemGit, SystemRunner,
};
use vowp_protocol::{
    Actor, Clock, Cost, ErrorKind, EventId, EventIndexRow, EventType, FakeClock, HarnessKind, JobId, JobKind,
    MemoryLog, NewJob, NodeName, Project, ProjectName, Role, Run, RunId, RunPurpose, RunRole, Session, SessionId,
    SessionState, Timestamp,
};
use vowp_store::{JobFilter, NodePaths, SessionFilter, Store};
use vowp_tmux::fake::FakeWindows;

/// One hermetic node with a fake clock and a recording command runner.
struct BackupFixture {
    env: common::TestEnv,
    clock: FakeClock,
    runner: RecordingRunner,
}

impl BackupFixture {
    fn new() -> Self {
        Self {
            env: common::TestEnv::new(),
            clock: FakeClock::at(timestamp("2026-01-01T02:00:00Z")),
            runner: RecordingRunner::default(),
        }
    }

    fn deps(&self) -> Deps {
        Deps {
            paths: NodePaths::new(self.env.home_path()),
            config: Arc::new(self.env.config.clone()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(self.clock.clone()),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
            cmd: Arc::new(self.runner.clone()),
            rng: Arc::new(FixedRng(0)),
        }
    }

    fn store(&self) -> Store {
        Store::open_global(&NodePaths::new(self.env.home_path())).expect("global store")
    }

    fn add_project(&self, name: &str) -> ProjectName {
        let name = ProjectName::parse(name).expect("project name");
        let now = self.clock.now();
        self.env
            .store()
            .upsert_project(&Project {
                name: name.clone(),
                path: self.env.project_path().join(name.as_str()).display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        name
    }

    fn insert_job(&self, project: &ProjectName, title: &str) -> JobId {
        let store = Store::open_with_project(&NodePaths::new(self.env.home_path()), project).expect("project store");
        let id = JobId::parse(&title.to_lowercase().replace(' ', "-")).expect("job id");
        let new = NewJob {
            id: id.clone(),
            title: title.to_owned(),
            kind: JobKind::Build,
            role: Role::Worker,
            profile: None,
            needs: Vec::new(),
            owns: Vec::new(),
            reads: Vec::new(),
            gates: None,
            priority: None,
            loop_: None,
            brief_text: None,
        };
        store.insert_job(project, &new, "test").expect("insert job");
        id
    }

    fn session(&self, title: &str) -> Session {
        Session {
            id: SessionId::new(),
            project: None,
            job_id: None,
            role: None,
            title: title.to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/tmp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: self.clock.now(),
            updated_at: self.clock.now(),
        }
    }

    fn run_backup(&self, store: &Store, opts: BackupOptions) -> Result<vowp_protocol::BackupReport, CoreError> {
        core_backup(&self.deps(), store, &opts)
    }

    fn run_restore(
        &self,
        source: &Path,
        to_home: &Path,
        force: bool,
    ) -> Result<vowp_protocol::BackupReport, CoreError> {
        core_restore(
            &self.deps(),
            &BackupOptions {
                restore: Some(source.to_path_buf()),
                to_home: Some(to_home.to_path_buf()),
                force,
                ..BackupOptions::default()
            },
        )
    }
}

/// A command runner that records argv and simulates `age` file output.
#[derive(Clone, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<CommandSpec>>>,
    fail_age: bool,
}

impl RecordingRunner {
    fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().expect("calls").clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CoreError> {
        assert!(
            spec.shell.is_none(),
            "external tools must run through argv, not a shell"
        );
        self.calls.lock().expect("calls").push(spec.clone());
        let program = spec.argv.first().map(String::as_str).unwrap_or_default();
        if program == "age" {
            if self.fail_age {
                return Err(CoreError::Io {
                    path: "age".to_owned(),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                });
            }
            let output = spec
                .argv
                .iter()
                .position(|arg| arg == "-o")
                .and_then(|index| spec.argv.get(index + 1))
                .map(PathBuf::from);
            let source = spec.argv.last().map(PathBuf::from);
            if let (Some(output), Some(source)) = (output, source) {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent).expect("age parent");
                }
                std::fs::copy(&source, &output).expect("fake age copy");
            }
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            output: Vec::new(),
            duration: Duration::ZERO,
        })
    }
}

fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

fn backup_directories(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("backup dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn backup_default_writes_full_backup_directory() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["backup"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("files"), "{stdout}");

    let backups = env.home_path().join("backups");
    let directories = backup_directories(&backups);
    assert_eq!(directories.len(), 1, "{directories:?}");
    let backup = backups.join(&directories[0]);
    assert!(backup.join("global.db").is_file());
    assert!(backup.join("manifest.json").is_file());
    assert!(backup.join("catalog.json").is_file());
}

#[test]
fn backup_global_and_all_project_dbs_are_openable() {
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    let beta = fixture.add_project("beta");
    fixture.insert_job(&alpha, "alpha job");
    fixture.insert_job(&beta, "beta job");

    let store = fixture.store();
    let destination = fixture.env.home_path().join("all-projects");
    let report = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    assert!(!report.files.is_empty());

    assert!(destination.join("global.db").is_file());
    assert!(
        destination
            .join("projects")
            .join(format!("{}.db", alpha.as_str()))
            .is_file()
    );
    assert!(
        destination
            .join("projects")
            .join(format!("{}.db", beta.as_str()))
            .is_file()
    );

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("restore");

    let paths = NodePaths::new(restore_home.path());
    let global = Store::open_global(&paths).expect("global");
    let projects = global.list_projects().expect("projects");
    assert_eq!(projects.len(), 2);
    for (project, title) in [(&alpha, "alpha job"), (&beta, "beta job")] {
        let restored = Store::open_with_project(&paths, project).expect("project");
        let jobs = restored.list_jobs(&JobFilter::default()).expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, title);
    }
}

#[test]
fn backup_rotation_seven_daily_four_weekly() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let start = timestamp("2026-01-01T02:00:00Z");
    for day in 0..20 {
        fixture.clock.set(start.plus(Duration::from_secs(day * 24 * 60 * 60)));
        fixture.run_backup(&store, BackupOptions::default()).expect("backup");
    }

    let names = backup_directories(&fixture.env.home_path().join("backups"));
    // Seven newest days (2026-01-14 .. 2026-01-20) plus the newest day of each of the
    // four latest ISO weeks (2026-01-04, 2026-01-11, 2026-01-18, 2026-01-20).
    assert_eq!(names.len(), 9, "{names:?}");
    for day in [
        "20260104", "20260111", "20260114", "20260115", "20260116", "20260117", "20260118", "20260119", "20260120",
    ] {
        assert!(
            names.iter().any(|name| name.starts_with(day)),
            "missing {day} in {names:?}"
        );
    }
    assert!(!names.iter().any(|name| name.starts_with("20260110")), "{names:?}");
    assert!(!names.iter().any(|name| name.starts_with("20260113")), "{names:?}");
}

#[test]
fn backup_rotation_weekly_keeps_newest_backup_of_the_week() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    // Three backups share Sunday 2026-01-04. Once seven newer days exist that date is
    // outside the daily picks, so only the weekly union can keep it.
    for hour in ["01", "12", "23"] {
        fixture.clock.set(timestamp(&format!("2026-01-04T{hour}:00:00Z")));
        fixture.run_backup(&store, BackupOptions::default()).expect("backup");
    }
    let start = timestamp("2026-01-14T02:00:00Z");
    for day in 0..7 {
        fixture.clock.set(start.plus(Duration::from_secs(day * 24 * 60 * 60)));
        fixture.run_backup(&store, BackupOptions::default()).expect("backup");
    }

    let names = backup_directories(&fixture.env.home_path().join("backups"));
    assert_eq!(names.len(), 8, "{names:?}");
    assert!(
        names.iter().any(|name| name.starts_with("20260104T230000")),
        "the weekly candidate must be the newest backup on the date: {names:?}"
    );
    for stale in ["20260104T010000", "20260104T120000"] {
        assert!(
            !names.iter().any(|name| name.starts_with(stale)),
            "{stale} is an older backup of the same date: {names:?}"
        );
    }
}

#[test]
fn backup_to_rsync_uses_argv_not_shell() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let target = "rsync://backup.example/vowp/";
    let report = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(target.to_owned()),
                ..BackupOptions::default()
            },
        )
        .expect("offsite backup");
    assert_eq!(report.offsite.as_deref(), Some(target));

    let calls = fixture.runner.calls();
    let rsync = calls
        .iter()
        .find(|call| call.argv.first().map(String::as_str) == Some("rsync"))
        .expect("rsync call");
    assert!(rsync.shell.is_none());
    assert_eq!(rsync.argv[0], "rsync");
    assert_eq!(rsync.argv[1], "-a");
    assert_eq!(rsync.argv[2], format!("{}/", report.directory));
    assert_eq!(rsync.argv[3], target);
    assert!(!fixture.env.home_path().join("rsync:").exists());
}

#[test]
fn backup_age_encryption_when_configured() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let destination = fixture.env.home_path().join("encrypted");
    let report = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(destination.display().to_string()),
                encrypt: Some("age1testrecipient".to_owned()),
                ..BackupOptions::default()
            },
        )
        .expect("encrypted backup");
    assert!(report.encrypted);
    assert!(report.files.iter().all(|file| file.path.ends_with(".age")));
    assert!(destination.join("global.db.age").is_file());
    assert!(!destination.join("global.db").exists());

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(manifest["encrypted"], true);

    let age = fixture
        .runner
        .calls()
        .into_iter()
        .find(|call| call.argv.first().map(String::as_str) == Some("age"))
        .expect("age call");
    assert!(age.argv.windows(2).any(|pair| pair == ["-r", "age1testrecipient"]));
    assert!(age.shell.is_none());

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("encrypted restore");
    assert!(restore_home.path().join("vowp.db").is_file());
}

#[test]
fn backup_age_missing_binary_is_refused_with_exit_5() {
    let fixture = BackupFixture {
        runner: RecordingRunner {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_age: true,
        },
        ..BackupFixture::new()
    };
    let store = fixture.store();
    let error = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(fixture.env.home_path().join("missing-age").display().to_string()),
                encrypt: Some("age1testrecipient".to_owned()),
                ..BackupOptions::default()
            },
        )
        .expect_err("missing age must fail");
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert_eq!(vowp_cli::CliError::from(error).exit_code(), 5);
    assert!(!fixture.env.home_path().join("missing-age").exists());
}

#[test]
fn restore_drill_reopens_and_counts_match() {
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    let beta = fixture.add_project("beta");
    fixture.insert_job(&alpha, "alpha job");
    fixture.insert_job(&beta, "beta job");

    let (code, stdout, stderr) = fixture.env.run(&["session", "new", "--title", "drill session"]);
    assert_eq!(code, 0, "{stderr}");
    let session = SessionId::parse(stdout.trim()).expect("session id");

    let store = fixture.store();
    let backup = fixture.env.home_path().join("drill-backup");
    fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(backup.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");

    let restore_home = tempfile::tempdir().expect("restore home");
    let report = fixture
        .run_restore(&backup, restore_home.path(), false)
        .expect("restore");
    assert!(!report.files.is_empty());

    let paths = NodePaths::new(restore_home.path());
    let global = Store::open_global(&paths).expect("global");
    let sessions = global.list_sessions(&SessionFilter::default()).expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
    assert_eq!(sessions[0].title, "drill session");
    assert_eq!(global.list_projects().expect("projects").len(), 2);

    for (project, title) in [(&alpha, "alpha job"), (&beta, "beta job")] {
        let restored = Store::open_with_project(&paths, project).expect("project");
        let jobs = restored.list_jobs(&JobFilter::default()).expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, title);
    }

    let conflict = fixture
        .run_restore(&backup, restore_home.path(), false)
        .expect_err("existing home needs --force");
    assert_eq!(conflict.kind(), ErrorKind::Conflict);
    fixture
        .run_restore(&backup, restore_home.path(), true)
        .expect("forced restore");
}

#[test]
fn restore_corrupt_backup_leaves_no_partial_home() {
    let fixture = BackupFixture::new();
    let session = SessionId::new();
    let session_dir = fixture.env.home_path().join("sessions").join(session.as_str());
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(session_dir.join("log.jsonl"), "{\"seq\":1}\n").expect("log");

    let destination = fixture.env.home_path().join("corrupt-backup");
    fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    let corrupt_log = destination.join("sessions").join(session.as_str()).join("log.jsonl");
    std::fs::write(&corrupt_log, "{\"seq\":1}\ncorrupt\n").expect("corrupt log");

    let parent = tempfile::tempdir().expect("restore parent");
    let target = parent.path().join("restored-home");
    let error = fixture
        .run_restore(&destination, &target, false)
        .expect_err("a corrupt content file must fail the restore");
    assert!(
        matches!(error, CoreError::Store(vowp_store::StoreError::Corrupt { .. })),
        "{error:?}"
    );
    assert!(!target.exists(), "a failed restore must leave no partial home");
    let leftovers: Vec<String> = std::fs::read_dir(parent.path())
        .expect("restore parent")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "staging directory left behind: {leftovers:?}");

    // The failed restore did not create the target, so a retry needs no `--force` and
    // fails on the same corrupt data instead of a target conflict.
    let retry = fixture
        .run_restore(&destination, &target, false)
        .expect_err("the retry must still report corruption");
    assert!(
        matches!(retry, CoreError::Store(vowp_store::StoreError::Corrupt { .. })),
        "{retry:?}"
    );
    assert!(!target.exists());
}

#[test]
fn restore_corrupt_backup_leaves_an_existing_target_untouched() {
    let fixture = BackupFixture::new();
    let session = SessionId::new();
    let session_dir = fixture.env.home_path().join("sessions").join(session.as_str());
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(session_dir.join("log.jsonl"), "{\"seq\":1}\n").expect("log");

    let destination = fixture.env.home_path().join("corrupt-forced-backup");
    fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    let corrupt_log = destination.join("sessions").join(session.as_str()).join("log.jsonl");
    std::fs::write(&corrupt_log, "{\"seq\":1}\ncorrupt\n").expect("corrupt log");

    let target = tempfile::tempdir().expect("existing target");
    let original = target.path().join("vowp.db");
    std::fs::write(&original, b"original database").expect("sentinel db");
    let error = fixture
        .run_restore(&destination, target.path(), true)
        .expect_err("a corrupt content file must fail the forced restore");
    assert!(
        matches!(error, CoreError::Store(vowp_store::StoreError::Corrupt { .. })),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read(&original).expect("original database still present"),
        b"original database"
    );
    assert!(!target.path().join("sessions").exists(), "no partial session tree");
    assert!(!target.path().join("projects").exists(), "no partial project tree");
}

#[test]
fn backup_includes_session_logs_and_blobs() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let session = SessionId::new();
    let session_dir = fixture.env.home_path().join("sessions").join(session.as_str());
    std::fs::create_dir_all(session_dir.join("blobs").join("ab")).expect("blobs dir");
    std::fs::create_dir_all(session_dir.join("capsules")).expect("capsules dir");
    std::fs::write(session_dir.join("log.jsonl"), "{\"seq\":1}\n").expect("log");
    std::fs::write(session_dir.join("blobs").join("ab").join("blob.bin"), b"blob bytes").expect("blob");
    std::fs::write(session_dir.join("capsules").join("cap.json"), "{\"id\":\"cap\"}\n").expect("capsule");
    std::fs::write(session_dir.join("adapters.json"), "{\"adapters\":[]}\n").expect("adapters");
    std::fs::write(session_dir.join("summary.md"), "# summary\n").expect("summary");

    let secrets = fixture.env.home_path().join("secrets");
    std::fs::create_dir_all(&secrets).expect("secrets dir");
    std::fs::write(secrets.join("API_KEY"), "do-not-copy").expect("secret");

    let destination = fixture.env.home_path().join("session-backup");
    let report = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    assert!(
        report.skipped.iter().any(|entry| entry.starts_with("secrets/")),
        "{:?}",
        report.skipped
    );

    let root = destination.join("sessions").join(session.as_str());
    assert_eq!(std::fs::read(root.join("log.jsonl")).expect("log"), b"{\"seq\":1}\n");
    assert_eq!(
        std::fs::read(root.join("blobs").join("ab").join("blob.bin")).expect("blob"),
        b"blob bytes"
    );
    assert!(root.join("capsules").join("cap.json").is_file());
    assert!(root.join("adapters.json").is_file());
    assert!(root.join("summary.md").is_file());
    assert!(!destination.join("secrets").exists());

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("restore");
    let restored = restore_home.path().join("sessions").join(session.as_str());
    assert_eq!(
        std::fs::read(restored.join("log.jsonl")).expect("restored log"),
        b"{\"seq\":1}\n"
    );
    assert_eq!(
        std::fs::read(restored.join("blobs").join("ab").join("blob.bin")).expect("restored blob"),
        b"blob bytes"
    );
}

#[test]
fn backup_manifest_records_counts() {
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    fixture.insert_job(&alpha, "counted job");
    let store = fixture.store();
    let destination = fixture.env.home_path().join("counted-backup");
    fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    let counts = manifest["row_counts"]["projects"][alpha.as_str()]["jobs"]
        .as_u64()
        .expect("job count");
    assert_eq!(counts, 1);
    assert_eq!(manifest["row_counts"]["global"]["projects"], 1);
}

#[test]
fn backup_manifest_counts_session_event_rows_not_last_seq() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let session = fixture.session("event gap");
    store.upsert_session(&session).expect("session");
    let now = fixture.clock.now();
    for seq in [1_u64, 3] {
        store
            .index_event(&EventIndexRow {
                session_id: session.id.clone(),
                seq,
                id: EventId::new(),
                ts: now,
                event_type: EventType::Note,
                actor: Actor::System,
                harness: HarnessKind::Vowp,
                model: None,
                byte_offset: (seq - 1) * 10,
                byte_len: 10,
                summary: format!("event {seq}"),
            })
            .expect("index event");
    }
    let row = store
        .session(&session.id)
        .expect("session lookup")
        .expect("session row");
    assert_eq!(row.last_seq, 3, "the sequence gap must be visible");
    drop(store);

    let destination = fixture.env.home_path().join("event-count-backup");
    fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(
        manifest["row_counts"]["global"]["session_events"].as_u64(),
        Some(2),
        "the manifest must count event rows, not the sessions' last_seq"
    );

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("the row-count restore drill must pass");
    let restored = Store::open_global(&NodePaths::new(restore_home.path())).expect("restored global");
    assert_eq!(restored.count_session_events().expect("event rows"), 2);
}

#[test]
fn backup_manifest_counts_orphan_runs() {
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    let job = fixture.insert_job(&alpha, "orphaned run job");
    let paths = NodePaths::new(fixture.env.home_path());
    let project = Store::open_with_project(&paths, &alpha).expect("project store");
    project
        .insert_run(&Run {
            id: RunId::new(),
            job_id: Some(job.clone()),
            session_id: None,
            role: RunRole::Worker,
            purpose: RunPurpose::Start,
            profile: None,
            rung: 0,
            executor: None,
            provider: None,
            node: NodeName::parse("local").expect("node"),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at: fixture.clock.now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        })
        .expect("run");
    // Deleting the job leaves the run behind with a NULL job_id: an orphan run.
    project.delete_job(&job).expect("delete job");
    assert_eq!(project.list_jobs(&JobFilter::default()).expect("jobs").len(), 0);
    assert_eq!(project.count_runs().expect("run rows"), 1);
    drop(project);

    let destination = fixture.env.home_path().join("orphan-run-backup");
    fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(
        manifest["row_counts"]["projects"][alpha.as_str()]["runs"].as_u64(),
        Some(1),
        "the manifest must count orphan runs too"
    );

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("the orphan-run restore drill must pass");
    let restored = Store::open_with_project(&NodePaths::new(restore_home.path()), &alpha).expect("restored project");
    assert_eq!(restored.count_runs().expect("restored run rows"), 1);
}

#[test]
fn backup_to_git_uses_argv_not_shell() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let target = "git:https://example.invalid/vowp-backup.git";
    let report = fixture
        .run_backup(
            &store,
            BackupOptions {
                to: Some(target.to_owned()),
                ..BackupOptions::default()
            },
        )
        .expect("git offsite backup");
    assert_eq!(report.offsite.as_deref(), Some(target));

    let git_calls: Vec<CommandSpec> = fixture
        .runner
        .calls()
        .into_iter()
        .filter(|call| call.argv.first().map(String::as_str) == Some("git"))
        .collect();
    assert_eq!(git_calls.len(), 4, "{git_calls:?}");
    assert!(git_calls.iter().all(|call| call.shell.is_none()));
    assert_eq!(git_calls[0].argv, ["git", "init", "-q"]);
    assert_eq!(git_calls[1].argv, ["git", "add", "-A"]);
    assert_eq!(
        git_calls[2].argv,
        [
            "git",
            "-c",
            "user.name=vowp",
            "-c",
            "user.email=vowp@localhost",
            "commit",
            "-q",
            "-m",
            "vowp backup",
        ]
    );
    assert_eq!(
        git_calls[3].argv,
        [
            "git",
            "push",
            "--force",
            "https://example.invalid/vowp-backup.git",
            "HEAD:refs/heads/main",
        ]
    );
}

#[test]
fn backup_directory_and_restore_via_cli() {
    let env = common::TestEnv::new();
    let backup = env.home_path().join("cli-backup");
    let backup_text = backup.display().to_string();
    let (code, stdout, stderr) = env.run(&["backup", "--to", &backup_text]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("files"), "{stdout}");
    assert!(backup.join("global.db").is_file());
    assert!(backup.join("manifest.json").is_file());

    // An explicit destination is replaced atomically on the next run.
    let (code, _stdout, stderr) = env.run(&["backup", "--to", &backup_text]);
    assert_eq!(code, 0, "replace explicit backup: {stderr}");

    let restore = env.home_path().join("cli-restored");
    let restore_text = restore.display().to_string();
    let (code, stdout, stderr) = env.run(&["backup", "--restore", &backup_text, "--to-home", &restore_text]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&backup_text), "{stdout}");
    assert!(restore.join("vowp.db").is_file());

    let (code, _stdout, stderr) = env.run(&["backup", "--restore", &backup_text, "--to-home", &restore_text]);
    assert_eq!(code, 4, "restore without --force: {stderr}");
    let (code, _stdout, stderr) = env.run(&[
        "backup",
        "--restore",
        &backup_text,
        "--to-home",
        &restore_text,
        "--force",
    ]);
    assert_eq!(code, 0, "forced restore: {stderr}");
}

#[test]
fn restore_works_without_an_initialized_source_home() {
    let env = common::TestEnv::new();
    let backup = env.home_path().join("lost-home-backup");
    let backup_text = backup.display().to_string();
    let (code, _stdout, stderr) = env.run(&["backup", "--to", &backup_text]);
    assert_eq!(code, 0, "{stderr}");

    // A machine that lost its serving home has no `vowp.db` and no `vowp init`. Running
    // with that fresh HOME must still reach the restore path.
    let lost_home = tempfile::tempdir().expect("lost home");
    let restore_parent = tempfile::tempdir().expect("restore parent");
    let restore_home = restore_parent.path().join("rescued");
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("HOME", lost_home.path())
        .env_remove("VOWP_HOME")
        .env_remove("VOWP_CONTROL_HOME")
        .current_dir(lost_home.path())
        .arg("backup")
        .arg("--restore")
        .arg(&backup_text)
        .arg("--to-home")
        .arg(&restore_home)
        .output()
        .expect("run vowp restore");
    assert!(
        output.status.success(),
        "restore from an uninitialised home failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(restore_home.join("vowp.db").is_file());
    let restored = Store::open_global(&NodePaths::new(&restore_home)).expect("restored global");
    assert_eq!(restored.list_projects().expect("projects").len(), 1);
    assert!(
        !lost_home.path().join(".vowp").join("vowp.db").exists(),
        "restore must not initialise the source home"
    );
}

#[test]
fn backup_offsite_cli_runs_rsync_argv() {
    let env = common::TestEnv::new();
    let bin = tempfile::tempdir().expect("fake bin");
    let rsync = bin.path().join("rsync");
    let log = env.home_path().join("rsync.log");
    std::fs::write(&rsync, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$VOWP_RSYNC_LOG\"\n").expect("fake rsync");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&rsync, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let target = "rsync://example.invalid/vowp";
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("VOWP_RSYNC_LOG", &log)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["backup", "--to", target])
        .output()
        .expect("run vowp backup");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let args = std::fs::read_to_string(&log).expect("rsync log");
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(lines[0], "-a", "{lines:?}");
    assert!(lines[1].ends_with('/'), "{lines:?}");
    assert_eq!(lines[2], target, "{lines:?}");
}

#[test]
fn backup_encryption_cli_runs_age_argv() {
    let env = common::TestEnv::new();
    let bin = tempfile::tempdir().expect("fake bin");
    let age = bin.path().join("age");
    let log = env.home_path().join("age.log");
    std::fs::write(
        &age,
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$VOWP_AGE_LOG"
out=""
src=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -r) shift 2 ;;
    --decrypt) shift ;;
    *) src="$1"; shift ;;
  esac
done
cp "$src" "$out"
"#,
    )
    .expect("fake age");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&age, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let destination = env.home_path().join("cli-encrypted");
    let destination_text = destination.display().to_string();
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("VOWP_AGE_LOG", &log)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args([
            "--json",
            "backup",
            "--to",
            &destination_text,
            "--encrypt",
            "age1testrecipient",
        ])
        .output()
        .expect("run vowp backup");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("backup json");
    assert_eq!(report["encrypted"], true);
    assert!(destination.join("global.db.age").is_file());
    assert!(!destination.join("global.db").exists());
    let age_args = std::fs::read_to_string(&log).expect("age log");
    assert!(age_args.contains("-r\nage1testrecipient\n"), "{age_args}");
    assert!(age_args.contains("-o\n"), "{age_args}");

    let restore = env.home_path().join("cli-encrypted-restored");
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("VOWP_AGE_LOG", &log)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args([
            "backup",
            "--restore",
            &destination_text,
            "--to-home",
            restore.to_str().expect("restore path"),
        ])
        .output()
        .expect("run vowp restore");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(restore.join("vowp.db").is_file());
}

#[test]
fn backup_project_database_with_open_wal() {
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    let paths = NodePaths::new(fixture.env.home_path());
    let writer = Store::open_with_project(&paths, &alpha).expect("project writer");
    for index in 0..500 {
        let id = JobId::parse(&format!("wal-job-{index:04}")).expect("job id");
        writer
            .insert_job(
                &alpha,
                &NewJob {
                    id,
                    title: format!("wal job {index}"),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: None,
                    needs: Vec::new(),
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: None,
                    priority: None,
                    loop_: None,
                    brief_text: None,
                },
                "test",
            )
            .expect("insert job");
    }
    let wal = fixture
        .env
        .home_path()
        .join("projects")
        .join(alpha.as_str())
        .join("project.db-wal");
    let wal_bytes = std::fs::metadata(&wal).expect("populated WAL").len();
    assert!(wal_bytes > 0, "the test must leave a populated WAL");

    let destination = fixture.env.home_path().join("wal-backup");
    fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup while the project WAL is open");
    drop(writer);

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("restore the WAL snapshot");
    let restored = Store::open_with_project(&NodePaths::new(restore_home.path()), &alpha).expect("restored project");
    assert_eq!(restored.list_jobs(&JobFilter::default()).expect("jobs").len(), 500);
}

#[test]
fn backup_counts_match_snapshot_with_concurrent_writes() {
    let fixture = BackupFixture::new();
    let paths = NodePaths::new(fixture.env.home_path());
    let seed = Store::open_global(&paths).expect("seed store");
    for index in 0..3000 {
        seed.upsert_session(&fixture.session(&format!("seed-{index}")))
            .expect("seed session");
    }
    drop(seed);

    let stop = Arc::new(AtomicBool::new(false));
    let writer_paths = paths.clone();
    let writer_stop = Arc::clone(&stop);
    let writer_clock = fixture.clock.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let store = Store::open_global(&writer_paths).expect("writer store");
        let mut written = 0_u64;
        while !writer_stop.load(Ordering::Relaxed) {
            let session = Session {
                id: SessionId::new(),
                project: None,
                job_id: None,
                role: None,
                title: format!("live-{written}"),
                parent_session_id: None,
                node: NodeName::parse("local").expect("node"),
                cwd: "/tmp".to_owned(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: writer_clock.now(),
                updated_at: writer_clock.now(),
            };
            store.upsert_session(&session).expect("writer session");
            written += 1;
            if written == 1 {
                started_tx.send(()).expect("writer started signal");
            }
        }
        written
    });
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("writer must commit before the backup starts");

    let destination = fixture.env.home_path().join("race-backup");
    let report = fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup during concurrent writes");
    stop.store(true, Ordering::SeqCst);
    let written = writer.join().expect("writer thread");
    assert!(written > 0, "the writer must commit while the backup runs");
    assert!(!report.files.is_empty());

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    let expected = manifest["row_counts"]["global"]["sessions"]
        .as_u64()
        .expect("manifest sessions");
    assert!(expected >= 3000, "{expected}");

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("the snapshot counts must match the snapshot files");
    let restored = Store::open_global(&NodePaths::new(restore_home.path())).expect("restored global");
    let restored_sessions = restored
        .list_sessions(&SessionFilter::default())
        .expect("restored sessions");
    assert_eq!(restored_sessions.len() as u64, expected);

    let live = fixture
        .store()
        .list_sessions(&SessionFilter::default())
        .expect("live sessions");
    assert!(
        live.len() as u64 > expected,
        "the live store must have rows the snapshot does not"
    );
}

#[test]
fn backup_rejects_force_without_restore() {
    let env = common::TestEnv::new();
    let (code, _stdout, stderr) = env.run(&["backup", "--force"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    let backups = env.home_path().join("backups");
    assert_eq!(backup_directories(&backups).len(), 0);
}

#[test]
fn legacy_backup_validates_encryption_before_writing() {
    let env = common::TestEnv::new();
    let path = env.home_path().join("empty-recipient.db");
    let path_text = path.display().to_string();
    let (code, _stdout, stderr) = env.run(&["backup", "--to", &path_text, "--encrypt", " "]);
    assert_eq!(code, 2, "{stderr}");
    assert!(!path.exists(), "plaintext backup must not be written");
    assert!(!Path::new(&format!("{path_text}.age")).exists());
}

#[test]
fn backup_skips_symlinked_session_directories() {
    use std::os::unix::fs::symlink;

    let fixture = BackupFixture::new();
    let session = SessionId::new();
    let outside = fixture.env.home_path().join("outside-session");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::write(outside.join("log.jsonl"), "{\"seq\":1}\n").expect("outside log");
    let sessions = fixture.env.home_path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    symlink(&outside, sessions.join(session.as_str())).expect("session symlink");

    let destination = fixture.env.home_path().join("symlink-backup");
    let report = fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("symlink not followed")),
        "{:?}",
        report.skipped
    );
    assert!(!destination.join("sessions").join(session.as_str()).exists());
}

#[test]
fn backup_offsite_alias_copies_the_default_backup() {
    let fixture = BackupFixture::new();
    let target = "rsync://backup.example/alias";
    let report = fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                offsite: Some(target.to_owned()),
                ..BackupOptions::default()
            },
        )
        .expect("offsite alias backup");
    assert_eq!(report.offsite.as_deref(), Some(target));
    assert!(Path::new(&report.directory).join("global.db").is_file());
    let rsync = fixture
        .runner
        .calls()
        .into_iter()
        .find(|call| call.argv.first().map(String::as_str) == Some("rsync"))
        .expect("rsync call");
    assert_eq!(rsync.argv[3], target);
    assert!(rsync.shell.is_none());
}

/// Stops the writer and disarms the wall-clock watchdog when the test leaves scope,
/// including while unwinding from a panic.
struct WriterGuard {
    stop: Arc<AtomicBool>,
    finished: Option<std::sync::mpsc::Sender<()>>,
}

impl WriterGuard {
    fn new(stop: Arc<AtomicBool>, finished: std::sync::mpsc::Sender<()>) -> Self {
        Self {
            stop,
            finished: Some(finished),
        }
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
    }
}

#[test]
fn backup_project_database_while_writer_checkpoints() {
    // A fixed budget keeps the writer from filling the runner's disk even if a backup
    // stalls: at most ~10 MiB of payload (10,000 x 1 KiB titles) and ~20 s of commits.
    const WRITER_MAX_COMMITS: u64 = 10_000;
    const WRITER_PAUSE: Duration = Duration::from_millis(2);
    const TITLE_BYTES: usize = 1024;
    const TEST_TIMEOUT: Duration = Duration::from_secs(60);

    let started_at = Instant::now();
    let fixture = BackupFixture::new();
    let alpha = fixture.env.config.project.name.clone();
    let paths = NodePaths::new(fixture.env.home_path());
    let seed = Store::open_with_project(&paths, &alpha).expect("seed project");
    for index in 0..500 {
        seed.insert_job(
            &alpha,
            &NewJob {
                id: JobId::parse(&format!("seed-{index:04}")).expect("job id"),
                title: "x".repeat(TITLE_BYTES),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: None,
                needs: Vec::new(),
                owns: Vec::new(),
                reads: Vec::new(),
                gates: None,
                priority: None,
                loop_: None,
                brief_text: None,
            },
            "seed",
        )
        .expect("seed job");
    }
    drop(seed);

    let stop = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(AtomicU64::new(0));
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = finished_rx.recv_timeout(TEST_TIMEOUT) {
            eprintln!(
                "backup_project_database_while_writer_checkpoints exceeded {TEST_TIMEOUT:?}; failing instead of hanging"
            );
            std::process::exit(101);
        }
    });
    let writer_guard = WriterGuard::new(Arc::clone(&stop), finished_tx);

    let writer_paths = paths.clone();
    let writer_project = alpha.clone();
    let writer_stop = Arc::clone(&stop);
    let writer_committed = Arc::clone(&committed);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let store = Store::open_with_project(&writer_paths, &writer_project).expect("writer project");
        let mut written = 0_u64;
        while written < WRITER_MAX_COMMITS && !writer_stop.load(Ordering::Relaxed) {
            store
                .insert_job(
                    &writer_project,
                    &NewJob {
                        id: JobId::parse(&format!("live-{written}")).expect("job id"),
                        title: "y".repeat(TITLE_BYTES),
                        kind: JobKind::Build,
                        role: Role::Worker,
                        profile: None,
                        needs: Vec::new(),
                        owns: Vec::new(),
                        reads: Vec::new(),
                        gates: None,
                        priority: None,
                        loop_: None,
                        brief_text: None,
                    },
                    "writer",
                )
                .expect("writer job");
            written += 1;
            writer_committed.store(written, Ordering::SeqCst);
            if written == 1 {
                started_tx.send(()).expect("writer started signal");
            }
            std::thread::sleep(WRITER_PAUSE);
        }
        written
    });
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("writer must commit before the backup starts");

    let mut last_backup = None;
    for round in 0..2 {
        let before = committed.load(Ordering::SeqCst);
        let destination = fixture.env.home_path().join(format!("writer-backup-{round}"));
        fixture
            .run_backup(
                &fixture.store(),
                BackupOptions {
                    to: Some(destination.display().to_string()),
                    ..BackupOptions::default()
                },
            )
            .expect("backup while a project writer checkpoints");
        let after = committed.load(Ordering::SeqCst);
        assert!(
            after > before,
            "the writer did not commit during backup round {round}: {before} before, {after} after"
        );
        last_backup = Some(destination);
    }
    stop.store(true, Ordering::SeqCst);
    let written = writer.join().expect("writer thread");
    assert!(written > 0, "the writer must commit while backups run");
    assert_eq!(
        written,
        committed.load(Ordering::SeqCst),
        "the writer count must match its last published progress"
    );

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(last_backup.as_ref().expect("backup path"), restore_home.path(), false)
        .expect("restore the last snapshot");
    let restored = Store::open_with_project(&NodePaths::new(restore_home.path()), &alpha).expect("restored project");
    restored.integrity_check().expect("restored integrity");
    let jobs = restored.list_jobs(&JobFilter::default()).expect("jobs");
    assert!(jobs.len() >= 500, "restored only {} jobs", jobs.len());

    let elapsed = started_at.elapsed();
    assert!(
        elapsed < TEST_TIMEOUT,
        "the test must finish within {TEST_TIMEOUT:?}; it took {elapsed:?}"
    );
    drop(writer_guard);
    watchdog.join().expect("watchdog thread");
}

#[test]
fn backup_skips_symlinked_blobs_and_capsules_roots() {
    use std::os::unix::fs::symlink;

    let fixture = BackupFixture::new();
    let session = SessionId::new();
    let session_dir = fixture.env.home_path().join("sessions").join(session.as_str());
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(session_dir.join("log.jsonl"), "{\"seq\":1}\n").expect("log");

    let secret_blobs = fixture.env.home_path().join("secret-blobs");
    std::fs::create_dir_all(&secret_blobs).expect("secret blobs");
    std::fs::write(secret_blobs.join("token"), "blob-secret").expect("blob secret");
    symlink(&secret_blobs, session_dir.join("blobs")).expect("blobs symlink");

    let secret_capsules = fixture.env.home_path().join("secret-capsules");
    std::fs::create_dir_all(&secret_capsules).expect("secret capsules");
    std::fs::write(secret_capsules.join("token"), "capsule-secret").expect("capsule secret");
    symlink(&secret_capsules, session_dir.join("capsules")).expect("capsules symlink");

    let destination = fixture.env.home_path().join("nested-symlink-backup");
    let report = fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    let backup_session = destination.join("sessions").join(session.as_str());
    assert!(!backup_session.join("blobs").exists());
    assert!(!backup_session.join("capsules").exists());
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("blobs") && entry.contains("symlink not followed")),
        "{:?}",
        report.skipped
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("capsules") && entry.contains("symlink not followed")),
        "{:?}",
        report.skipped
    );

    let restore_home = tempfile::tempdir().expect("restore home");
    fixture
        .run_restore(&destination, restore_home.path(), false)
        .expect("restore");
    assert!(
        !restore_home
            .path()
            .join("sessions")
            .join(session.as_str())
            .join("blobs")
            .join("token")
            .exists(),
        "no secret may be restored"
    );
}

#[test]
fn backup_skips_symlinked_sessions_root() {
    use std::os::unix::fs::symlink;

    let fixture = BackupFixture::new();
    let session = SessionId::new();
    let outside = fixture.env.home_path().join("outside-sessions");
    std::fs::create_dir_all(outside.join(session.as_str())).expect("outside session dir");
    std::fs::write(outside.join(session.as_str()).join("log.jsonl"), "{\"seq\":1}\n").expect("outside log");
    let sessions = fixture.env.home_path().join("sessions");
    std::fs::remove_dir_all(&sessions).expect("remove sessions dir");
    symlink(&outside, &sessions).expect("sessions symlink");

    let destination = fixture.env.home_path().join("root-symlink-backup");
    let report = fixture
        .run_backup(
            &fixture.store(),
            BackupOptions {
                to: Some(destination.display().to_string()),
                ..BackupOptions::default()
            },
        )
        .expect("backup");
    assert!(!destination.join("sessions").exists());
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("sessions") && entry.contains("symlink not followed")),
        "{:?}",
        report.skipped
    );
}

#[test]
fn backup_to_offsite_rotates_default_backups() {
    let fixture = BackupFixture::new();
    let store = fixture.store();
    let start = timestamp("2026-01-01T02:00:00Z");
    for day in 0..20 {
        fixture.clock.set(start.plus(Duration::from_secs(day * 24 * 60 * 60)));
        let report = fixture
            .run_backup(
                &store,
                BackupOptions {
                    to: Some("rsync://backup.example/rotated".to_owned()),
                    ..BackupOptions::default()
                },
            )
            .expect("offsite backup");
        assert_eq!(report.offsite.as_deref(), Some("rsync://backup.example/rotated"));
    }

    let names = backup_directories(&fixture.env.home_path().join("backups"));
    assert_eq!(names.len(), 9, "{names:?}");
    for day in [
        "20260104", "20260111", "20260114", "20260115", "20260116", "20260117", "20260118", "20260119", "20260120",
    ] {
        assert!(
            names.iter().any(|name| name.starts_with(day)),
            "missing {day} in {names:?}"
        );
    }
}
