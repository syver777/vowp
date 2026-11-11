//! `snapshot.json` and the fleet snapshot cards.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §5 and the snapshot tests in §15.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::fleet;
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    ActiveState, Clock, Cost, Evidence, Executor, FakeClock, Gate, GateResult, HarnessKind, InboxItem, InboxKind,
    InboxState, Job, JobId, JobKind, JobState, LedgerEntry, LedgerKind, Limit, LimitRowKind, Loop, LoopKind, NewJob,
    Node, NodeName, ParkedReason, Project, ProjectName, ProjectState, ProviderConfig, ReportGate, Role, Run, RunId,
    RunPurpose, RunRole, SNAPSHOT_FORMAT, SNAPSHOT_VERSION, SecretName, Session, SessionId, SessionRole, SessionState,
    Snapshot, TargetMode, Timestamp, Tokens,
};
use vowp_store::{NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

/// A hermetic, file-backed home with two registered projects.
struct Fixture {
    _home: TempDir,
    _alpha_dir: TempDir,
    _beta_dir: TempDir,
    paths: NodePaths,
    deps: Deps,
    store: Store,
    clock: FakeClock,
    alpha: ProjectName,
    beta: ProjectName,
}

impl Fixture {
    fn new() -> Self {
        let home = tempdir().expect("home");
        let alpha_dir = tempdir().expect("alpha dir");
        let beta_dir = tempdir().expect("beta dir");
        let paths = NodePaths::new(home.path());
        let clock = FakeClock::at(timestamp("2026-11-10T12:00:00Z"));
        let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock.clone()));
        let alpha = ProjectName::parse("alpha").expect("alpha");
        let beta = ProjectName::parse("beta").expect("beta");
        let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
        config.project.name = alpha.clone();
        config.project.path = Some(alpha_dir.path().display().to_string());
        let now = clock.now();
        store
            .upsert_node(&Node {
                name: NodeName::parse("local").expect("node"),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: Some(now),
                created_at: now,
            })
            .expect("node");
        for (name, dir, project_config) in [(&alpha, alpha_dir.path(), &config), (&beta, beta_dir.path(), &config)] {
            let mut project_config = (*project_config).clone();
            project_config.project.name = name.clone();
            project_config.project.path = Some(dir.display().to_string());
            store
                .upsert_project(&Project {
                    name: name.clone(),
                    path: dir.display().to_string(),
                    remote_url: None,
                    default_branch: "main".to_owned(),
                    config: serde_json::to_value(&project_config).expect("config json"),
                    created_at: now,
                    updated_at: now,
                })
                .expect("project");
        }
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: Arc::new(vowp_protocol::MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(FakeGit::new()),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        Self {
            _home: home,
            _alpha_dir: alpha_dir,
            _beta_dir: beta_dir,
            paths,
            deps,
            store,
            clock,
            alpha,
            beta,
        }
    }

    fn store_for(&self, project: &ProjectName) -> Store {
        Store::open_with_project(&self.paths, project)
            .expect("project store")
            .with_clock(Arc::new(self.clock.clone()))
    }

    fn job(&self, project: &ProjectName, id: &str, priority: i32) -> Job {
        self.job_with_title(project, id, &format!("job {id}"), priority)
    }

    fn job_with_title(&self, project: &ProjectName, id: &str, title: &str, priority: i32) -> Job {
        let store = self.store_for(project);
        store
            .insert_job(
                project,
                &NewJob {
                    id: JobId::parse(id).expect("job id"),
                    title: title.to_owned(),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: None,
                    needs: Vec::new(),
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: None,
                    priority: Some(priority),
                    loop_: None,
                    brief_text: None,
                },
                "test",
            )
            .expect("insert job")
    }

    fn set_state(&self, project: &ProjectName, job: &Job, state: JobState, change: StateChange) {
        self.store_for(project)
            .set_job_state(&job.id, state, &change)
            .expect("state");
    }

    fn snapshot(&self) -> Snapshot {
        fleet::build_snapshot(&self.deps, &self.store, self.clock.now()).expect("snapshot")
    }
}

/// Parses a timestamp, failing the test on bad input.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

/// Builds a small token count.
fn tokens(input: u64, output: u64) -> Tokens {
    Tokens {
        input,
        output,
        ..Tokens::default()
    }
}

