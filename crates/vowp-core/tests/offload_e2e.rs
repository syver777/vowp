//! The Phase 2 offload loop end to end: dispatch, accept, worktree, events, push, return.
//!
//! Every test uses a temporary home store, a temporary bare origin, a real home clone, a
//! real node clone, a fake in-memory gateway transport and a fake adapter script. The home
//! half runs the real gateway, dispatcher and result return; the node half is a small fake
//! agent that resolves the assignment, creates a real worktree, emits recorded events,
//! commits and pushes `job/<id>`. No test opens `~/.vowp`, touches the network, starts tmux
//! or runs a real harness. Spec: `docs/design/v0.4/local-orchestrator.md` §6.3–§6.5, §7,
//! §8.4, §9 and §14.3.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use tempfile::TempDir;
use vowp_core::agent::gateway::{self, GatewayOptions, GatewayOutcome};
use vowp_core::agent::{EnrollRequest, auth, create_pending, dispatch, redeem, result};
use vowp_core::{AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    Actor, AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, AssignmentPayload, Clock, Cost, EventBody,
    EventId, EventSource, EventType, FakeClock, Gate, GateKind, Job, JobId, JobKind, JobState, MemoryLog, NewJob,
    NodeName, PathRef, Placement, Project, ProjectName, RemoteEvent, RemoteLine, RemoteRunState, ResultTransport, Role,
    RunId, RunOutcome, RunResume, SecretName, StopReason, Timestamp, TurnSummary, VowpConfig,
};
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState};
use vowp_store::session_log::SessionLog;
use vowp_store::{LedgerFilter, NodePaths, StateChange, Store, hash_lease_token};

/// The fake adapter body: one recorded JSONL line per assistant message.
const ADAPTER_BODY: &str = r#"
printf '%s\n' '{"type":"assistant.message","text":"read the brief and the owned files"}'
printf '%s\n' '{"type":"assistant.message","text":"edited only the paths the job owns"}'
printf '%s\n' '{"type":"assistant.message","text":"the branch is ready for the home"}'
"#;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

fn project() -> ProjectName {
    ProjectName::parse("alpha").expect("project")
}

fn node() -> NodeName {
    NodeName::parse("mac-studio").expect("node")
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[11_u8; 32])
}

