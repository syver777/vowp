//! Path-map config, enrollment and update tests.
//!
//! Every checkout, state directory and transport is temporary and in memory. No test reads the
//! real `~/.vowp-agent`, opens SSH or starts a harness.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vowp_agent::{AgentClient, AgentClock, AgentConfig, FrameRead, FrameStream, NodeKey, Transport};
use vowp_protocol::{AGENT_PROTOCOL, AgentFrame, AgentPathMap, NodeName, ProjectName, Timestamp};

const CODE: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

#[derive(Default)]
struct FakeState {
    connections: usize,
    received: Vec<AgentFrame>,
}

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<FakeState>>,
    clock: FixedClock,
}

impl FakeHome {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(FakeState::default())),
            clock: FixedClock(ts()),
        }
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn path_updates(&self) -> Vec<AgentPathMap> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                AgentFrame::PathMapUpdate { map } => Some(map),
                _ => None,
            })
            .collect()
    }
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, vowp_agent::AgentError> {
        let connection = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connections += 1;
            state.connections
        };
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            clock: self.clock,
            connection,
            outbound: VecDeque::new(),
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<FakeState>>,
    clock: FixedClock,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, vowp_agent::AgentError> {
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Timeout),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), vowp_agent::AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::Enroll { .. } => {
                self.outbound.push_back(AgentFrame::Enrolled {
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node slug"),
                    protocol: AGENT_PROTOCOL.to_owned(),
                    server_time: self.clock.now(),
                    heartbeat_secs: 10,
                });
            }
            AgentFrame::AgentHello { .. } => {
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: format!("nonce-{}", self.connection),
                    home_id: "home-1".to_owned(),
                    connection_id: format!("connection-{}", self.connection),
                    server_time: self.clock.now(),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                self.outbound.push_back(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node slug"),
                    connection_id: format!("connection-{}", self.connection),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

fn test_config(state_dir: &Path) -> AgentConfig {
    AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        state_dir.to_path_buf(),
        "home.example",
    )
}

/// Creates a temporary checkout with a `vowp.toml` and its default worktree root.
fn checkout(base: &Path, directory: &str, project: &str) -> PathBuf {
    let root = base.join(directory);
    fs::create_dir_all(root.join(".vowp/worktrees")).expect("worktree root");
    fs::write(root.join("vowp.toml"), format!("[project]\nname = \"{project}\"\n")).expect("vowp.toml");
    fs::canonicalize(root).expect("canonical checkout")
}

fn path_map(root: &Path, project: &str) -> AgentPathMap {
    let worktree_root = fs::canonicalize(root.join(".vowp/worktrees")).expect("canonical worktrees");
    AgentPathMap {
        project: ProjectName::parse(project).expect("project slug"),
        local_root: root.to_str().expect("utf-8 checkout").to_owned(),
        worktree_root: Some(worktree_root.to_str().expect("utf-8 worktrees").to_owned()),
        allow: Vec::new(),
    }
}

fn client(state_dir: &Path, config: AgentConfig, home: &FakeHome) -> AgentClient {
    let key = NodeKey::load_or_generate(state_dir).expect("node key");
    AgentClient::new(
        state_dir.to_path_buf(),
        key,
        config,
        Box::new(home.clone()),
        Arc::new(FixedClock(ts())),
    )
}

#[test]
fn agent_enroll_sends_the_configured_path_map() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    let root = checkout(dir.path(), "vowp", "vowp");
    let map = path_map(&root, "vowp");
    let home = FakeHome::new();
    let key = NodeKey::load_or_generate(&state_dir).expect("node key");
    let mut client = AgentClient::unenrolled(
        state_dir.clone(),
        key,
        Box::new(home.clone()),
        Arc::new(FixedClock(ts())),
    )
    .with_path_maps(vec![map.clone()]);

    let enrollment = client.enroll("home.example", CODE).expect("enroll");
    assert_eq!(enrollment.node.as_ref(), "mac-studio");

    let (paths, projects) = home
        .frames()
        .into_iter()
        .find_map(|frame| match frame {
            AgentFrame::Enroll {
                paths, capabilities, ..
            } => Some((paths, capabilities.projects)),
            _ => None,
        })
        .expect("Enroll frame");
    assert_eq!(paths, vec![map.clone()]);
    assert_eq!(projects, vec![map.project.clone()]);

    let stored = AgentConfig::load(&state_dir).expect("stored config");
    assert_eq!(stored.path_maps, vec![map]);
    let mode = fs::metadata(AgentConfig::file_path(&state_dir))
        .expect("agent.json")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "agent.json must stay private");
}