#[test]
fn snapshot_json_validates_against_exported_schema() {
    let fixture = Fixture::new();
    fixture.job(&fixture.alpha, "a01", 50);
    let snapshot = fixture.snapshot();
    let value = serde_json::to_value(&snapshot).expect("serialise snapshot");
    assert_eq!(value["schema"], SNAPSHOT_FORMAT);
    assert_eq!(value["version"], SNAPSHOT_VERSION);

    let schema = vowp_protocol::schema::schema_for_name("Snapshot").expect("Snapshot schema");
    assert!(
        schema["$defs"].get("ProjectSnapshot").is_some(),
        "exported Snapshot schema must define ProjectSnapshot: {schema}"
    );
    validate_schema(&value, &schema).expect("the snapshot validates against the exported schema");

    // The checker must reject a type mismatch, otherwise a schema drift could pass silently.
    let mut wrong_scalar = value.clone();
    wrong_scalar["sequence"] = serde_json::json!("not-an-integer");
    let error = validate_schema(&wrong_scalar, &schema).expect_err("a string sequence must fail");
    assert!(error.contains("sequence"), "the error names the field: {error}");

    let mut wrong_nested = value.clone();
    wrong_nested["supervisor"]["running"] = serde_json::json!("yes");
    let error = validate_schema(&wrong_nested, &schema).expect_err("a string running flag must fail");
    assert!(error.contains("running"), "the error names the nested field: {error}");

    let decoded: Snapshot = serde_json::from_value(value).expect("schema-valid document decodes");
    assert_eq!(decoded, snapshot);
}

/// Validates `value` against a schemars-generated JSON Schema document.
///
/// This is deliberately a small checker for the keywords schemars emits: `$ref`, `type`,
/// `enum`/`const`, `anyOf`/`oneOf`/`allOf`, object `properties`/`required`/
/// `additionalProperties`, array `items`/`prefixItems`, and numeric `minimum`/`maximum`.
/// It is test-only and does not add a runtime dependency.
fn validate_schema(value: &serde_json::Value, schema: &serde_json::Value) -> Result<(), String> {
    validate_at(value, schema, schema, "$")
}

/// Validates one value at one schema node, resolving `$ref` against the document root.
fn validate_at(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    let object = match schema {
        serde_json::Value::Bool(true) => return Ok(()),
        serde_json::Value::Bool(false) => return Err(format!("{path}: schema rejects every value")),
        serde_json::Value::Object(object) => object,
        other => return Err(format!("{path}: unsupported schema node {other}")),
    };

    if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str) {
        let target = resolve_ref(root, reference)?;
        return validate_at(value, target, root, path);
    }

    if let Some(constant) = object.get("const")
        && value != constant
    {
        return Err(format!("{path}: expected constant {constant}, got {value}"));
    }

    if let Some(options) = object.get("enum").and_then(serde_json::Value::as_array)
        && !options.iter().any(|option| option == value)
    {
        return Err(format!("{path}: {value} is not one of {options:?}"));
    }

    if let Some(expected) = object.get("type") {
        let matches = match expected {
            serde_json::Value::String(name) => json_type_matches(value, name),
            serde_json::Value::Array(names) => names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|name| json_type_matches(value, name)),
            other => return Err(format!("{path}: unsupported schema type {other}")),
        };
        if !matches {
            return Err(format!("{path}: expected type {expected}, got {value}"));
        }
    }

    if let Some(options) = object.get("allOf").and_then(serde_json::Value::as_array) {
        for subschema in options {
            validate_at(value, subschema, root, path)?;
        }
    }
    if let Some(options) = object.get("anyOf").and_then(serde_json::Value::as_array) {
        let first_error = options
            .iter()
            .find_map(|subschema| validate_at(value, subschema, root, path).err())
            .unwrap_or_else(|| format!("{path}: value {value} matches none of the anyOf branches"));
        if !options
            .iter()
            .any(|subschema| validate_at(value, subschema, root, path).is_ok())
        {
            return Err(first_error);
        }
    }
    if let Some(options) = object.get("oneOf").and_then(serde_json::Value::as_array) {
        let matches = options
            .iter()
            .filter(|subschema| validate_at(value, subschema, root, path).is_ok())
            .count();
        if matches != 1 {
            return Err(format!(
                "{path}: expected exactly one oneOf branch to match, {matches} did"
            ));
        }
    }

    if let serde_json::Value::Object(map) = value {
        if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
            for name in required {
                let name = name
                    .as_str()
                    .ok_or_else(|| format!("{path}: required entry is not a string: {name}"))?;
                if !map.contains_key(name) {
                    return Err(format!("{path}: required property {name:?} is missing"));
                }
            }
        }
        let properties = object.get("properties").and_then(serde_json::Value::as_object);
        if let Some(properties) = properties {
            for (name, subschema) in properties {
                if let Some(field) = map.get(name) {
                    validate_at(field, subschema, root, &format!("{path}.{name}"))?;
                }
            }
        }
        match object.get("additionalProperties") {
            Some(serde_json::Value::Bool(false)) => {
                for name in map.keys() {
                    if !properties.is_some_and(|properties| properties.contains_key(name)) {
                        return Err(format!("{path}: unknown property {name:?}"));
                    }
                }
            }
            Some(subschema @ (serde_json::Value::Bool(true) | serde_json::Value::Object(_))) => {
                for (name, field) in map {
                    if !properties.is_some_and(|properties| properties.contains_key(name)) {
                        validate_at(field, subschema, root, &format!("{path}.{name}"))?;
                    }
                }
            }
            _ => {}
        }
    }

    if let serde_json::Value::Array(items) = value {
        let prefix = object.get("prefixItems").and_then(serde_json::Value::as_array);
        if let Some(prefix) = prefix {
            for (index, (item, subschema)) in items.iter().zip(prefix).enumerate() {
                validate_at(item, subschema, root, &format!("{path}[{index}]"))?;
            }
        }
        if let Some(items_schema) = object.get("items") {
            let start = prefix.map(Vec::len).unwrap_or(0);
            for (index, item) in items.iter().enumerate().skip(start) {
                validate_at(item, items_schema, root, &format!("{path}[{index}]"))?;
            }
        }
    }

    if let Some(minimum) = object.get("minimum").and_then(serde_json::Value::as_f64)
        && let Some(number) = value.as_f64()
        && number < minimum
    {
        return Err(format!("{path}: {number} is below minimum {minimum}"));
    }
    if let Some(maximum) = object.get("maximum").and_then(serde_json::Value::as_f64)
        && let Some(number) = value.as_f64()
        && number > maximum
    {
        return Err(format!("{path}: {number} is above maximum {maximum}"));
    }

    Ok(())
}

