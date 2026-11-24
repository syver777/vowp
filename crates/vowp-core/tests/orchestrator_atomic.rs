//! `orchestrator.json` is written atomically: a reader never sees an empty or truncated
//! document while the home rewrites the record.
//!
//! Every test uses a temporary home and a remote-node record, so no test touches the real
//! `~/.vowp`, a store, a socket, an app or the network.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use tempfile::TempDir;
use vowp_core::surface::orchestrator::Orchestrator;
use vowp_core::{AdapterRegistry, CoreError, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{FakeClock, MemoryLog, NodeName, OrchestratorNodeState, OrchestratorResult, Timestamp, VowpConfig};
use vowp_store::NodePaths;

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const REQUEST_ID: &str = "req-atomic-1";
const NODE: &str = "worker-1";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-19T12:00:00Z").expect("test timestamp")
}

fn deps(home: &Path) -> Deps {
    Deps {
        paths: NodePaths::new(home),
        config: Arc::new(VowpConfig::with_defaults_for_tests()),
        node: NodeName::parse("home-1").expect("home node"),
        clock: Arc::new(FakeClock::at(ts())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(SystemGit::default()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

/// A home with one remote-node orchestrator record whose request id is still current.
struct Fixture {
    _home: TempDir,
    dir: std::path::PathBuf,
    manager: Orchestrator,
    node: NodeName,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let dir = home.path().join("orchestrators").join(name);
        fs::create_dir_all(&dir).expect("orchestrator dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("private orchestrator dir");
        let state = serde_json::json!({
            "name": name,
            "app": "opencode",
            "session_id": SESSION,
            "state": "running",
            "created_at": ts(),
            "node": NODE,
            "last_request_id": REQUEST_ID,
        });
        let path = dir.join("orchestrator.json");
        fs::write(&path, serde_json::to_vec(&state).expect("state JSON")).expect("initial state");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private state");
        let manager = Orchestrator::with_hosts(deps(home.path()), Vec::new());
        Self {
            _home: home,
            dir,
            manager,
            node: NodeName::parse(NODE).expect("node"),
        }
    }

    fn state_path(&self) -> std::path::PathBuf {
        self.dir.join("orchestrator.json")
    }

    fn result(detail: impl Into<String>) -> OrchestratorResult {
        OrchestratorResult {
            request_id: REQUEST_ID.to_owned(),
            accepted: true,
            state: OrchestratorNodeState::Running,
            error: None,
            detail: Some(detail.into()),
        }
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// Sets the writer-done flag even when the writer panics, so the reader cannot spin forever.
struct WriterDone(Arc<AtomicBool>);

impl Drop for WriterDone {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_reader_never_sees_an_empty_or_truncated_record_while_it_is_rewritten() {
    let fixture = Arc::new(Fixture::new("race"));
    let state_path = fixture.state_path();
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let writer = {
        let fixture = Arc::clone(&fixture);
        let barrier = Arc::clone(&barrier);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let _done = WriterDone(done);
            barrier.wait();
            // The payload grows from one byte to the helper's 512-character detail cap,
            // so the old truncate-and-write helper exposes a visible gap. A four-byte
            // step keeps the test well under a second while still growing every write.
            for width in (1..=512).step_by(4) {
                let result = Fixture::result("x".repeat(width));
                fixture
                    .manager
                    .apply_result(&fixture.node, &result)
                    .expect("atomic state write");
            }
        })
    };

    let reader = {
        let barrier = Arc::clone(&barrier);
        let done = Arc::clone(&done);
        let reads = Arc::clone(&reads);
        thread::spawn(move || {
            barrier.wait();
            loop {
                let bytes = fs::read(&state_path).expect("the record exists while it is rewritten");
                assert!(
                    !bytes.is_empty(),
                    "the reader saw an empty orchestrator.json (the writer truncated it)"
                );
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                    panic!(
                        "the reader saw a truncated orchestrator.json: {error}; bytes: {:?}",
                        String::from_utf8_lossy(&bytes)
                    )
                });
                assert_eq!(value["name"], "race");
                reads.fetch_add(1, Ordering::Relaxed);
                if done.load(Ordering::SeqCst) {
                    break;
                }
            }
        })
    };

    writer.join().expect("writer thread");
    reader.join().expect("reader thread");
    assert!(reads.load(Ordering::Relaxed) > 0, "the reader never ran");
    assert_eq!(mode(&fixture.state_path()), 0o600, "the record stays private");
    assert_eq!(mode(&fixture.dir), 0o700, "the orchestrator directory stays private");
}

#[test]
fn a_failed_write_leaves_no_temp_file_and_returns_the_typed_error() {
    let fixture = Fixture::new("write-failure");
    let before = fs::read(fixture.state_path()).expect("state before the failure");
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o555)).expect("read-only orchestrator dir");

    let result = fixture
        .manager
        .apply_result(&fixture.node, &Fixture::result("must not land"));

    // Restore the mode before asserting so a failing assertion cannot leave an
    // unremovable temporary directory behind.
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).expect("restore orchestrator dir");
    let error = result.expect_err("a read-only directory refuses the temporary file");
    match error {
        CoreError::Io { path, source } => {
            assert!(
                path.ends_with("orchestrator.json"),
                "the typed error names the target: {path}"
            );
            assert_eq!(
                source.kind(),
                std::io::ErrorKind::PermissionDenied,
                "the failure is a permission error: {source}"
            );
        }
        other => panic!("expected a typed I/O error, got {other:?}"),
    }

    let leftovers: Vec<String> = fs::read_dir(&fixture.dir)
        .expect("read orchestrator dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
    assert_eq!(
        fs::read(fixture.state_path()).expect("state after the failure"),
        before,
        "a failed write must not touch the existing record"
    );
}

#[test]
fn an_atomic_write_keeps_the_file_private_and_the_directory_mode_unchanged() {
    let fixture = Fixture::new("modes");
    fixture
        .manager
        .apply_result(&fixture.node, &Fixture::result("private detail"))
        .expect("state write");
    assert_eq!(mode(&fixture.state_path()), 0o600);
    assert_eq!(mode(&fixture.dir), 0o700);
}