#[test]
fn agent_path_map_persists_0600_and_reloads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    let root = checkout(dir.path(), "vowp", "vowp");
    let map = path_map(&root, "vowp");
    let config = test_config(&state_dir).with_path_maps(vec![map.clone()]);

    config.write().expect("write agent.json");
    let mode = fs::metadata(AgentConfig::file_path(&state_dir))
        .expect("agent.json")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = fs::metadata(&state_dir).expect("state dir").permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "the state directory must stay private");

    let loaded = AgentConfig::load(&state_dir).expect("reload");
    assert_eq!(loaded.path_maps, vec![map]);
}

#[test]
fn agent_path_map_rejects_a_relative_root_a_traversal_and_a_missing_checkout() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    let root = checkout(dir.path(), "vowp", "vowp");
    let valid = path_map(&root, "vowp");

    let mut relative = valid.clone();
    relative.local_root = "relative/checkout".to_owned();
    let error = test_config(&state_dir)
        .with_path_maps(vec![relative])
        .write()
        .unwrap_err();
    assert!(
        error.to_string().contains("local root must be an absolute path"),
        "{error}"
    );

    let mut traversal = valid.clone();
    traversal.local_root = format!("{}/../elsewhere", valid.local_root);
    let error = test_config(&state_dir)
        .with_path_maps(vec![traversal])
        .write()
        .unwrap_err();
    assert!(error.to_string().contains("parent traversal"), "{error}");

    let mut missing = valid.clone();
    missing.local_root = dir.path().join("missing").to_str().expect("utf-8").to_owned();
    let error = test_config(&state_dir)
        .with_path_maps(vec![missing])
        .write()
        .unwrap_err();
    assert!(error.to_string().contains("local root is missing"), "{error}");

    let mut nul = valid.clone();
    nul.local_root = format!("{}\0x", valid.local_root);
    let error = test_config(&state_dir).with_path_maps(vec![nul]).write().unwrap_err();
    assert!(error.to_string().contains("NUL byte"), "{error}");

    let mut allow = valid;
    allow.allow = vec!["../src".to_owned()];
    let error = test_config(&state_dir).with_path_maps(vec![allow]).write().unwrap_err();
    assert!(error.to_string().contains("normalised relative path"), "{error}");
}

#[test]
fn agent_paths_add_update_and_remove_one_project() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    let root = checkout(dir.path(), "vowp", "vowp");
    let map = path_map(&root, "vowp");
    let project = map.project.clone();
    let mut config = test_config(&state_dir);
    config.write().expect("initial config");
    assert!(config.path_map(&project).is_none());

    config.set_path_map(map.clone()).expect("add");
    config.write().expect("save add");
    let loaded = AgentConfig::load(&state_dir).expect("reload add");
    assert_eq!(loaded.path_map(&project), Some(&map));

    let other_worktrees = dir.path().join("other-worktrees");
    fs::create_dir_all(&other_worktrees).expect("other worktrees");
    let other_worktrees = fs::canonicalize(other_worktrees).expect("canonical");
    let mut updated = map.clone();
    updated.worktree_root = Some(other_worktrees.to_str().expect("utf-8").to_owned());
    updated.allow = vec!["src".to_owned()];
    config.set_path_map(updated.clone()).expect("update");
    config.write().expect("save update");
    let loaded = AgentConfig::load(&state_dir).expect("reload update");
    assert_eq!(loaded.path_map(&project), Some(&updated));

    assert!(config.remove_path_map(&project));
    config.write().expect("save remove");
    let loaded = AgentConfig::load(&state_dir).expect("reload remove");
    assert!(loaded.path_map(&project).is_none());
    assert!(!config.remove_path_map(&project), "removing twice changes nothing");
}

#[test]
fn agent_path_map_update_is_sent_when_the_map_changes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    let first_root = checkout(dir.path(), "first", "vowp");
    let second_root = checkout(dir.path(), "second", "vowp");
    let first = path_map(&first_root, "vowp");
    let second = path_map(&second_root, "vowp");
    let config = test_config(&state_dir).with_path_maps(vec![first.clone()]);
    config.write().expect("agent.json");

    let home = FakeHome::new();
    let mut client = client(&state_dir, config, &home);
    client.connect().expect("handshake");
    assert!(home.path_updates().is_empty(), "connect alone does not resend the map");

    client.heartbeat().expect("first heartbeat");
    assert_eq!(
        home.path_updates(),
        vec![first.clone()],
        "a fresh run reports the saved map after the handshake"
    );

    let mut stored = AgentConfig::load(&state_dir).expect("reload");
    stored.path_maps = vec![second.clone()];
    stored.write().expect("save changed map");

    client.heartbeat().expect("heartbeat");
    let updates = home.path_updates();
    assert_eq!(
        updates,
        vec![first, second.clone()],
        "the changed map is sent once after the startup report"
    );

    client.heartbeat().expect("second heartbeat");
    let updates = home.path_updates();
    assert_eq!(updates.len(), 2, "an unchanged map is not resent");
    assert_eq!(updates.last(), Some(&second));
}