/// Returns whether a JSON value has the named JSON-Schema primitive type.
fn json_type_matches(value: &serde_json::Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// Resolves one local `$ref` (`#/$defs/Name`) against the schema root.
fn resolve_ref<'a>(root: &'a serde_json::Value, reference: &str) -> Result<&'a serde_json::Value, String> {
    let pointer = reference.strip_prefix('#').unwrap_or(reference);
    if pointer.is_empty() {
        return Ok(root);
    }
    if !pointer.starts_with('/') {
        return Err(format!("unsupported non-local schema reference {reference:?}"));
    }
    root.pointer(pointer)
        .ok_or_else(|| format!("unresolved schema reference {reference:?}"))
}

#[test]
fn snapshot_cards_cover_every_project_and_state() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();

    let done = fixture.job(&fixture.alpha, "a01-done", 50);
    fixture.set_state(
        &fixture.alpha,
        &done,
        JobState::Done,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    let running = fixture.job(&fixture.alpha, "a02-run", 70);
    fixture.set_state(
        &fixture.alpha,
        &running,
        JobState::Running,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    let ready = fixture.job(&fixture.alpha, "a03-ready", 90);
    fixture.set_state(
        &fixture.alpha,
        &ready,
        JobState::Ready,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    fixture
        .store_for(&fixture.alpha)
        .set_job_note(&ready.id, "waiting: RAM 1.2 GB short")
        .expect("waiting note");
    let parked = fixture.job(&fixture.alpha, "a04-parked", 10);
    fixture.set_state(
        &fixture.alpha,
        &parked,
        JobState::Parked,
        StateChange {
            note: Some("out of credits".to_owned()),
            parked_reason: Some(ParkedReason::Credits),
        },
    );
    let failed = fixture.job(&fixture.alpha, "a05-failed", 10);
    fixture.set_state(
        &fixture.alpha,
        &failed,
        JobState::Failed,
        StateChange {
            note: Some("compile failed".to_owned()),
            parked_reason: None,
        },
    );
    fixture.job(&fixture.beta, "b01-pending", 50);

    let alpha_store = fixture.store_for(&fixture.alpha);
    let run = Run {
        id: RunId::new(),
        job_id: Some(running.id.clone()),
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").expect("node"),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at: now.minus(Duration::from_secs(120)),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    alpha_store.insert_run(&run).expect("run");

    alpha_store
        .insert_gate_result(&GateResult {
            id: None,
            job_id: ready.id.clone(),
            run_id: None,
            attempt: 0,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: false,
            evidence: Evidence::Report {
                front_matter: None,
                missing_fields: vec!["status".to_owned()],
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 1,
            ts: now.minus(Duration::from_secs(30)),
        })
        .expect("gate result");

    alpha_store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: now.minus(Duration::from_secs(3600)),
            kind: LedgerKind::Merge,
            project: Some(fixture.alpha.clone()),
            job_id: Some(done.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: "merged a01-done".to_owned(),
            detail: serde_json::json!({
                "main_after": "abc1234",
                "branch": "job/a01-done",
            }),
        })
        .expect("merge row");
    alpha_store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: now.minus(Duration::from_secs(60)),
            kind: LedgerKind::Cost,
            project: Some(fixture.alpha.clone()),
            job_id: Some(running.id.clone()),
            run_id: Some(run.id.clone()),
            session_id: None,
            role: Some(RunRole::Worker),
            provider: Some("deepseek".to_owned()),
            harness: Some(HarnessKind::Opencode),
            model: Some("deepseek-flash".to_owned()),
            cost: Some(Cost::receipt(1.25, tokens(1_000, 100))),
            summary: "cost".to_owned(),
            detail: serde_json::json!({}),
        })
        .expect("cost row");
    alpha_store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: None,
            kind: InboxKind::Question,
            title: "Which branch?".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(10)),
            answered_at: None,
        })
        .expect("project inbox");
    fixture
        .store
        .insert_node_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: None,
            job_id: None,
            kind: InboxKind::Notice,
            title: "Node notice".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(20)),
            answered_at: None,
        })
        .expect("node inbox");
    fixture
        .store
        .upsert_session(&Session {
            id: SessionId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: Some(running.id.clone()),
            role: Some(SessionRole::Worker),
            title: "worker session".to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/srv/vowp".to_owned(),
            state: SessionState::Running,
            executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now.minus(Duration::from_secs(300)),
            updated_at: now.minus(Duration::from_secs(5)),
        })
        .expect("session");

    let snapshot = fixture.snapshot();
    let names: Vec<&str> = snapshot.projects.iter().map(|project| project.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"], "every registered project has a card");

    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.jobs_by_state.get("running"), Some(&1));
    assert_eq!(alpha.jobs_by_state.get("ready"), Some(&1));
    assert_eq!(alpha.jobs_by_state.get("parked"), Some(&1));
    assert_eq!(alpha.jobs_by_state.get("failed"), Some(&1));

    assert_eq!(alpha.running.len(), 1);
    let card = &alpha.running[0];
    assert_eq!(card.job, running.id);
    assert_eq!(card.executor.as_deref(), Some("opencode:deepseek/deepseek-flash"));
    assert_eq!(card.node.as_str(), "local");
    assert_eq!(card.age_secs, 120);

    let waiting = alpha
        .queue
        .iter()
        .find(|job| job.job == ready.id)
        .expect("ready job in queue");
    assert_eq!(
        waiting.waiting_reason.as_deref(),
        Some("waiting: RAM 1.2 GB short"),
        "the admission reason comes from jobs.note"
    );
    assert!(
        alpha.queue.iter().any(|job| job.job == parked.id),
        "parked jobs also appear in the queue"
    );

    let blocked: Vec<&str> = alpha.blocked.iter().map(|job| job.job.as_str()).collect();
    assert_eq!(blocked, vec!["a04-parked", "a05-failed"]);
    let parked_card = alpha
        .blocked
        .iter()
        .find(|job| job.job == parked.id)
        .expect("parked card");
    assert_eq!(parked_card.reason, "credits");
    assert_eq!(parked_card.detail, "out of credits");

    assert_eq!(alpha.gate_failures.len(), 1);
    assert_eq!(alpha.gate_failures[0].gate, "report");
    assert!(alpha.gate_failures[0].evidence.contains("missing fields: status"));
    assert_eq!(
        alpha.last_merge.as_ref().map(|merge| merge.commit.as_str()),
        Some("abc1234")
    );
    assert_eq!(
        alpha.last_merge.as_ref().map(|merge| merge.branch.as_str()),
        Some("job/a01-done")
    );
    assert_eq!(alpha.cost_today_usd, Some(1.25));

    let beta = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.beta)
        .expect("beta card");
    assert_eq!(beta.jobs_by_state.get("pending"), Some(&1));

    assert_eq!(snapshot.inbox.len(), 2, "node and project inbox rows merge");
    assert_eq!(snapshot.inbox[0].title, "Node notice", "oldest first");
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(
        snapshot.sessions[0].active_surface, None,
        "no lease is live in this fixture"
    );
}