fn public_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn signature_hex(key: &SigningKey, nonce: &str, home: &str, connection: &str) -> String {
    key.sign(&auth::handshake_payload(nonce, home, connection))
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The knobs one end-to-end home can vary.
struct HomeOptions {
    placement: Placement,
    kind: JobKind,
    project_home_only: bool,
    online: bool,
    result_transport: ResultTransport,
}

impl Default for HomeOptions {
    fn default() -> Self {
        Self {
            placement: Placement::Any,
            kind: JobKind::Build,
            project_home_only: false,
            online: true,
            result_transport: ResultTransport::Push,
        }
    }
}

/// One hermetic home, project clone, bare origin, node clone and fake adapter.
struct Home {
    _tmp: TempDir,
    root: PathBuf,
    origin: PathBuf,
    checkout: PathBuf,
    node_checkout: PathBuf,
    worktree_root: PathBuf,
    adapter: PathBuf,
    deps: Deps,
    store: Store,
    clock: Arc<FakeClock>,
    project: ProjectName,
    node: NodeName,
    key: SigningKey,
    job: Job,
    base: String,
}

impl Home {
    fn new(options: HomeOptions) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        let checkout = root.join("checkout");
        init_bare(&origin);
        init_repo(&checkout);
        git_ok(&checkout, &["remote", "add", "origin", &origin.display().to_string()]);
        git_ok(&checkout, &["push", "-u", "origin", "main"]);
        let base = git_output(&checkout, &["rev-parse", "HEAD"]).trim().to_owned();

        let node_checkout = root.join("node-checkout");
        git_ok(
            &root,
            &[
                "clone",
                &origin.display().to_string(),
                &node_checkout.display().to_string(),
            ],
        );
        let worktree_root = root.join("node-worktrees");
        fs::create_dir_all(&worktree_root).expect("worktree root");

        let adapter = root.join("fake-adapter.sh");
        write(
            &adapter,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '1.2.3\\n'\n  exit 0\nfi\n{ADAPTER_BODY}"
            ),
        );
        let mut permissions = fs::metadata(&adapter).expect("adapter stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&adapter, permissions).expect("adapter mode");

        let paths = NodePaths::new(root.join("node"));
        paths.ensure().expect("node paths");
        let clock = Arc::new(FakeClock::at(ts()));
        let project = project();
        let node = node();
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(checkout.display().to_string());
        config.project.remote = "origin".to_owned();
        config.project.default_branch = "main".to_owned();
        config.offload.enabled = true;
        config.offload.result_transport = options.result_transport;
        config.offload.projects.insert(
            project.clone(),
            vowp_protocol::config::OffloadProjectConfig {
                home_only: options.project_home_only,
                allow_secrets: vec![SecretName::parse("BUILD_TOKEN").expect("secret name")],
                ..vowp_protocol::config::OffloadProjectConfig::default()
            },
        );
        config.harnesses.insert(
            "opencode".to_owned(),
            vowp_protocol::HarnessConfig {
                env_pass: vec!["BUILD_TOKEN".to_owned()],
                ..vowp_protocol::HarnessConfig::default()
            },
        );
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: NodeName::parse("home-1").expect("home node"),
            clock: clock.clone(),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
            git: Arc::new(SystemGit::default()),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(5)),
        };
        let store = Store::open(&deps.paths).expect("store").with_clock(clock.clone());
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: checkout.display().to_string(),
                remote_url: Some(origin.display().to_string()),
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: ts(),
                updated_at: ts(),
            })
            .expect("project row");
        Store::create_project_db(&deps.paths, &project).expect("project database");

        let key = signing_key();
        let pending = create_pending(&store, &node, &["linux".to_owned()], ts()).expect("pending node");
        let request = EnrollRequest {
            code: pending.code,
            public_key: public_hex(&key),
            agent_version: "0.3.0".to_owned(),
            capabilities: capabilities(&project),
            paths: vec![AgentPathMap {
                project: project.clone(),
                local_root: node_checkout.display().to_string(),
                worktree_root: Some(worktree_root.display().to_string()),
                allow: Vec::new(),
            }],
        };
        redeem(&store, &request, ts()).expect("redeem enrollment");
        let seen = if options.online {
            ts()
        } else {
            ts().minus(Duration::from_secs(120))
        };
        store
            .touch_node(
                &node,
                seen,
                Some("0.3.0"),
                Some(vowp_protocol::AGENT_PROTOCOL),
                &capabilities_with_capacity(&project),
            )
            .expect("node heartbeat");

        let new = NewJob {
            id: JobId::parse("offload-e2e").expect("job id"),
            title: "offload end to end".to_owned(),
            kind: options.kind,
            role: Role::Worker,
            profile: Some("worker".to_owned()),
            needs: Vec::new(),
            owns: vec!["src/**".to_owned()],
            reads: Vec::new(),
            gates: Some(vec![Gate::OwnsRespected(true), Gate::SecretScan(true)]),
            priority: None,
            loop_: None,
            brief_text: None,
        };
        let job = store.insert_job(&project, &new, "test").expect("insert job");
        store
            .set_job_placement(&job.id, options.placement, ts())
            .expect("job placement");
        store
            .set_job_state(
                &job.id,
                JobState::Ready,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("ready job");

        Self {
            _tmp: tmp,
            root,
            origin,
            checkout,
            node_checkout,
            worktree_root,
            adapter,
            deps,
            store,
            clock,
            project,
            node,
            key,
            job,
            base,
        }
    }

    /// Starts one gateway connection and completes the authenticated handshake.
    fn connect(&self) -> GatewayHarness {
        self.connect_with_store(None)
    }

    /// Starts one gateway connection over the home's global store.
    ///
    /// This is how `vowp agent gateway --stdio` runs from `$HOME` on a multi-project
    /// home: no project is bound at the top level, and each session/assignment names its
    /// own project.
    fn connect_global(&self) -> GatewayHarness {
        let store = Store::open_global(&self.deps.paths)
            .expect("global store")
            .with_clock(self.clock.clone());
        self.connect_with_store(Some(store))
    }

    fn connect_with_store(&self, store: Option<Store>) -> GatewayHarness {
        let harness = match store {
            Some(store) => GatewayHarness::start_with_store(self.deps.clone(), store),
            None => GatewayHarness::start(self.deps.clone()),
        };
        harness.send(&AgentFrame::AgentHello {
            node: self.node.clone(),
            key_id: public_hex(&self.key),
            agent_version: "0.3.0".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            labels: vec!["linux".to_owned()],
            capabilities: capabilities(&self.project),
        });
        let AgentFrame::HomeChallenge {
            nonce,
            home_id,
            connection_id,
            ..
        } = harness.expect(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }))
        else {
            panic!("expected HomeChallenge");
        };
        harness.send(&AgentFrame::AgentAuth {
            signature: signature_hex(&self.key, &nonce, &home_id, &connection_id),
        });
        harness.expect(|frame| matches!(frame, AgentFrame::HomeWelcome { .. }));
        harness
    }

    /// Registers one more project with no jobs, as a second checkout on the home would be.
    fn add_project(&self, name: &str) -> ProjectName {
        let project = ProjectName::parse(name).expect("project slug");
        let checkout = self.root.join(format!("{name}-checkout"));
        fs::create_dir_all(&checkout).expect("second checkout");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(checkout.display().to_string());
        // Offload stays off: this project exists only to make the home multi-project.
        config.offload.enabled = false;
        fs::write(
            checkout.join("vowp.toml"),
            toml::to_string_pretty(&config).expect("second config"),
        )
        .expect("second vowp.toml");
        let global = Store::open_global(&self.deps.paths)
            .expect("global store")
            .with_clock(self.clock.clone());
        global
            .upsert_project(&Project {
                name: project.clone(),
                path: checkout.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("second config json"),
                created_at: ts(),
                updated_at: ts(),
            })
            .expect("second project row");
        Store::create_project_db(&self.deps.paths, &project).expect("second project database");
        project
    }

    /// Sends one heartbeat so dispatch sees fresh capacity.
    fn heartbeat(&self, harness: &GatewayHarness) {
        harness.send(&AgentFrame::Heartbeat {
            at: ts(),
            capacity: capacity(),
            pending_spool_bytes: 0,
        });
    }

    /// Waits for one frame and returns the `AssignRun` payload and frame-only facts.
    fn expect_assign(&self, harness: &GatewayHarness, start: usize) -> (AssignmentPayload, String, Timestamp) {
        match harness.expect_after(start, |frame| matches!(frame, AgentFrame::AssignRun { .. })) {
            AgentFrame::AssignRun {
                assignment,
                lease_token,
                lease_expires_at,
            } => (assignment, lease_token, lease_expires_at),
            other => panic!("expected AssignRun, got {other:?}"),
        }
    }

    /// Sends the agent's acceptance of one assignment and waits for it to be applied.
    fn accept(&self, harness: &GatewayHarness, assignment: &AssignmentPayload) {
        harness.send(&AgentFrame::RunAccepted {
            assignment_id: assignment.assignment_id.clone(),
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
        });
        harness.barrier();
    }

    /// Opens the run's ingest stream with the frame-only run lease token.
    fn hello(&self, harness: &GatewayHarness, assignment: &AssignmentPayload, lease: &str) {
        harness.send(&AgentFrame::RemoteLine {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            line: RemoteLine::Hello {
                session_id: assignment.session_id.clone(),
                run_id: assignment.run_id.clone(),
                executor: assignment.executor.to_string(),
                node: self.node.clone(),
                spool_id: format!("{}-{}", assignment.session_id, assignment.run_id),
                lease: lease.to_owned(),
            },
        });
    }

    /// Sends one run state with the head commit the node produced.
    fn finishing(&self, harness: &GatewayHarness, assignment: &AssignmentPayload, head: &str) {
        harness.send(&AgentFrame::RunState {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            state: RemoteRunState::Finishing,
            spool_bytes: 0,
            head_commit: Some(head.to_owned()),
            detail: "the job branch is pushed".to_owned(),
        });
    }

    /// Sends the terminal summary and waits for its acknowledgement.
    fn turn_end(&self, harness: &GatewayHarness, assignment: &AssignmentPayload, summary: TurnSummary) {
        harness.send(&AgentFrame::TurnEnd {
            run_id: assignment.run_id.clone(),
            summary,
        });
        harness.expect(|frame| {
            matches!(
                frame,
                AgentFrame::IngestAck {
                    event_id: None,
                    error: None,
                    ..
                }
            )
        });
    }

    /// Creates the node worktree, runs the fake adapter and commits the owned files.
    ///
    /// The fake agent mirrors the real executor: the worktree starts at the assigned base
    /// commit on branch `job/<id>`, the adapter's recorded JSONL becomes ordered events and
    /// only committed files reach the push.
    fn run_job(&self, assignment: &AssignmentPayload, files: &[(&str, &str)]) -> FakeRun {
        let worktree = self.worktree_root.join(assignment.job.as_str());
        let branch = format!("job/{}", assignment.job);
        let worktree_text = worktree.display().to_string();
        git_ok(
            &self.node_checkout,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree_text,
                &assignment.base_commit,
            ],
        );
        for (path, text) in files {
            write(&worktree.join(path), text);
        }
        let events = run_adapter(&self.adapter, &worktree, assignment, &self.node);
        git_ok(&worktree, &["add", "-A"]);
        git_ok(&worktree, &["commit", "-m", "offload job work"]);
        let head = git_output(&worktree, &["rev-parse", "HEAD"]).trim().to_owned();
        FakeRun { worktree, events, head }
    }

    /// Pushes only `job/<id>` from the fake node worktree.
    fn push_job(&self, assignment: &AssignmentPayload, run: &FakeRun) {
        git_ok(
            &run.worktree,
            &[
                "push",
                "-f",
                "origin",
                &format!("HEAD:refs/heads/job/{}", assignment.job),
            ],
        );
    }

    fn job(&self) -> Job {
        self.store.require_job(&self.job.id).expect("job row")
    }

    fn assignment(&self, run: &RunId) -> AgentAssignment {
        self.store
            .assignments_for_run(run)
            .expect("assignments")
            .into_iter()
            .max_by_key(|assignment| assignment.epoch)
            .expect("assignment row")
    }

    fn log(&self, session: &vowp_protocol::SessionId) -> Vec<vowp_protocol::EventEnvelope> {
        SessionLog::open_read(&self.deps.paths, session)
            .expect("open session log")
            .read_all()
            .expect("read session log")
    }
}

