//! `vowp reindex`, `backup`, `schema`, `version`, and the CLI-wide listing and stub tests.

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
            // A command may still be an explicit stub only while its own handler source
            // contains the stub. Once it is implemented, a NotImplemented return fails.
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