#[test]
fn project_cards_read_registry_state_weight_and_pause_reason() {
    let fixture = Fixture::new();
    fixture
        .store
        .set_project_state(&fixture.alpha, ProjectState::Paused, "owner")
        .expect("pause alpha");
    fixture
        .store
        .set_project_weight(&fixture.alpha, 7)
        .expect("alpha weight");
    fixture
        .store
        .set_project_state(&fixture.beta, ProjectState::Archived, "")
        .expect("archive beta");
    fixture
        .store
        .set_project_weight(&fixture.beta, 23)
        .expect("beta weight");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.state, ProjectState::Paused);
    assert_eq!(alpha.weight, 7);
    assert_eq!(alpha.paused_reason, "owner");

    let beta = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.beta)
        .expect("beta card");
    assert_eq!(beta.state, ProjectState::Archived);
    assert_eq!(beta.weight, 23, "a non-default weight reaches the snapshot");
    assert_eq!(beta.paused_reason, "");

    let alpha_entry = fixture
        .store
        .project_entry(&fixture.alpha)
        .expect("alpha entry")
        .expect("alpha row");
    assert_eq!(alpha_entry.state, ProjectState::Paused);
    assert_eq!(alpha_entry.weight, 7);
    assert_eq!(alpha_entry.paused_reason, "owner");
    assert_eq!(alpha_entry.archived_at, None);

    let beta_entry = fixture
        .store
        .project_entry(&fixture.beta)
        .expect("beta entry")
        .expect("beta row");
    assert_eq!(beta_entry.state, ProjectState::Archived);
    assert_eq!(beta_entry.weight, 23);
    assert!(beta_entry.archived_at.is_some(), "archiving stamps archived_at");

    let by_path = fixture
        .store
        .project_by_path(std::path::Path::new(&beta_entry.path))
        .expect("lookup by path")
        .expect("beta by path");
    assert_eq!(by_path.name, fixture.beta);
    let entries = fixture.store.list_project_entries().expect("list entries");
    assert_eq!(entries.len(), 2, "both registry rows are listed");
}