/// One fake run produced by the node side.
struct FakeRun {
    worktree: PathBuf,
    events: Vec<RemoteEvent>,
    head: String,
}

/// A names-only capability document without a capacity snapshot.
fn capabilities(project: &ProjectName) -> AgentCapabilities {
    let mut harnesses = BTreeMap::new();
    harnesses.insert("opencode".to_owned(), "1.0.0".to_owned());
    AgentCapabilities {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.3.0".to_owned(),
        harnesses,
        git_version: Some("git version 2.47.0".to_owned()),
        labels: vec!["linux".to_owned()],
        secret_names: vec![SecretName::parse("BUILD_TOKEN").expect("secret name")],
        projects: vec![project.clone()],
    }
}

/// A capability document with one idle capacity snapshot.
fn capabilities_with_capacity(project: &ProjectName) -> serde_json::Value {
    let mut value = serde_json::to_value(capabilities(project)).expect("capabilities JSON");
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "capacity".to_owned(),
            serde_json::to_value(capacity()).expect("capacity JSON"),
        );
    }
    value
}

fn capacity() -> AgentCapacity {
    AgentCapacity {
        cpu_cores: 8,
        load1: 0.1,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        mem_free_bytes: 8 * 1024 * 1024 * 1024,
        disk_free_bytes: 100 * 1024 * 1024 * 1024,
        busy_slots: 0,
        max_slots: 4,
        on_battery: Some(false),
        thermal_state: Some("nominal".to_owned()),
    }
}

/// Runs the fake adapter in the worktree and turns its recorded JSONL into ordered events.
fn run_adapter(adapter: &Path, worktree: &Path, assignment: &AssignmentPayload, node: &NodeName) -> Vec<RemoteEvent> {
    let output = SystemRunner
        .run(&CommandSpec {
            shell: None,
            argv: vec![adapter.display().to_string()],
            cwd: Some(worktree.to_path_buf()),
            env: Vec::new(),
            timeout: Duration::from_secs(60),
        })
        .expect("fake adapter");
    assert_eq!(output.exit_code, Some(0), "fake adapter failed: {}", output.text());
    output
        .text()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("adapter JSONL");
            let event_type = match value["type"].as_str().expect("adapter type") {
                "assistant.message" => EventType::AssistantMessage,
                other => panic!("unexpected fake adapter event type {other}"),
            };
            RemoteEvent {
                id: EventId::new(),
                ts: ts(),
                event_type,
                actor: Actor::Assistant,
                source: EventSource {
                    harness: assignment.executor.harness,
                    model: Some(assignment.executor.model.clone()),
                    node: Some(node.clone()),
                    native_session_id: Some("fake-native".to_owned()),
                },
                payload: serde_json::json!({ "text": value["text"].as_str().expect("adapter text") }),
                raw: None,
                replayable: true,
            }
        })
        .collect()
}

/// The successful terminal summary the fake agent reports.
fn summary_ok() -> TurnSummary {
    TurnSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: Some("fake-native".to_owned()),
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("finished the offload job".to_owned()),
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

/// The summary the node reports when its `job/<id>` push failed.
///
/// The wire summary carries the same facts the real `CoreRunHandler` derives from
/// `PushStatus::Failed`: outcome `error` and a `git_push_failed` result text.
fn summary_push_failed() -> TurnSummary {
    TurnSummary {
        exit_code: Some(1),
        outcome: RunOutcome::Error,
        native_session_id: Some("fake-native".to_owned()),
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("git_push_failed: the job branch could not be pushed".to_owned()),
        stop_reason: StopReason::Error,
        usage: None,
    }
}

/// One event acknowledgement reduced to the facts these tests assert.
struct AckRecord {
    seq: Option<u64>,
    duplicate: bool,
}

/// Sends every event in `range` for one run and waits for each acknowledgement.
fn send_events(
    harness: &GatewayHarness,
    assignment: &AssignmentPayload,
    events: &[RemoteEvent],
    range: std::ops::Range<usize>,
) -> Vec<AckRecord> {
    let mut acks = Vec::new();
    for event in &events[range] {
        harness.send(&AgentFrame::RemoteLine {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            line: RemoteLine::Event { event: event.clone() },
        });
        let event_id = event.id.clone();
        let ack = harness.expect(|frame| {
            matches!(
                frame,
                AgentFrame::IngestAck { event_id: Some(id), .. } if *id == event_id
            )
        });
        let AgentFrame::IngestAck {
            event_id,
            seq,
            duplicate,
            error,
            ..
        } = ack
        else {
            panic!("expected IngestAck");
        };
        assert!(event_id.is_some(), "an event ack names its event");
        assert!(error.is_none(), "a valid event is never refused");
        acks.push(AckRecord { seq, duplicate });
    }
    acks
}

/// One controllable in-memory reader.
#[derive(Clone)]
struct TestReader {
    state: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
    closed: Arc<AtomicBool>,
}

impl TestReader {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn push(&self, frame: &AgentFrame) {
        let mut line = serde_json::to_vec(frame).expect("frame JSON");
        line.push(b'\n');
        let (queue, ready) = &*self.state;
        let mut queue = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.extend(line);
        ready.notify_all();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let (_, ready) = &*self.state;
        ready.notify_all();
    }
}

impl Read for TestReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let (queue, ready) = &*self.state;
        let mut queue = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while queue.is_empty() && !self.closed.load(Ordering::SeqCst) {
            queue = ready.wait(queue).expect("input wait");
        }
        if queue.is_empty() {
            return Ok(0);
        }
        let count = buf.len().min(queue.len());
        for slot in buf.iter_mut().take(count) {
            *slot = queue.pop_front().expect("queued byte");
        }
        Ok(count)
    }
}

