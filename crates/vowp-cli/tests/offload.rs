//! `vowp offload` and `vowp job add --placement` integration tests.
//!
//! Every fixture is a temporary home and project; the tests never touch the real `~/.vowp`,
//! tmux, the network or a harness. Spec: `docs/design/v0.4/local-orchestrator.md` §7 and
//! §12.1.

mod common;

use serde_json::json;
use vowp_protocol::{
    AssignmentPayload, Executor, JobId, JobKind, JobState, Node, NodeName, NodeStatus, PathRef, Placement, ProjectName,
    RunId, RunPurpose, SecretName, SessionId, VowpConfig,
};
use vowp_store::StateChange;
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState};

use common::TestEnv;

/// Writes `config` as the project's `vowp.toml`.
fn write_config(env: &TestEnv, config: &VowpConfig) {
    std::fs::write(
        env.project_path().join("vowp.toml"),
        toml::to_string_pretty(config).expect("config serialises"),
    )
    .expect("write vowp.toml");
}

/// Enables offload and sets the project placement to `any`.
fn enabled_config(env: &TestEnv) -> VowpConfig {
    let mut config = env.config.clone();
    config.offload.enabled = true;
    config.offload.default = Placement::Home;
    config.offload.projects.insert(
        config.project.name.clone(),
        vowp_protocol::OffloadProjectConfig {
            placement: Some(Placement::Any),
            ..vowp_protocol::OffloadProjectConfig::default()
        },
    );
    config
}

#[test]
fn job_add_refuses_node_placement_when_offload_is_disabled() {
    let env = TestEnv::new();
    assert!(!env.config.offload.enabled, "the default policy must be off");

    let (code, stdout, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "pinned",
        "--title",
        "Pinned",
        "--kind",
        "build",
        "--placement",
        "node:mac-studio",
    ]);
    assert_eq!(
        code, 5,
        "a refused node placement is exit 5; stdout: {stdout}; stderr: {stderr}"
    );
    assert!(stderr.contains("offload is disabled"), "{stderr}");

    let store = env.store();
    let id = JobId::parse("pinned").expect("job slug");
    assert!(
        store.job(&id).expect("lookup").is_none(),
        "the job row must not exist after a refused placement"
    );
    assert!(
        store.job_placement(&id).expect("placement lookup").is_none(),
        "no placement row may be written"
    );
    assert!(!env.brief_path("pinned").exists(), "no brief may be written");
}