#[test]
fn capacity_card_uses_the_spec_defaults_until_a_sampler_lands() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let capacity = &snapshot.capacity;
    assert!(capacity.measured.is_none(), "no measurement is fabricated");
    assert_eq!(capacity.reserve_ram_bytes, 1_073_741_824, "the spec's 1 GiB reserve");
    assert_eq!(capacity.min_disk_bytes, 5_368_709_120, "the spec's 5 GiB minimum disk");
    assert!(!capacity.brake.active, "the default brake is inactive");
    assert!(
        capacity.brake.reasons.is_empty() && capacity.brake.since.is_none(),
        "an inactive brake carries no reasons: {:?}",
        capacity.brake
    );
}

#[test]
fn capacity_card_uses_the_configured_target_and_provider_brake() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let mut config = (*fixture.deps.config).clone();
    config.providers.insert(
        "deepseek".to_owned(),
        ProviderConfig {
            max_concurrent: 2,
            on_credits_exhausted: Default::default(),
            prices: Default::default(),
        },
    );
    let mut deps = fixture.deps.clone();
    deps.config = Arc::new(config);

    let manager = vowp_core::capacity::CapacityManager::new(deps.clone());
    let target = manager
        .set_target(&fixture.store, TargetMode::Fixed, 2, false, "owner", "two workers")
        .unwrap();
    assert_eq!(target.target_workers, 2);
    fixture
        .store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Rate,
            resets_at: None,
            detail: "429".to_owned(),
            since: now,
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .unwrap();

    fleet::write_snapshot(&deps, &fixture.store, now).expect("write snapshot");
    let snapshot = fleet::read_snapshot(&fixture.paths).expect("read snapshot");
    assert_eq!(snapshot.capacity.target.mode, TargetMode::Fixed);
    assert_eq!(snapshot.capacity.target.target_workers, 2);
    assert_eq!(snapshot.capacity.target.set_by, "owner");
    assert_eq!(
        snapshot.capacity.target.advised_workers,
        Some(2),
        "the configured provider cap seeds the advised target, which the store-only helper cannot see"
    );
    assert!(
        snapshot.capacity.brake.active,
        "the configured provider limit must appear in the snapshot"
    );
    assert!(
        snapshot
            .capacity
            .brake
            .reasons
            .iter()
            .any(|reason| reason.code == vowp_protocol::BrakeCode::Provider),
        "{:?}",
        snapshot.capacity.brake
    );
}

#[test]
fn last_merge_skips_a_newer_merge_refusal() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let merged = fixture.job(&fixture.alpha, "m01-merged", 50);
    let refused = fixture.job(&fixture.alpha, "m02-refused", 50);
    let store = fixture.store_for(&fixture.alpha);
    let merged_at = now.minus(Duration::from_secs(120));
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: merged_at,
            kind: LedgerKind::Merge,
            project: Some(fixture.alpha.clone()),
            job_id: Some(merged.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: "merged m01-merged".to_owned(),
            detail: serde_json::json!({
                "main_after": "def5678",
                "branch": "job/m01-merged",
            }),
        })
        .expect("real merge row");
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: now.minus(Duration::from_secs(30)),
            kind: LedgerKind::Merge,
            project: Some(fixture.alpha.clone()),
            job_id: Some(refused.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: "merge refused".to_owned(),
            detail: serde_json::json!({
                "failure": "main advanced during the merge",
                "branch": "job/m02-refused",
            }),
        })
        .expect("refusal row");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    let merge = alpha.last_merge.as_ref().expect("a real merge is reported");
    assert_eq!(merge.job, merged.id, "the refusal is not the last merge");
    assert_eq!(merge.commit, "def5678");
    assert_eq!(merge.branch, "job/m01-merged");
    assert_eq!(merge.at, merged_at);
}