/// One shared writer that tests can inspect without consuming it.
#[derive(Clone, Default)]
struct TestWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TestWriter {
    fn frames(&self) -> Vec<AgentFrame> {
        let bytes = self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn text(&self) -> String {
        let bytes = self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn wait_frame_after(&self, start: usize, predicate: impl Fn(&AgentFrame) -> bool, timeout: Duration) -> AgentFrame {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.frames().into_iter().skip(start).find(|frame| predicate(frame)) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a frame; saw {} frames",
                self.frames().len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One running gateway connection over the in-memory transport.
struct GatewayHarness {
    input: TestReader,
    output: TestWriter,
    handle: Option<JoinHandle<Result<GatewayOutcome, vowp_core::CoreError>>>,
}

impl GatewayHarness {
    fn start(deps: Deps) -> Self {
        let store = deps.open_store().expect("gateway store");
        Self::start_with_store(deps, store)
    }

    /// Starts one connection over an explicit store handle.
    fn start_with_store(deps: Deps, store: Store) -> Self {
        let input = TestReader::new();
        let output = TestWriter::default();
        let reader = BufReader::new(input.clone());
        let writer = output.clone();
        let options = GatewayOptions {
            home_id: "home-1".to_owned(),
            heartbeat_secs: 10,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(5),
        };
        let handle = thread::spawn(move || gateway::serve_with_options(&store, &deps, reader, writer, options));
        Self {
            input,
            output,
            handle: Some(handle),
        }
    }

    fn send(&self, frame: &AgentFrame) {
        self.input.push(frame);
    }

    fn expect(&self, predicate: impl Fn(&AgentFrame) -> bool) -> AgentFrame {
        self.expect_after(0, predicate)
    }

    fn expect_after(&self, start: usize, predicate: impl Fn(&AgentFrame) -> bool) -> AgentFrame {
        self.output.wait_frame_after(start, predicate, Duration::from_secs(20))
    }

    fn frame_count(&self) -> usize {
        self.output.frames().len()
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.output.frames()
    }

    fn text(&self) -> String {
        self.output.text()
    }

    /// Round-trips a `Ping` so a test can observe writes made by earlier frames.
    ///
    /// The gateway handles one frame at a time, so a `Pong` proves every frame sent
    /// before the `Ping` has completed its durable write.
    fn barrier(&self) {
        let start = self.frame_count();
        self.send(&AgentFrame::Ping);
        self.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    }

    fn shutdown(mut self) -> GatewayOutcome {
        self.input.close();
        self.handle
            .take()
            .expect("gateway thread")
            .join()
            .expect("gateway thread panicked")
            .expect("gateway returns")
    }
}

/// Runs the full offload turn, accepts, pushes, returns and gates one job.
fn assert_full_turn(home: &Home, gateway: GatewayHarness) {
    // The heartbeat is what carries fresh capacity into the real claim pass.
    gateway.send(&AgentFrame::Heartbeat {
        at: ts(),
        capacity: capacity(),
        pending_spool_bytes: 0,
    });
    let (assignment, lease_token, lease_expires_at) = home.expect_assign(&gateway, 0);

    // The AssignRun payload is durable, token-free and carries only names.
    assert_eq!(
        assignment.cwd_ref,
        PathRef::Worktree {
            project: home.project.clone(),
            job: home.job.id.clone(),
        }
    );
    assert_eq!(assignment.base_commit, home.base);
    assert_eq!(assignment.project, home.project);
    assert_eq!(assignment.job, home.job.id);
    assert_eq!(assignment.epoch, 1);
    assert_eq!(assignment.remote, "origin");
    assert_eq!(assignment.remote_url, home.origin.display().to_string());
    assert!(
        assignment.prompt.contains("offload-e2e"),
        "prompt: {}",
        assignment.prompt
    );
    assert_eq!(assignment.env_names, vec!["BUILD_TOKEN".to_owned()]);
    assert_eq!(assignment.secret_names.len(), 1);
    assert_eq!(assignment.secret_names[0].as_str(), "BUILD_TOKEN");
    assert!(assignment.deadline.is_none());
    assert_eq!(assignment.spool_cap_bytes, dispatch::SPOOL_CAP_BYTES);
    assert_eq!(lease_token.len(), 52);
    assert!(lease_expires_at > home.clock.now());
    let payload = serde_json::to_string(&assignment).expect("payload JSON");
    assert!(
        !payload.contains(&lease_token),
        "the lease token never enters the payload"
    );
    assert!(!payload.contains("lease_token"));
    assert_eq!(
        gateway.text().matches(&lease_token).count(),
        1,
        "the run lease token exists only in the AssignRun frame"
    );
    assert!(
        !payload.contains(&home.node_checkout.display().to_string()),
        "the node's local root never enters the payload"
    );
    let stored = home
        .store
        .assignment(&assignment.assignment_id)
        .expect("read assignment")
        .expect("queued row");
    assert_eq!(stored.payload, assignment);
    assert_eq!(stored.state, AgentAssignmentState::Sent);
    let lease = home
        .store
        .lease(&assignment.session_id)
        .expect("lease")
        .expect("run lease");
    assert_eq!(lease.token, hash_lease_token(&lease_token), "only the hash is stored");
    assert_ne!(lease.token, lease_token);
    assert_eq!(home.job().state, JobState::Ready, "the job runs only after acceptance");

    // The node accepts, creates a real worktree, commits and pushes.
    home.accept(&gateway, &assignment);
    assert_eq!(home.job().state, JobState::Running);
    let run = home.run_job(
        &assignment,
        &[("src/lib.rs", "pub fn offloaded() -> u32 {\n    7\n}\n")],
    );
    assert_eq!(
        git_output(&run.worktree, &["symbolic-ref", "--short", "HEAD"]).trim(),
        format!("job/{}", home.job.id)
    );
    let staged_base = git_output(&run.worktree, &["rev-parse", "HEAD~1"]).trim().to_owned();
    assert_eq!(staged_base, home.base, "the worktree starts at the assigned base");
    home.push_job(&assignment, &run);

    home.hello(&gateway, &assignment, &lease_token);
    let acks = send_events(&gateway, &assignment, &run.events, 0..run.events.len());
    let seqs = acks.iter().filter_map(|ack| ack.seq).collect::<Vec<_>>();
    assert_eq!(seqs.len(), 3);
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "ordered acks: {seqs:?}"
    );
    assert!(acks.iter().all(|ack| !ack.duplicate));
    home.finishing(&gateway, &assignment, &run.head);
    home.turn_end(&gateway, &assignment, summary_ok());

    // The home fetched and verified the branch, gated it and left it for review/merge.
    let job = home.job();
    assert_eq!(job.state, JobState::Gating, "note: {}", job.note);
    assert_eq!(job.branch.as_deref(), Some(format!("job/{}", home.job.id).as_str()));
    let staging = PathBuf::from(job.worktree.clone().expect("staging worktree"));
    assert!(staging.is_dir());
    assert_eq!(git_output(&staging, &["rev-parse", "HEAD"]).trim(), run.head);
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", "refs/remotes/origin/job/offload-e2e"]).trim(),
        run.head,
        "the home fetched the pushed job branch"
    );
    assert_eq!(
        git_output(&home.origin, &["rev-parse", "refs/heads/main"]).trim(),
        home.base,
        "the node never moves the default branch"
    );
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", "HEAD"]).trim(),
        home.base,
        "the main checkout does not move"
    );
    assert_eq!(
        git_output(&home.checkout, &["status", "--porcelain"]),
        "",
        "the main checkout stays clean"
    );
    assert_ne!(job.state, JobState::Merging);
    let merges = home
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Merge),
            job_id: Some(home.job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger");
    assert!(merges.is_empty(), "result return never merges");

    let record = result::load(&home.store, &assignment.run_id)
        .expect("result record")
        .expect("returned record");
    assert_eq!(record.status, "returned");
    assert_eq!(record.transport, "push");
    assert_eq!(record.base_commit, home.base);
    assert_eq!(record.head_commit.as_deref(), Some(run.head.as_str()));
    let gates = home
        .store
        .gate_results_for(&home.job.id, job.attempts)
        .expect("gate results");
    assert_eq!(gates.len(), 2, "owns_respected and secret_scan ran");
    assert!(gates.iter().all(|gate| gate.passed), "{gates:?}");
    assert!(
        gates.iter().any(|gate| gate.gate.kind() == GateKind::OwnsRespected)
            && gates.iter().any(|gate| gate.gate.kind() == GateKind::SecretScan)
    );
    let assignment_row = home.assignment(&assignment.run_id);
    assert_eq!(assignment_row.state, AgentAssignmentState::Finished);
    assert!(home.store.lease(&assignment.session_id).expect("lease").is_none());

    // The three adapter events reached the log in order, once each.
    let log = home.log(&assignment.session_id);
    let ids = run
        .events
        .iter()
        .map(|event| event.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let ours = log
        .iter()
        .filter(|event| ids.contains(event.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(ours.len(), 3);
    assert_eq!(
        ours.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
        run.events.iter().map(|event| event.id.as_str()).collect::<Vec<_>>()
    );
    assert!(
        ours.iter()
            .all(|event| matches!(event.body(), Ok(EventBody::AssistantMessage(_))))
    );
    assert_no_gaps(&log);

    let outcome = gateway.shutdown();
    assert!(matches!(outcome, GatewayOutcome::Closed { .. }));
    assert!(home.root.join("node").is_dir());
}

#[test]
fn fake_gateway_full_turn_and_job_end_to_end() {
    let home = Home::new(HomeOptions::default());
    let gateway = home.connect();
    assert_full_turn(&home, gateway);
}

#[test]
fn fake_gateway_full_turn_from_a_global_store_on_a_multi_project_home() {
    let home = Home::new(HomeOptions::default());
    home.add_project("beta");
    let gateway = home.connect_global();
    assert_full_turn(&home, gateway);
}

/// Runs a bundle result end to end and asserts the home imported the verified head.
fn assert_bundle_result(home: &Home, gateway: GatewayHarness) {
    home.heartbeat(&gateway);
    let (assignment, lease, _) = home.expect_assign(&gateway, 0);
    assert_eq!(assignment.result_transport, ResultTransport::Bundle);
    assert!(
        assignment.remote_url.is_empty(),
        "a bundle assignment carries no push remote"
    );
    home.accept(&gateway, &assignment);
    let run = home.run_job(&assignment, &[("src/lib.rs", "pub fn bundled() -> u32 {\n    4\n}\n")]);
    home.hello(&gateway, &assignment, &lease);

    // The node creates exactly one `base..job/<id>` bundle in its worktree.
    let bundle_path = home.root.join("node-bundle");
    let bundle_text = bundle_path.display().to_string();
    git_ok(
        &run.worktree,
        &[
            "bundle",
            "create",
            &bundle_text,
            &format!("{}..job/{}", assignment.base_commit, assignment.job),
        ],
    );
    let bytes = fs::read(&bundle_path).expect("bundle file");
    let sha = result::sha256_hex(&bytes);
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::BundleStart {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        base: assignment.base_commit.clone(),
        head: run.head.clone(),
        branch: format!("job/{}", assignment.job),
        bytes: bytes.len() as u64,
        sha256: sha.clone(),
    });
    let accepted = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::BundleAccept { .. }));
    match accepted {
        AgentFrame::BundleAccept { offset, .. } => assert_eq!(offset, 0, "a fresh transfer starts at zero"),
        other => panic!("expected BundleAccept, got {other:?}"),
    }
    for (index, chunk) in bytes.chunks(256 * 1024).enumerate() {
        gateway.send(&AgentFrame::BundleChunk {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            offset: (index * 256 * 1024) as u64,
            data: to_hex(chunk),
        });
    }
    gateway.send(&AgentFrame::BundleEnd {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        bytes: bytes.len() as u64,
        sha256: sha.clone(),
    });
    gateway.barrier();
    assert!(
        !gateway
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::BundleAbort { .. })),
        "the gateway did not refuse the bundle: {:?}",
        gateway.frames()
    );

    home.finishing(&gateway, &assignment, &run.head);
    home.turn_end(&gateway, &assignment, summary_ok());

    let job = home.job();
    assert_eq!(job.state, JobState::Gating, "note: {}", job.note);
    let inspection = format!("refs/vowp/jobs/{}", home.job.id);
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", &inspection]).trim(),
        run.head,
        "the verified inspection ref is the bundled head"
    );
    assert_eq!(
        git_output(
            &home.checkout,
            &["rev-parse", &format!("refs/heads/job/{}", home.job.id)]
        )
        .trim(),
        run.head
    );
    let pushed = git_output(&home.origin, &["for-each-ref", "--format=%(refname)", "refs/heads/job"]);
    assert!(pushed.trim().is_empty(), "the node never pushed: {pushed}");
    let record = result::load(&home.store, &assignment.run_id)
        .expect("result record")
        .expect("returned record");
    assert_eq!(record.status, "returned");
    assert_eq!(record.transport, "bundle");
    assert_eq!(record.base_commit, home.base);
    assert_eq!(record.head_commit.as_deref(), Some(run.head.as_str()));
    assert_eq!(record.bundle_bytes, Some(bytes.len() as u64));
    assert_eq!(record.bundle_sha256.as_deref(), Some(sha.as_str()));
    let staging = home
        .root
        .join("tmp")
        .join("agent-bundles")
        .join(assignment.run_id.as_str());
    assert!(!staging.exists(), "the home staging file is removed after settle");

    let outcome = gateway.shutdown();
    assert!(matches!(outcome, GatewayOutcome::Closed { .. }));
}