#[test]
fn job_add_persists_any_placement_and_job_show_reads_it() {
    let env = TestEnv::new();
    let (code, added, stderr) = env.run_json(&[
        "job",
        "add",
        "--id",
        "offload-me",
        "--title",
        "Offload me",
        "--kind",
        "build",
        "--placement",
        "any",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(added["placement"], "any");

    let store = env.store();
    let id = JobId::parse("offload-me").expect("job slug");
    assert_eq!(
        store.job_placement(&id).expect("placement lookup"),
        Some(Placement::Any)
    );

    let (code, shown, stderr) = env.run_json(&["job", "show", "offload-me"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(shown["id"], "offload-me");
    assert_eq!(shown["placement"], "any");

    let (code, human, stderr) = env.run(&["job", "show", "offload-me"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("  placement: any"), "{human}");

    // `job ls` keeps its v0.2 table: placement is not a column.
    let (code, listing, stderr) = env.run(&["job", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!listing.contains("PLACEMENT"), "{listing}");
}

#[test]
fn offload_policy_json_is_the_effective_policy() {
    let env = TestEnv::new();
    let mut config = env.config.clone();
    config.offload.enabled = true;
    config.offload.default = Placement::Any;
    config.offload.max_remote_runs = 3;
    config.offload.grace_secs = 120;
    config.offload.projects.insert(
        config.project.name.clone(),
        vowp_protocol::OffloadProjectConfig {
            placement: Some(Placement::Any),
            allow_secrets: vec![SecretName::parse("CARGO_REGISTRY_TOKEN").expect("secret name")],
            ..vowp_protocol::OffloadProjectConfig::default()
        },
    );
    config.offload.nodes.insert(
        NodeName::parse("mac-studio").expect("node slug"),
        vowp_protocol::OffloadNodeConfig {
            max_workers: Some(2),
            allow_battery: true,
            labels: vec!["darwin-arm64".to_owned(), "xcode".to_owned()],
        },
    );
    write_config(&env, &config);

    let (code, value, stderr) = env.run_json(&["offload", "policy"]);
    assert_eq!(code, 0, "stderr: {stderr}; json: {value}");
    assert_eq!(value["enabled"], true);
    assert_eq!(value["default"], "any");
    assert_eq!(value["max_remote_runs"], 3);
    assert_eq!(value["grace_secs"], 120);
    assert_eq!(value["result_transport"], "push");
    assert_eq!(value["min_disk_bytes"], 5_368_709_120_u64);

    let projects = value["projects"].as_array().expect("projects array");
    let row = projects
        .iter()
        .find(|row| row["project"] == "test")
        .expect("the selected project must have a row");
    assert_eq!(row["placement"], "any");
    assert_eq!(row["home_only"], false);
    assert_eq!(row["allow_kinds"], json!(["build", "test", "docs", "fix", "verify"]));
    assert_eq!(row["deny_kinds"], json!(["merge", "promote", "release"]));
    assert_eq!(row["allow_secrets"], json!(["CARGO_REGISTRY_TOKEN"]));

    let nodes = value["nodes"].as_array().expect("nodes array");
    let row = nodes
        .iter()
        .find(|row| row["node"] == "mac-studio")
        .expect("the configured node must have a row");
    assert_eq!(row["max_workers"], 2);
    assert_eq!(row["allow_battery"], true);
    assert_eq!(row["labels"], json!(["darwin-arm64", "xcode"]));

    // An explicit global `--project <name>` restricts the document to that project.
    let mut io = vowp_cli::BufferIo::new("");
    let code = vowp_cli::run(
        vec![
            "vowp".to_owned(),
            "--json".to_owned(),
            "--home".to_owned(),
            env.home_path().display().to_string(),
            "--project".to_owned(),
            "test".to_owned(),
            "offload".to_owned(),
            "policy".to_owned(),
        ],
        &mut io,
    );
    let stdout = io.stdout();
    let restricted: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("not JSON ({error}): {stdout:?}"));
    assert_eq!(code, 0, "stderr: {}; json: {restricted}", io.stderr());
    let projects = restricted["projects"].as_array().expect("projects array");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["project"], "test");

    let (code, human, stderr) = env.run(&["offload", "policy"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("offload: enabled"), "{human}");
    assert!(human.contains("mac-studio"), "{human}");
}

#[test]
fn offload_policy_restricts_rows_for_an_explicit_project_selection() {
    let env = TestEnv::new();
    let mut config = env.config.clone();
    config.offload.projects.insert(
        config.project.name.clone(),
        vowp_protocol::OffloadProjectConfig {
            placement: Some(Placement::Any),
            ..vowp_protocol::OffloadProjectConfig::default()
        },
    );
    config.offload.projects.insert(
        ProjectName::parse("beta").expect("project slug"),
        vowp_protocol::OffloadProjectConfig::default(),
    );
    write_config(&env, &config);

    // Run from the selected checkout with `--project <name>`: the explicit selection must
    // restrict the rows even when the cwd and the selected root are the same directory.
    let home = env.home_path().display().to_string();
    let name = env.config.project.name.to_string();
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(env.project_path())
        .args(["--home", &home, "--json", "--project", &name, "offload", "policy"])
        .output()
        .expect("run vowp");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("policy json");
    let projects = value["projects"].as_array().expect("projects array");
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(projects[0]["project"], name);
}

#[test]
fn offload_status_uses_the_effective_placement_without_a_job_row() {
    let env = TestEnv::new();
    let config = enabled_config(&env);
    write_config(&env, &config);

    let (code, added, stderr) = env.run_json(&[
        "job", "add", "--id", "implicit", "--title", "Implicit", "--kind", "build",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        added.get("placement").is_none(),
        "the job must not get a placement row: {added}"
    );

    let store = env.store();
    let id = JobId::parse("implicit").expect("job slug");
    assert!(store.job_placement(&id).expect("placement lookup").is_none());
    store
        .set_job_state(
            &id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready");

    let (code, value, stderr) = env.run_json(&["offload", "status"]);
    assert_eq!(code, 0, "{stderr}");
    let ready = value["ready"].as_array().expect("ready array");
    assert_eq!(ready.len(), 1, "{ready:?}");
    assert_eq!(ready[0]["job"], "implicit");
    assert_eq!(
        ready[0]["placement"], "any",
        "the project default must supply the effective placement: {ready:?}"
    );
    assert_eq!(ready[0]["waiting_reason"], serde_json::Value::Null);
    assert_eq!(ready[0]["targets"], json!(["home"]));
    assert_eq!(ready[0]["detail"], "eligible: home");
}

#[test]
fn offload_status_keeps_the_home_target_for_an_explicit_any_in_a_home_only_project() {
    let env = TestEnv::new();
    let mut config = env.config.clone();
    config.offload.enabled = true;
    config.offload.default = Placement::Home;
    let name = config.project.name.clone();
    config.offload.projects.insert(
        name.clone(),
        vowp_protocol::OffloadProjectConfig {
            home_only: true,
            ..vowp_protocol::OffloadProjectConfig::default()
        },
    );
    write_config(&env, &config);

    let store = env.store();
    let job = env.insert_job("explicit-any", "Explicit any", JobKind::Build, &[]);
    store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready");
    store
        .set_job_placement(&job.id, Placement::Any, store.clock().now())
        .expect("placement");

    let (code, value, stderr) = env.run_json(&["offload", "status"]);
    assert_eq!(code, 0, "{stderr}");
    let ready = value["ready"].as_array().expect("ready array");
    assert_eq!(ready.len(), 1, "{ready:?}");
    assert_eq!(ready[0]["job"], "explicit-any");
    assert_eq!(ready[0]["placement"], "any");
    assert_eq!(
        ready[0]["waiting_reason"],
        serde_json::Value::Null,
        "a home_only project still runs the job at home: {ready:?}"
    );
    assert_eq!(ready[0]["targets"], json!(["home"]));
    assert_eq!(ready[0]["detail"], "eligible: home");
}

#[test]
fn offload_status_lists_nodes_assignments_and_waiting_reasons() {
    let env = TestEnv::new();
    let config = enabled_config(&env);
    write_config(&env, &config);

    let store = env.store();
    let now = store.clock().now();
    let mac = NodeName::parse("mac-studio").expect("node slug");
    store
        .upsert_node(&Node {
            name: mac.clone(),
            ssh_target: Some("mac-studio.example".to_owned()),
            is_local: false,
            capabilities: json!({}),
            last_seen_at: Some(now),
            created_at: now,
        })
        .expect("node row");
    store
        .set_node_status(&mac, NodeStatus::Revoked, "revoked by test", now)
        .expect("revoked node");

    let job = env.insert_job("pinned-job", "Pinned job", JobKind::Build, &[]);
    store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready");
    store
        .set_job_placement(
            &job.id,
            Placement::Node {
                node: mac.clone(),
                fallback: None,
            },
            now,
        )
        .expect("placement");

    let run = RunId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("run ulid");
    let session = SessionId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("session ulid");
    let payload = AssignmentPayload {
        assignment_id: "assignment-1".to_owned(),
        run_id: run.clone(),
        session_id: session.clone(),
        project: config.project.name.clone(),
        job: job.id.clone(),
        executor: Executor::parse("opencode:example/model").expect("executor"),
        purpose: RunPurpose::Start,
        prompt: "do the work".to_owned(),
        cwd_ref: PathRef::Project {
            project: config.project.name.clone(),
        },
        base_commit: "0".repeat(40),
        remote: "origin".to_owned(),
        remote_url: "https://example.invalid/owner/repo.git".to_owned(),
        result_transport: vowp_protocol::ResultTransport::Push,
        env_names: Vec::new(),
        secret_names: Vec::new(),
        epoch: 1,
        deadline: None,
        spool_cap_bytes: 1024,
    };
    store
        .insert_assignment(&AgentAssignment {
            id: "assignment-1".to_owned(),
            run_id: run,
            session_id: session,
            project: config.project.name.clone(),
            node: mac.clone(),
            epoch: 1,
            state: AgentAssignmentState::Running,
            payload,
            created_at: now,
            claimed_at: Some(now),
            expires_at: None,
            detail: "running".to_owned(),
        })
        .expect("assignment");

    let (code, value, stderr) = env.run_json(&["offload", "status"]);
    assert_eq!(code, 0, "{stderr}");

    let nodes = value["nodes"].as_array().expect("nodes array");
    let row = nodes
        .iter()
        .find(|row| row["name"] == "mac-studio")
        .expect("the revoked node must be listed");
    assert_eq!(row["status"], "revoked");
    assert_eq!(row["presence"], "revoked");
    assert_eq!(row["last_error"], "revoked by test");

    let assignments = value["assignments"].as_array().expect("assignments array");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0]["id"], "assignment-1");
    assert_eq!(assignments[0]["node"], "mac-studio");
    assert_eq!(assignments[0]["state"], "running");
    assert!(
        assignments[0].get("payload").is_none(),
        "the token-free payload (with a prompt) must not be echoed"
    );

    let ready = value["ready"].as_array().expect("ready array");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0]["job"], "pinned-job");
    assert_eq!(ready[0]["placement"], "node:mac-studio");
    assert_eq!(ready[0]["waiting_reason"], "node_revoked");

    let (code, human, stderr) = env.run(&["offload", "status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("mac-studio"), "{human}");
    assert!(human.contains("assignment-1"), "{human}");
    assert!(human.contains("node_revoked"), "{human}");
    assert!(
        !human.contains(&env.home_path().display().to_string()),
        "status must not print the home path: {human}"
    );
    assert!(
        !human.contains(&env.project_path().display().to_string()),
        "status must not print the project path: {human}"
    );
}

#[test]
fn offload_policy_reports_the_bundle_result_transport() {
    let env = TestEnv::new();
    let mut config = env.config.clone();
    config.offload.result_transport = vowp_protocol::ResultTransport::Bundle;
    write_config(&env, &config);

    let (code, value, stderr) = env.run_json(&["offload", "policy"]);
    assert_eq!(code, 0, "bundle must be a legal result transport: {stderr}");
    assert_eq!(value["result_transport"], "bundle", "{value}");

    let (code, human, stderr) = env.run(&["offload", "policy"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("result_transport: bundle"), "{human}");
}