#[test]
fn snapshot_keeps_full_titles_and_questions_but_caps_body_detail() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let long_title = format!("title {}", "t".repeat(240));
    let long_question = format!("question {}", "q".repeat(240));
    let long_session = format!("session {}", "s".repeat(240));
    assert!(long_title.chars().count() > 200);

    let ready = fixture.job_with_title(&fixture.alpha, "t01-ready", &long_title, 50);
    fixture.set_state(
        &fixture.alpha,
        &ready,
        JobState::Ready,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    let failed = fixture.job(&fixture.alpha, "t02-failed", 10);
    let body = format!("body {}", "b".repeat(240));
    fixture.set_state(
        &fixture.alpha,
        &failed,
        JobState::Failed,
        StateChange {
            note: Some(body),
            parked_reason: None,
        },
    );

    let alpha_store = fixture.store_for(&fixture.alpha);
    alpha_store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: None,
            kind: InboxKind::Question,
            title: long_question.clone(),
            body: "question body".to_owned(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(10)),
            answered_at: None,
        })
        .expect("long question");
    fixture
        .store
        .upsert_session(&Session {
            id: SessionId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: None,
            role: Some(SessionRole::Worker),
            title: long_session.clone(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/srv/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now.minus(Duration::from_secs(60)),
            updated_at: now.minus(Duration::from_secs(5)),
        })
        .expect("long session title");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    let queued = alpha.queue.iter().find(|job| job.job == ready.id).expect("ready job");
    assert_eq!(queued.title, long_title, "a title is not truncated to 200 chars");
    let blocked = alpha
        .blocked
        .iter()
        .find(|job| job.job == failed.id)
        .expect("failed job");
    assert_eq!(
        blocked.detail.chars().count(),
        200,
        "body/detail text keeps the 200-character cap"
    );
    assert_eq!(snapshot.inbox[0].title, long_question, "a question keeps its full text");
    assert_eq!(
        snapshot.sessions[0].title, long_session,
        "a session title keeps its full text"
    );
}

#[test]
fn snapshot_written_atomically_every_tick() {
    let fixture = Fixture::new();
    fixture.job(&fixture.alpha, "a01", 50);
    let now = fixture.clock.now();

    fleet::write_snapshot(&fixture.deps, &fixture.store, now).expect("first write");
    let first = fleet::read_snapshot(&fixture.paths).expect("first read");
    assert_eq!(first.sequence, 1);
    assert_eq!(first.schema, SNAPSHOT_FORMAT);
    assert_eq!(first.version, SNAPSHOT_VERSION);
    assert!(
        !fixture.paths.tmp_dir().join("snapshot.json.tmp").exists(),
        "the temporary file is renamed away"
    );

    fleet::write_snapshot(&fixture.deps, &fixture.store, now).expect("second write");
    let second = fleet::read_snapshot(&fixture.paths).expect("second read");
    assert_eq!(second.sequence, 2, "the sequence increments every write");
    assert_eq!(second.projects.len(), 2);
}

#[test]
fn snapshot_without_secrets_is_written_unchanged() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let ready = fixture.job_with_title(&fixture.alpha, "plain-01", "plain title", 40);
    fixture.set_state(
        &fixture.alpha,
        &ready,
        JobState::Ready,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    fixture
        .store
        .set_project_state(&fixture.alpha, ProjectState::Paused, "owner unavailable")
        .expect("pause alpha");

    let mut built = fixture.snapshot();
    fleet::write_snapshot(&fixture.deps, &fixture.store, now).expect("write snapshot");
    let written = fleet::read_snapshot(&fixture.paths).expect("read snapshot");

    assert_eq!(written.sequence, built.sequence + 1);
    built.sequence = written.sequence;
    assert_eq!(built, written, "a home without secrets writes the same document");
    let alpha = written
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.paused_reason, "owner unavailable");
    let queued = alpha.queue.iter().find(|job| job.job == ready.id).expect("ready job");
    assert_eq!(queued.title, "plain title");
    assert_eq!(queued.waiting_reason, None);
    let text = std::fs::read_to_string(fixture.paths.root().join("snapshot.json")).expect("snapshot text");
    assert!(!text.contains("[REDACTED:"), "no secret marker is introduced");
}