#[test]
fn fake_gateway_bundle_result_end_to_end() {
    let home = Home::new(HomeOptions {
        result_transport: ResultTransport::Bundle,
        ..HomeOptions::default()
    });
    let gateway = home.connect();
    assert_bundle_result(&home, gateway);
}

#[test]
fn fake_gateway_bundle_result_from_a_global_store_on_a_multi_project_home() {
    let home = Home::new(HomeOptions {
        result_transport: ResultTransport::Bundle,
        ..HomeOptions::default()
    });
    home.add_project("beta");
    let gateway = home.connect_global();
    assert_bundle_result(&home, gateway);
}

#[test]
fn bundle_post_import_gate_failure_keeps_the_inspection_ref_and_job_branch() {
    let home = Home::new(HomeOptions {
        result_transport: ResultTransport::Bundle,
        ..HomeOptions::default()
    });
    let gateway = home.connect();
    home.heartbeat(&gateway);
    let (assignment, lease, _) = home.expect_assign(&gateway, 0);
    assert_eq!(assignment.result_transport, ResultTransport::Bundle);
    assert!(
        assignment.remote_url.is_empty(),
        "a bundle assignment carries no push remote"
    );
    home.accept(&gateway, &assignment);
    // The bundle is a valid `base..job/<id>` transfer with one owned change and one path
    // outside the job's `src/**` owns. Only the post-import gate can reject it.
    let run = home.run_job(
        &assignment,
        &[
            ("src/lib.rs", "pub fn bundled() -> u32 {\n    4\n}\n"),
            ("notes/outside.txt", "not in the job brief\n"),
        ],
    );
    home.hello(&gateway, &assignment, &lease);

    // The node creates exactly one `base..job/<id>` bundle in its worktree.
    let bundle_path = home.root.join("node-bundle");
    let bundle_text = bundle_path.display().to_string();
    git_ok(
        &run.worktree,
        &[
            "bundle",
            "create",
            &bundle_text,
            &format!("{}..job/{}", assignment.base_commit, assignment.job),
        ],
    );
    let bytes = fs::read(&bundle_path).expect("bundle file");
    let sha = result::sha256_hex(&bytes);
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::BundleStart {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        base: assignment.base_commit.clone(),
        head: run.head.clone(),
        branch: format!("job/{}", assignment.job),
        bytes: bytes.len() as u64,
        sha256: sha.clone(),
    });
    let accepted = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::BundleAccept { .. }));
    match accepted {
        AgentFrame::BundleAccept { offset, .. } => assert_eq!(offset, 0, "a fresh transfer starts at zero"),
        other => panic!("expected BundleAccept, got {other:?}"),
    }
    for (index, chunk) in bytes.chunks(256 * 1024).enumerate() {
        gateway.send(&AgentFrame::BundleChunk {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            offset: (index * 256 * 1024) as u64,
            data: to_hex(chunk),
        });
    }
    gateway.send(&AgentFrame::BundleEnd {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        bytes: bytes.len() as u64,
        sha256: sha.clone(),
    });
    gateway.barrier();
    assert!(
        !gateway
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::BundleAbort { .. })),
        "the gateway did not refuse the bundle: {:?}",
        gateway.frames()
    );

    home.finishing(&gateway, &assignment, &run.head);
    home.turn_end(&gateway, &assignment, summary_ok());

    // The transfer and import succeeded; the gate rejection is the terminal result.
    let run_row = home.store.require_run(&assignment.run_id).expect("run row");
    assert_eq!(run_row.outcome, Some(RunOutcome::Error), "the run fails the gate");
    assert!(
        run_row.detail.contains("result_gate_failed"),
        "run detail: {}",
        run_row.detail
    );
    let record = result::load(&home.store, &assignment.run_id)
        .expect("result record")
        .expect("failed record");
    assert_eq!(record.status, "failed");
    assert_eq!(record.transport, "bundle");
    assert_eq!(record.base_commit, home.base);
    assert_eq!(record.head_commit.as_deref(), Some(run.head.as_str()));
    assert!(
        record.detail.starts_with("result_gate_failed:"),
        "detail: {}",
        record.detail
    );
    assert!(
        !record.detail.contains("bundle_failed"),
        "the bundle itself was accepted: {}",
        record.detail
    );

    let job = home.job();
    assert_eq!(job.state, JobState::Ready, "a gate failure keeps the job retryable");
    assert!(job.note.contains("result_gate_failed"), "note: {}", job.note);
    assert!(job.note.contains("owns_respected"), "note: {}", job.note);
    assert!(job.note.contains("notes/outside.txt"), "note: {}", job.note);
    assert_ne!(job.state, JobState::Merging);
    assert!(job.worktree.is_none(), "the staging worktree is removed for a retry");
    assert!(
        !home
            .deps
            .paths
            .root()
            .join("agent-results")
            .join(assignment.run_id.as_str())
            .exists(),
        "the staging worktree path is removed"
    );
    assert!(
        !home
            .deps
            .paths
            .root()
            .join("tmp")
            .join("agent-bundles")
            .join(assignment.run_id.as_str())
            .exists(),
        "the staging bundle file and repository are removed after settle"
    );

    // Step 4 imported the objects before the gate ran: both refs stay at the verified head.
    let inspection = format!("refs/vowp/jobs/{}", home.job.id);
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", &inspection]).trim(),
        run.head,
        "the inspection ref stays at the verified head"
    );
    let branch = format!("refs/heads/job/{}", home.job.id);
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", &branch]).trim(),
        run.head,
        "the local job branch stays at the verified head"
    );

    // A failed gate never merges and a bundle run never pushes.
    let merges = home
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Merge),
            job_id: Some(home.job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger");
    assert!(merges.is_empty(), "a failed gate never merges");
    assert_eq!(
        git_output(&home.checkout, &["rev-parse", "HEAD"]).trim(),
        home.base,
        "the main checkout does not move"
    );
    assert_eq!(
        git_output(&home.origin, &["rev-parse", "refs/heads/main"]).trim(),
        home.base,
        "the origin's default branch is untouched"
    );
    let pushed = git_output(&home.origin, &["for-each-ref", "--format=%(refname)", "refs/heads/job"]);
    assert!(pushed.trim().is_empty(), "the bundle path never pushes: {pushed}");
    let remote_branch = git_run(
        &home.checkout,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/job/{}", home.job.id),
        ],
    );
    assert_ne!(
        remote_branch.exit_code,
        Some(0),
        "the home never fetched a pushed job branch"
    );

    // The authoritative gates ran on the imported diff.
    let gates = home
        .store
        .gate_results_for(&home.job.id, job.attempts)
        .expect("gate results");
    let owns = gates
        .iter()
        .find(|gate| gate.gate.kind() == GateKind::OwnsRespected)
        .expect("owns result");
    assert!(!owns.passed, "the outside path violates owns: {:?}", owns.evidence);
    assert!(
        matches!(
            &owns.evidence,
            vowp_protocol::Evidence::OwnsRespected { violations, .. }
                if violations.iter().any(|path| path == "notes/outside.txt")
        ),
        "owns evidence: {:?}",
        owns.evidence
    );
    let secrets = gates
        .iter()
        .find(|gate| gate.gate.kind() == GateKind::SecretScan)
        .expect("secret result");
    assert!(secrets.passed, "the diff carries no secret: {:?}", secrets.evidence);
    assert_eq!(
        home.assignment(&assignment.run_id).state,
        AgentAssignmentState::Finished
    );
    assert!(home.store.lease(&assignment.session_id).expect("lease").is_none());

    let outcome = gateway.shutdown();
    assert!(matches!(outcome, GatewayOutcome::Closed { .. }));
    drop(home);
}

