//! `vowp graph` integration tests: golden renderings and the YAML round trip.

mod common;

use vowp_protocol::JobKind;

#[test]
fn graph_ascii_and_mermaid_golden() {
    let env = common::TestEnv::new();
    env.insert_job("a-job", "Alpha", JobKind::Build, &[]);
    env.insert_job("b-job", "Beta", JobKind::Build, &["a-job"]);

    let (code, stdout, stderr) = env.run(&["graph"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "L0 a-job [pending] Alpha\nL1 b-job [pending] Beta  <- a-job\n");

    let (code, stdout, stderr) = env.run(&["graph", "--mermaid"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        concat!(
            "graph TD\n",
            "    a_job[\"a-job (pending)\"]\n",
            "    b_job[\"b-job (pending)\"]\n",
            "    a_job --> b_job\n",
            "    classDef running fill:#fff4d6,stroke:#b7791f,color:#744210\n",
            "    classDef waiting fill:#e8f0fe,stroke:#3b6fb6,color:#1a3d6d\n",
            "    classDef failed fill:#fde8e8,stroke:#b42318,color:#7a1a12\n",
            "    classDef done fill:#e6f4ea,stroke:#2f7d4f,color:#1d4d31\n",
            "    class a_job waiting\n",
            "    class b_job waiting\n",
        )
    );
}

#[test]
fn graph_export_import_roundtrip() {
    let env = common::TestEnv::new();
    env.insert_job("z-parent", "Parent job", JobKind::Build, &[]);
    env.insert_job("b-middle", "Middle job", JobKind::Build, &["z-parent"]);
    env.insert_job("a-child", "Child job", JobKind::Verify, &["b-middle"]);
    let first_export = env.project_path().join(".vowp/graph-1.yaml");
    let second_export = env.project_path().join(".vowp/graph-2.yaml");

    let (code, _, stderr) = env.run(&["graph", "--export", first_export.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    let exported = std::fs::read_to_string(&first_export).expect("export file");
    assert!(exported.contains("version: 1"), "{exported}");
    assert!(exported.contains("    needs: [b-middle]"), "{exported}");
    // Export sorts by id, so both dependents are written before their dependency.
    let child_at = exported.find("- id: a-child").expect("child job");
    let middle_at = exported.find("- id: b-middle").expect("middle job");
    let parent_at = exported.find("- id: z-parent").expect("parent job");
    assert!(child_at < middle_at, "{exported}");
    assert!(middle_at < parent_at, "{exported}");

    let store = env.store();
    for id in ["a-child", "b-middle", "z-parent"] {
        store
            .delete_job(&vowp_protocol::JobId::parse(id).expect("slug"))
            .expect("delete pending job");
    }
    assert!(
        store
            .list_jobs(&vowp_store::JobFilter::default())
            .expect("jobs")
            .is_empty()
    );

    let (code, value, stderr) = env.run_json(&["graph", "--import", first_export.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["imported"].as_array().expect("imported").len(), 3);
    assert_eq!(value["conflicts"].as_array().expect("conflicts").len(), 0);

    let child = store
        .require_job(&vowp_protocol::JobId::parse("a-child").expect("slug"))
        .expect("imported child");
    assert_eq!(
        child.needs,
        vec![vowp_protocol::JobId::parse("b-middle").expect("slug")]
    );

    let (code, _, stderr) = env.run(&["graph", "--export", second_export.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&first_export).expect("first export"),
        std::fs::read_to_string(&second_export).expect("second export")
    );

    // Importing the same file again is a conflict, exit 4, and adds nothing.
    let (code, value, _) = env.run_json(&["graph", "--import", first_export.to_str().expect("path")]);
    assert_eq!(code, 4);
    assert_eq!(value["imported"].as_array().expect("imported").len(), 0);
    assert_eq!(value["conflicts"].as_array().expect("conflicts").len(), 3);
}

/// Returns the `(id, state)` pairs of a serialised job array.
fn ids_and_states(jobs: &[serde_json::Value]) -> Vec<(String, String)> {
    jobs.iter()
        .map(|job| {
            (
                job["id"].as_str().expect("job id").to_owned(),
                job["state"].as_str().expect("job state").to_owned(),
            )
        })
        .collect()
}

#[test]
fn graph_json_matches_job_ls_and_keeps_needs_edge() {
    let env = common::TestEnv::new();
    env.insert_job("a-job", "Alpha", JobKind::Build, &[]);
    env.insert_job("b-job", "Beta", JobKind::Build, &["a-job"]);

    let (code, graph, stderr) = env.run_json(&["graph"]);
    assert_eq!(code, 0, "{stderr}");
    let jobs = graph["jobs"].as_array().expect("jobs array");
    let edges = graph["edges"].as_array().expect("edges array");

    // Jobs are ordered by id and serialised like the `job ls --json` objects.
    assert_eq!(
        jobs.iter()
            .map(|job| job["id"].as_str().expect("job id"))
            .collect::<Vec<_>>(),
        ["a-job", "b-job"]
    );
    // Edges keep the store/graph order: the stored needs edge for b-job.
    assert_eq!(edges.len(), 1, "{graph}");
    assert_eq!(edges[0]["src"], "b-job");
    assert_eq!(edges[0]["dst"], "a-job");
    assert_eq!(edges[0]["kind"], "needs");

    let (code, listed, stderr) = env.run_json(&["job", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = listed.as_array().expect("job ls array");
    let mut graph_jobs = ids_and_states(jobs);
    graph_jobs.sort();
    let mut listed_jobs = ids_and_states(listed);
    listed_jobs.sort();
    assert_eq!(graph_jobs.len(), 2);
    assert_eq!(graph_jobs, listed_jobs);
    assert_eq!(
        graph_jobs,
        vec![
            ("a-job".to_owned(), "pending".to_owned()),
            ("b-job".to_owned(), "pending".to_owned()),
        ]
    );
}

#[test]
fn graph_json_mermaid_prints_only_mermaid() {
    let env = common::TestEnv::new();
    env.insert_job("a-job", "Alpha", JobKind::Build, &[]);

    let (code, stdout, stderr) = env.run(&["graph", "--json", "--mermaid"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        concat!(
            "graph TD\n",
            "    a_job[\"a-job (pending)\"]\n",
            "    classDef running fill:#fff4d6,stroke:#b7791f,color:#744210\n",
            "    classDef waiting fill:#e8f0fe,stroke:#3b6fb6,color:#1a3d6d\n",
            "    classDef failed fill:#fde8e8,stroke:#b42318,color:#7a1a12\n",
            "    classDef done fill:#e6f4ea,stroke:#2f7d4f,color:#1d4d31\n",
            "    class a_job waiting\n",
        )
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_err(),
        "mermaid output must not be JSON: {stdout:?}"
    );
}

#[test]
fn graph_mermaid_renders_every_edge_kind() {
    let env = common::TestEnv::new();
    env.insert_job("base", "Base", JobKind::Build, &[]);
    env.insert_job("child", "Child", JobKind::Build, &["base"]);
    env.insert_job("x", "X", JobKind::Build, &[]);
    env.insert_job("y", "Y", JobKind::Build, &[]);
    let store = env.store();
    let child = vowp_protocol::JobId::parse("child").expect("slug");
    let base = vowp_protocol::JobId::parse("base").expect("slug");
    for kind in [
        vowp_protocol::EdgeKind::Verifies,
        vowp_protocol::EdgeKind::Fixes,
        vowp_protocol::EdgeKind::Escalates,
    ] {
        store
            .add_edge(&vowp_protocol::Edge {
                src: child.clone(),
                dst: base.clone(),
                kind,
            })
            .expect("edge");
    }
    store
        .add_edge(&vowp_protocol::Edge {
            src: vowp_protocol::JobId::parse("x").expect("slug"),
            dst: vowp_protocol::JobId::parse("y").expect("slug"),
            kind: vowp_protocol::EdgeKind::Blocks,
        })
        .expect("blocks edge");
    let (code, stdout, stderr) = env.run(&["graph", "--mermaid"]);
    assert_eq!(code, 0, "{stderr}");
    for expected in [
        "base --> child",
        "child -.->|verifies| base",
        "child ==>|fixes| base",
        "child -.->|escalates| base",
        "x -->|blocks| y",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?} in {stdout}");
    }
}