#[test]
fn snapshot_write_redacts_secret_values_including_json_escaped_text() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let secret = "snap-\"quoted\"\\path\nsecond line";
    let name = SecretName::parse("SNAPSHOT_KEY").expect("secret name");
    vowp_core::vault::write_secret(&fixture.deps, &name, secret).expect("write secret");
    let marker = "[REDACTED:SNAPSHOT_KEY]";

    let ready = fixture.job_with_title(&fixture.alpha, "s01-ready", &format!("ready {secret}"), 50);
    fixture.set_state(
        &fixture.alpha,
        &ready,
        JobState::Ready,
        StateChange {
            note: None,
            parked_reason: None,
        },
    );
    let alpha_store = fixture.store_for(&fixture.alpha);
    alpha_store
        .set_job_note(&ready.id, &format!("waiting: {secret}"))
        .expect("waiting note");

    let failed = fixture.job(&fixture.alpha, "s02-failed", 10);
    fixture.set_state(
        &fixture.alpha,
        &failed,
        JobState::Failed,
        StateChange {
            note: Some(format!("blocked {secret}")),
            parked_reason: None,
        },
    );

    alpha_store
        .insert_gate_result(&GateResult {
            id: None,
            job_id: ready.id.clone(),
            run_id: None,
            attempt: 0,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: false,
            evidence: Evidence::Report {
                front_matter: None,
                missing_fields: vec![format!("status {secret}")],
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 1,
            ts: now.minus(Duration::from_secs(30)),
        })
        .expect("gate result");

    alpha_store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: None,
            kind: InboxKind::Question,
            title: format!("question {secret}"),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now.minus(Duration::from_secs(10)),
            answered_at: None,
        })
        .expect("inbox item");

    fixture
        .store
        .upsert_session(&Session {
            id: SessionId::new(),
            project: Some(fixture.alpha.clone()),
            job_id: None,
            role: Some(SessionRole::Worker),
            title: format!("session {secret}"),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/srv/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now.minus(Duration::from_secs(60)),
            updated_at: now.minus(Duration::from_secs(5)),
        })
        .expect("session");

    // `paused_reason` is copied verbatim by the card builder, so this field proves the
    // final structured pass runs rather than only the per-field redaction inside the cards.
    fixture
        .store
        .set_project_state(&fixture.alpha, ProjectState::Paused, &format!("owner {secret}"))
        .expect("pause alpha");

    fleet::write_snapshot(&fixture.deps, &fixture.store, now).expect("write snapshot");
    let text = std::fs::read_to_string(fixture.paths.root().join("snapshot.json")).expect("snapshot text");
    let escaped = serde_json::to_string(secret).expect("json-escape the secret");
    let escaped = &escaped[1..escaped.len() - 1];
    assert!(!text.contains(secret), "the raw secret is not written");
    assert!(
        !text.contains(escaped),
        "the JSON-escaped secret is not written: {escaped:?}"
    );
    assert!(text.contains(marker), "the snapshot records the redaction");

    let snapshot = fleet::read_snapshot(&fixture.paths).expect("read snapshot");
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.paused_reason, format!("owner {marker}"));
    let queued = alpha.queue.iter().find(|job| job.job == ready.id).expect("ready job");
    assert_eq!(queued.title, format!("ready {marker}"));
    assert_eq!(
        queued.waiting_reason.as_deref(),
        Some(format!("waiting: {marker}").as_str())
    );
    let blocked = alpha
        .blocked
        .iter()
        .find(|job| job.job == failed.id)
        .expect("failed job");
    assert_eq!(blocked.detail, format!("blocked {marker}"));
    assert_eq!(
        alpha.gate_failures[0].evidence,
        format!("missing fields: status {marker}")
    );
    assert_eq!(snapshot.inbox[0].title, format!("question {marker}"));
    assert_eq!(snapshot.sessions[0].title, format!("session {marker}"));
}

#[test]
fn snapshot_reader_never_sees_partial_json() {
    let fixture = Fixture::new();
    fixture.job(&fixture.alpha, "a01", 50);
    fleet::write_snapshot(&fixture.deps, &fixture.store, fixture.clock.now()).expect("write");
    let valid = fleet::read_snapshot(&fixture.paths).expect("valid snapshot");

    // A half-written temporary file is invisible to readers: only the renamed file is read.
    std::fs::write(
        fixture.paths.tmp_dir().join("snapshot.json.tmp"),
        b"{\"schema\": \"vowp-snapshot/1\", \"version\":",
    )
    .expect("partial tmp");
    assert_eq!(
        fleet::read_snapshot(&fixture.paths).expect("main file still valid"),
        valid
    );

    // A truncated main file is a clean error, never a silent partial document.
    let path = fixture.paths.root().join("snapshot.json");
    let text = std::fs::read_to_string(&path).expect("snapshot text");
    std::fs::write(&path, &text[..text.len() / 2]).expect("truncated snapshot");
    let error = fleet::read_snapshot(&fixture.paths).expect_err("partial JSON must fail");
    assert!(
        error.to_string().contains("snapshot.json"),
        "the reader names the file: {error}"
    );
}

#[test]
fn snapshot_reader_rejects_newer_or_wrong_schema() {
    let fixture = Fixture::new();
    fleet::write_snapshot(&fixture.deps, &fixture.store, fixture.clock.now()).expect("write");
    let path = fixture.paths.root().join("snapshot.json");
    let text = std::fs::read_to_string(&path).expect("snapshot text");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("snapshot json");

    value["future_field"] = serde_json::json!(true);
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("render")).expect("unknown field");
    assert!(
        fleet::read_snapshot(&fixture.paths).is_ok(),
        "unknown fields in a known version are ignored"
    );

    value["version"] = serde_json::json!(SNAPSHOT_VERSION + 1);
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("render")).expect("newer version");
    let error = fleet::read_snapshot(&fixture.paths).expect_err("newer version must be rejected");
    assert!(error.to_string().contains("newer"), "{error}");

    value["version"] = serde_json::json!(SNAPSHOT_VERSION);
    value["schema"] = serde_json::json!("vowp-snapshot/99");
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("render")).expect("wrong schema");
    let error = fleet::read_snapshot(&fixture.paths).expect_err("wrong schema must be rejected");
    assert!(error.to_string().contains("schema"), "{error}");
}