#[test]
fn offload_rejects_a_home_only_project_and_a_denied_kind() {
    let home_only = Home::new(HomeOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        project_home_only: true,
        ..HomeOptions::default()
    });
    let gateway = home_only.connect();
    home_only.heartbeat(&gateway);
    gateway.barrier();
    assert!(
        !gateway
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::AssignRun { .. })),
        "a home-only project never gets an AssignRun"
    );
    let job = home_only.job();
    assert_eq!(job.state, JobState::Ready, "the job stays home and ready");
    assert!(job.note.starts_with(dispatch::WAITING_PREFIX), "note: {}", job.note);
    assert!(job.note.contains("project_home_only"), "note: {}", job.note);
    assert!(
        home_only
            .store
            .assignments_for_job(&home_only.job.id)
            .expect("rows")
            .is_empty()
    );
    assert!(matches!(gateway.shutdown(), GatewayOutcome::Closed { .. }));

    let denied = Home::new(HomeOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        kind: JobKind::Merge,
        ..HomeOptions::default()
    });
    let gateway = denied.connect();
    denied.heartbeat(&gateway);
    gateway.barrier();
    assert!(
        !gateway
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::AssignRun { .. })),
        "a denied kind never gets an AssignRun"
    );
    let job = denied.job();
    assert_eq!(job.state, JobState::Ready, "the job stays home and ready");
    assert!(job.note.starts_with(dispatch::WAITING_PREFIX), "note: {}", job.note);
    assert!(job.note.contains("kind_not_allowed"), "note: {}", job.note);
    assert!(job.note.contains("merge"), "note: {}", job.note);
    assert!(
        denied
            .store
            .assignments_for_job(&denied.job.id)
            .expect("rows")
            .is_empty()
    );
    assert!(matches!(gateway.shutdown(), GatewayOutcome::Closed { .. }));
}

#[test]
fn a_pinned_node_offline_waits_and_never_runs_home() {
    let home = Home::new(HomeOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        online: false,
        ..HomeOptions::default()
    });
    let gateway = home.connect();
    // A Ping round-trip proves the post-handshake enqueue pass has finished; no heartbeat
    // is sent, so the node stays offline.
    gateway.barrier();
    assert!(
        !gateway
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::AssignRun { .. })),
        "an offline pinned node never gets an AssignRun"
    );
    let job = home.job();
    assert_eq!(job.state, JobState::Ready, "a waiting job is never failed");
    assert!(job.note.starts_with(dispatch::WAITING_PREFIX), "note: {}", job.note);
    assert!(job.note.contains("node_offline"), "note: {}", job.note);
    assert!(
        home.store.assignments_for_job(&home.job.id).expect("rows").is_empty(),
        "no home fallback and no node assignment"
    );
    assert_eq!(job.attempts, 0);
    assert!(matches!(gateway.shutdown(), GatewayOutcome::Closed { .. }));
}

#[test]
fn a_disconnect_mid_run_replays_without_duplicates() {
    let home = Home::new(HomeOptions::default());
    let first = home.connect();
    first.send(&AgentFrame::Heartbeat {
        at: ts(),
        capacity: capacity(),
        pending_spool_bytes: 0,
    });
    let (assignment, lease_token, _) = home.expect_assign(&first, 0);
    home.accept(&first, &assignment);
    let run = home.run_job(
        &assignment,
        &[("src/lib.rs", "pub fn resumed() -> bool {\n    true\n}\n")],
    );
    home.push_job(&assignment, &run);

    home.hello(&first, &assignment, &lease_token);
    let first_acks = send_events(&first, &assignment, &run.events, 0..2);
    let last_acked = first_acks
        .iter()
        .filter_map(|ack| ack.seq)
        .max()
        .expect("first half acknowledged");
    let session = home.store.require_session(&assignment.session_id).expect("session");
    assert!(matches!(first.shutdown(), GatewayOutcome::Closed { .. }));
    let assignment_row = home.assignment(&assignment.run_id);
    assert_eq!(
        assignment_row.state,
        AgentAssignmentState::Disconnected,
        "the home keeps the run resumable after the drop"
    );
    assert_eq!(
        home.job().state,
        JobState::Running,
        "a short drop does not fail the job"
    );

    // The reconnect resumes the run, gets a fresh lease token and a fresh AssignRun.
    // The start marker is taken before `Resume`: the home may write `ResumeOk` and the
    // fresh `AssignRun` back to back, so a marker taken after `ResumeOk` could skip it.
    let second = home.connect();
    let start = second.frame_count();
    second.send(&AgentFrame::Resume {
        runs: vec![RunResume {
            run_id: assignment.run_id.clone(),
            epoch: assignment.epoch,
            last_acked,
        }],
        surfaces: Vec::new(),
    });
    let AgentFrame::ResumeOk { runs, .. } =
        second.expect_after(start, |frame| matches!(frame, AgentFrame::ResumeOk { .. }))
    else {
        panic!("expected ResumeOk");
    };
    assert_eq!(runs.len(), 1);
    assert!(runs[0].accepted, "the run resumes after a short drop: {runs:?}");
    assert_eq!(runs[0].run_id, assignment.run_id);
    assert_eq!(runs[0].tail_seq, session.last_seq);
    let (resumed, resumed_lease, _) = home.expect_assign(&second, start);
    assert_eq!(
        resumed.assignment_id, assignment.assignment_id,
        "the same epoch resumes"
    );
    assert_eq!(resumed.epoch, assignment.epoch);
    assert_ne!(resumed_lease, lease_token, "a resume mints a fresh lease token");

    // The real agent re-accepts the live run and opens a new ingest stream.
    home.accept(&second, &resumed);
    home.hello(&second, &resumed, &resumed_lease);
    let replay = send_events(&second, &assignment, &run.events, 0..run.events.len());
    assert_eq!(replay.iter().filter(|ack| ack.duplicate).count(), 2);
    assert_eq!(replay.iter().filter(|ack| !ack.duplicate).count(), 1);
    home.finishing(&second, &assignment, &run.head);
    home.turn_end(&second, &assignment, summary_ok());

    let job = home.job();
    assert_eq!(job.state, JobState::Gating, "note: {}", job.note);
    assert_eq!(
        home.assignment(&assignment.run_id).state,
        AgentAssignmentState::Finished
    );
    assert!(home.store.lease(&assignment.session_id).expect("lease").is_none());

    let log = home.log(&assignment.session_id);
    assert_no_gaps(&log);
    let ids = run
        .events
        .iter()
        .map(|event| event.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let ours = log
        .iter()
        .filter(|event| ids.contains(event.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(ours.len(), 3, "replayed events are not appended twice");
    let unique = ours
        .iter()
        .map(|event| event.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), 3);
    assert_eq!(
        ours.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
        run.events.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
        "the log keeps the adapter order"
    );
    assert!(matches!(second.shutdown(), GatewayOutcome::Closed { .. }));
}

#[test]
fn a_failed_push_keeps_the_job_retryable() {
    let home = Home::new(HomeOptions::default());
    let gateway = home.connect();
    gateway.send(&AgentFrame::Heartbeat {
        at: ts(),
        capacity: capacity(),
        pending_spool_bytes: 0,
    });
    let (assignment, lease_token, _) = home.expect_assign(&gateway, 0);
    home.accept(&gateway, &assignment);
    let run = home.run_job(
        &assignment,
        &[("src/lib.rs", "pub fn retryable() -> bool {\n    true\n}\n")],
    );
    // The node commits locally but its push fails, so no job branch reaches the origin.
    home.hello(&gateway, &assignment, &lease_token);
    send_events(&gateway, &assignment, &run.events, 0..run.events.len());
    home.finishing(&gateway, &assignment, &run.head);
    home.turn_end(&gateway, &assignment, summary_push_failed());

    let job = home.job();
    assert_eq!(job.state, JobState::Ready, "a failed push keeps the job retryable");
    assert_eq!(job.attempts, 0);
    assert_eq!(job.continues, 0);
    assert!(job.note.contains("git_push_failed"), "note: {}", job.note);
    assert!(job.worktree.is_none(), "no staging worktree is published");
    assert!(run.worktree.is_dir(), "the node worktree remains for inspection");
    let branch = format!("refs/heads/job/{}", home.job.id);
    let pushed = git_run(&home.origin, &["show-ref", "--verify", "--quiet", &branch]);
    assert_ne!(pushed.exit_code, Some(0), "the failed push never reached the origin");
    assert_ne!(job.state, JobState::Merging);
    let record = result::load(&home.store, &assignment.run_id)
        .expect("result record")
        .expect("failed record");
    assert_eq!(record.status, "failed");
    assert!(
        record
            .detail
            .starts_with("git_push_failed: the agent could not push the job branch"),
        "detail: {}",
        record.detail
    );
    let assignment_row = home.assignment(&assignment.run_id);
    assert_eq!(assignment_row.state, AgentAssignmentState::Finished);
    assert!(home.store.lease(&assignment.session_id).expect("lease").is_none());
    let merges = home
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Merge),
            job_id: Some(home.job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger");
    assert!(merges.is_empty());
    assert!(matches!(gateway.shutdown(), GatewayOutcome::Closed { .. }));
}

#[test]
fn a_head_outside_the_job_owns_fails_the_result_gate() {
    let home = Home::new(HomeOptions::default());
    let gateway = home.connect();
    gateway.send(&AgentFrame::Heartbeat {
        at: ts(),
        capacity: capacity(),
        pending_spool_bytes: 0,
    });
    let (assignment, lease_token, _) = home.expect_assign(&gateway, 0);
    home.accept(&gateway, &assignment);
    let run = home.run_job(
        &assignment,
        &[
            ("src/lib.rs", "pub fn owned() {}\n"),
            ("notes/outside.txt", "not in the job brief\n"),
        ],
    );
    home.push_job(&assignment, &run);
    home.hello(&gateway, &assignment, &lease_token);
    send_events(&gateway, &assignment, &run.events, 0..run.events.len());
    home.finishing(&gateway, &assignment, &run.head);
    home.turn_end(&gateway, &assignment, summary_ok());

    let job = home.job();
    assert_eq!(job.state, JobState::Ready, "a gate failure keeps the job retryable");
    assert!(job.note.contains("result_gate_failed"), "note: {}", job.note);
    assert!(job.note.contains("owns_respected"), "note: {}", job.note);
    assert!(job.note.contains("notes/outside.txt"), "note: {}", job.note);
    assert!(job.worktree.is_none(), "the staging worktree is removed for a retry");
    assert_ne!(job.state, JobState::Merging);
    let gates = home
        .store
        .gate_results_for(&home.job.id, job.attempts)
        .expect("gate results");
    let owns = gates
        .iter()
        .find(|gate| gate.gate.kind() == GateKind::OwnsRespected)
        .expect("owns result");
    assert!(!owns.passed);
    let secrets = gates
        .iter()
        .find(|gate| gate.gate.kind() == GateKind::SecretScan)
        .expect("secret result");
    assert!(secrets.passed);
    let merges = home
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Merge),
            job_id: Some(home.job.id.clone()),
            ..LedgerFilter::default()
        })
        .expect("merge ledger");
    assert!(merges.is_empty(), "a failed gate never merges");
    let record = result::load(&home.store, &assignment.run_id)
        .expect("result record")
        .expect("failed record");
    assert_eq!(record.status, "failed");
    assert!(
        record.detail.starts_with("result_gate_failed:"),
        "detail: {}",
        record.detail
    );
    assert!(matches!(gateway.shutdown(), GatewayOutcome::Closed { .. }));
}