#[test]
fn snapshot_degrades_one_broken_project_and_keeps_the_rest() {
    let fixture = Fixture::new();
    fixture.job(&fixture.alpha, "a01", 50);
    let beta_db = fixture.paths.project_db_path(&fixture.beta);
    std::fs::remove_file(&beta_db).expect("remove beta db");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.jobs_by_state.get("pending"), Some(&1));

    let beta = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.beta)
        .expect("beta card still exists");
    assert_eq!(beta.state, vowp_protocol::ProjectState::Paused);
    assert!(
        beta.paused_reason.starts_with("broken:"),
        "the card explains why: {}",
        beta.paused_reason
    );
    assert!(beta.running.is_empty() && beta.queue.is_empty());
}

#[test]
fn gate_failures_include_older_attempts() {
    let fixture = Fixture::new();
    let job = fixture.job(&fixture.alpha, "g01-two-attempts", 50);
    let store = fixture.store_for(&fixture.alpha);
    let now = fixture.clock.now();
    store
        .set_job_counters(&job.id, Some(1), None, None)
        .expect("attempt counter");

    store
        .insert_gate_result(&GateResult {
            id: None,
            job_id: job.id.clone(),
            run_id: None,
            attempt: 0,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: false,
            evidence: Evidence::Report {
                front_matter: None,
                missing_fields: vec!["status".to_owned()],
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 1,
            ts: now.minus(Duration::from_secs(60)),
        })
        .expect("failed attempt 0");
    store
        .insert_gate_result(&GateResult {
            id: None,
            job_id: job.id.clone(),
            run_id: None,
            attempt: 1,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: true,
            evidence: Evidence::Report {
                front_matter: Some(serde_json::json!({"status": "done"})),
                missing_fields: Vec::new(),
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 1,
            ts: now.minus(Duration::from_secs(10)),
        })
        .expect("passed attempt 1");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(
        alpha.gate_failures.len(),
        1,
        "a failed older attempt is still a project failure: {:?}",
        alpha.gate_failures
    );
    assert_eq!(alpha.gate_failures[0].job, job.id);
    assert_eq!(alpha.gate_failures[0].gate, "report");
    assert!(alpha.gate_failures[0].evidence.contains("status"));
}

#[test]
fn gate_failures_keep_only_the_newest_ten() {
    let fixture = Fixture::new();
    let job = fixture.job(&fixture.alpha, "g02-many-failures", 50);
    let store = fixture.store_for(&fixture.alpha);
    let now = fixture.clock.now();
    store
        .set_job_counters(&job.id, Some(1), None, None)
        .expect("attempt counter");

    for index in 0..12_u32 {
        store
            .insert_gate_result(&GateResult {
                id: None,
                job_id: job.id.clone(),
                run_id: None,
                attempt: 0,
                gate_index: i32::try_from(index).expect("gate index"),
                gate: Gate::Report(ReportGate::default()),
                passed: false,
                evidence: Evidence::Report {
                    front_matter: None,
                    missing_fields: vec![format!("field{index}")],
                    problems: Vec::new(),
                },
                output_blob: None,
                duration_ms: 1,
                ts: now.minus(Duration::from_secs(u64::from(index) + 1)),
            })
            .expect("failed result");
    }
    store
        .insert_gate_result(&GateResult {
            id: None,
            job_id: job.id.clone(),
            run_id: None,
            attempt: 1,
            gate_index: 0,
            gate: Gate::Report(ReportGate::default()),
            passed: true,
            evidence: Evidence::Report {
                front_matter: Some(serde_json::json!({"status": "done"})),
                missing_fields: Vec::new(),
                problems: Vec::new(),
            },
            output_blob: None,
            duration_ms: 1,
            ts: now,
        })
        .expect("passed newest attempt");

    let snapshot = fixture.snapshot();
    let alpha = snapshot
        .projects
        .iter()
        .find(|project| project.name == fixture.alpha)
        .expect("alpha card");
    assert_eq!(alpha.gate_failures.len(), 10, "the card is capped at ten");
    assert!(
        alpha.gate_failures[0].evidence.contains("field0"),
        "newest failure first: {:?}",
        alpha.gate_failures
    );
    assert!(
        alpha.gate_failures[9].evidence.contains("field9"),
        "the ten newest survive: {:?}",
        alpha.gate_failures
    );
    assert!(
        alpha
            .gate_failures
            .iter()
            .all(|failure| !failure.evidence.contains("field10") && !failure.evidence.contains("field11")),
        "older failures are dropped: {:?}",
        alpha.gate_failures
    );
}

#[test]
fn supervisor_card_reports_l1_fail_streak() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    fixture
        .store_for(&fixture.alpha)
        .upsert_loop(&Loop {
            name: "l1-supervisor".to_owned(),
            kind: LoopKind::L1,
            project: Some(fixture.alpha.clone()),
            interval_secs: 60,
            state: ActiveState::Active,
            bounds: Default::default(),
            last_run_at: Some(now),
            next_run_at: Some(now.plus(Duration::from_secs(60))),
            last_status: "degraded: merge".to_owned(),
            fail_streak: 3,
            updated_at: now,
        })
        .expect("supervisor loop row");

    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.supervisor.fail_streak, 3,
        "the card mirrors loops['l1-supervisor'].fail_streak"
    );
}