/// Asserts that a session log has no sequence gap.
fn assert_no_gaps(log: &[vowp_protocol::EventEnvelope]) {
    assert!(!log.is_empty(), "the session log has framing events");
    for (index, event) in log.iter().enumerate() {
        assert_eq!(
            event.seq,
            u64::try_from(index).expect("log index") + 1,
            "the session log is gapless at index {index}"
        );
    }
}

/// Runs git in `cwd`, asserting success.
fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = git_run(cwd, args);
    assert_eq!(output.exit_code, Some(0), "git {args:?} failed: {}", output.text());
    output.text()
}

/// Runs git in `cwd` and returns its trimmed output.
fn git_output(cwd: &Path, args: &[&str]) -> String {
    git_ok(cwd, args)
}

/// Runs git in `cwd` without asserting success.
fn git_run(cwd: &Path, args: &[&str]) -> CommandOutput {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    SystemRunner
        .run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: git_env(),
            timeout: Duration::from_secs(60),
        })
        .expect("git")
}

/// Encodes bytes as lowercase hex for `BundleChunk.data`.
fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    text
}

/// Environment that keeps git hermetic: no user or system config, fixed identity.
fn git_env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_AUTHOR_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_AUTHOR_EMAIL".to_owned(), "test@example.invalid".to_owned()),
        ("GIT_COMMITTER_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_COMMITTER_EMAIL".to_owned(), "test@example.invalid".to_owned()),
    ]
}

/// Creates a real repository on `main` with one initial commit.
fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("repo dir");
    git_ok(repo, &["init", "-b", "main"]);
    write(&repo.join("README.md"), "base\n");
    write(&repo.join("src/base.rs"), "pub fn base() {}\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "-m", "initial"]);
}

/// Creates a bare repository to act as `origin`.
fn init_bare(path: &Path) {
    fs::create_dir_all(path).expect("bare dir");
    git_ok(path, &["init", "--bare", "-b", "main"]);
}

/// Writes `text` to `path`, creating parent directories.
fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, text).expect("write file");
}
