//! Process plumbing tests: spawning, reader threads, tees, signals and env filtering.
//! Spec: `docs/design/crates/vowp-adapters.md` §7 and `docs/design/adapters.md` §1.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use vowp_adapters::proc::{RunHandle, Signal, SpawnSpec, child_env, resolve_binary, signal_group};
use vowp_adapters::stream::StreamParser;
use vowp_protocol::{
    Actor, AdapterError, AdapterRun, Cost, EventBody, HarnessEvent, Note, RunOutcome, RunSummary, StopReason, TurnEnded,
};

/// A parser that turns every stdout line into a `Note`.
struct StubParser {
    lines: Arc<AtomicUsize>,
}

impl StubParser {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let lines = Arc::new(AtomicUsize::new(0));
        (Self { lines: lines.clone() }, lines)
    }
}

impl StreamParser for StubParser {
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        self.lines.fetch_add(1, Ordering::SeqCst);
        vec![HarnessEvent {
            ts: None,
            actor: Actor::System,
            body: EventBody::Note(Note { text: line.to_owned() }),
            raw: None,
        }]
    }

    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, _stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary) {
        let (outcome, stop_reason) = if killed {
            (RunOutcome::Killed, StopReason::Interrupted)
        } else {
            match exit {
                Some(0) => (RunOutcome::Ok, StopReason::EndTurn),
                Some(_) => (RunOutcome::Error, StopReason::Error),
                None => (RunOutcome::Crash, StopReason::Crash),
            }
        };
        (
            vec![HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason,
                    duration_ms: None,
                }),
                raw: None,
            }],
            RunSummary {
                exit_code: exit,
                outcome,
                native_session_id: None,
                cost: Cost::unknown(),
                limit: None,
                result_text: None,
                stop_reason,
            },
        )
    }
}

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes").join(name)
}

fn spec(binary: &Path, cwd: &Path, env: &[(&str, String)]) -> SpawnSpec {
    let env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect::<Vec<_>>();
    SpawnSpec {
        argv: vec![binary.to_string_lossy().into_owned()],
        cwd: cwd.to_path_buf(),
        env: child_env(&env),
        stdin: None,
        tee_stdout: None,
        tee_stderr: None,
        timeout: None,
    }
}

fn spawn_test(spawn: SpawnSpec, parser: Box<dyn StreamParser>) -> RunHandle {
    RunHandle::spawn_with_grace(spawn, parser, Duration::from_millis(300)).expect("spawn fake harness")
}

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::write(path, contents).expect("write fixture");
}

fn notes(events: &[HarnessEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Note(note) => Some(note.text.clone()),
            _ => None,
        })
        .collect()
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if !contents.trim().is_empty() {
                return contents.trim().to_owned();
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_until_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still alive");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn own_process_group() -> u32 {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid])
        .output()
        .expect("run ps");
    assert!(
        output.status.success(),
        "ps failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("ps prints a numeric process group")
}

#[test]
fn proc_spawn_collects_events_and_exit() {
    let temp = TempDir::new().expect("temp dir");
    let fixture = temp.path().join("three.jsonl");
    write_file(&fixture, b"one\ntwo\nthree\n");

    let (parser, line_count) = StubParser::new();
    let spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[("FAKE_FIXTURE", fixture.to_string_lossy().into_owned())],
    );
    let mut handle = spawn_test(spawn, Box::new(parser));

    let summary = handle.wait().expect("wait for fake harness");
    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(line_count.load(Ordering::SeqCst), 3);

    let events = handle.events().try_iter().collect::<Vec<_>>();
    assert_eq!(notes(&events), vec!["one", "two", "three"]);
    assert!(
        matches!(events.last().map(|event| &event.body), Some(EventBody::TurnEnded(ended)) if ended.stop_reason == StopReason::EndTurn)
    );
    assert!(matches!(handle.events().try_recv(), Err(TryRecvError::Disconnected)));
}

#[test]
fn proc_tees_raw_stdout_before_parsing() {
    let temp = TempDir::new().expect("temp dir");
    let fixture = temp.path().join("mixed.jsonl");
    let raw = b"{\"a\":1}\nnot json\n{\"b\":2}\n";
    write_file(&fixture, raw);
    let tee = temp.path().join("stdout.log");

    let (parser, line_count) = StubParser::new();
    let mut spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[("FAKE_FIXTURE", fixture.to_string_lossy().into_owned())],
    );
    spawn.tee_stdout = Some(tee.clone());
    let mut handle = spawn_test(spawn, Box::new(parser));
    handle.wait().expect("wait for fake harness");

    assert_eq!(std::fs::read(&tee).expect("read tee file"), raw);
    assert_eq!(line_count.load(Ordering::SeqCst), 3);
}

#[test]
fn proc_stop_is_idempotent_and_kills_group() {
    let temp = TempDir::new().expect("temp dir");
    let fixture = temp.path().join("started.jsonl");
    write_file(&fixture, b"started\n");
    let child_pid_file = temp.path().join("child.pid");

    let (parser, _) = StubParser::new();
    let spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[
            ("FAKE_FIXTURE", fixture.to_string_lossy().into_owned()),
            ("FAKE_HANG", "1".to_owned()),
            ("FAKE_SPAWN_CHILD", "1".to_owned()),
            ("FAKE_CHILD_PID_OUT", child_pid_file.to_string_lossy().into_owned()),
        ],
    );
    let mut handle = spawn_test(spawn, Box::new(parser));
    let main_pid = handle.pid().expect("child pid");
    let child_pid = wait_for_file(&child_pid_file)
        .parse::<u32>()
        .expect("child pid is a number");

    handle.stop().expect("stop the run");
    let summary = handle.wait().expect("wait after stop");
    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);

    wait_until_dead(main_pid);
    wait_until_dead(child_pid);
    handle.stop().expect("second stop is a no-op");
    assert_eq!(handle.wait().expect("second wait").outcome, RunOutcome::Killed);
}

#[test]
fn proc_stop_escalates_to_sigkill() {
    let temp = TempDir::new().expect("temp dir");
    let fixture = temp.path().join("started.jsonl");
    write_file(&fixture, b"started\n");

    let (parser, _) = StubParser::new();
    let spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[
            ("FAKE_FIXTURE", fixture.to_string_lossy().into_owned()),
            ("FAKE_HANG", "ignoreterm".to_owned()),
        ],
    );
    let mut handle = spawn_test(spawn, Box::new(parser));
    let pid = handle.pid().expect("child pid");
    handle
        .events()
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");

    let started = Instant::now();
    handle.stop().expect("stop the run");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "stop took {elapsed:?}");
    assert_eq!(handle.wait().expect("wait after stop").outcome, RunOutcome::Killed);
    wait_until_dead(pid);
}

#[test]
fn proc_interrupt_then_stop() {
    let temp = TempDir::new().expect("temp dir");
    let fixture = temp.path().join("started.jsonl");
    write_file(&fixture, b"started\n");

    let (parser, _) = StubParser::new();
    let spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[
            ("FAKE_FIXTURE", fixture.to_string_lossy().into_owned()),
            ("FAKE_HANG", "ignoreint".to_owned()),
        ],
    );
    let mut handle = spawn_test(spawn, Box::new(parser));
    let pid = handle.pid().expect("child pid");
    handle
        .events()
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");

    let started = Instant::now();
    handle.interrupt().expect("interrupt the run");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "interrupt took {elapsed:?}");
    assert_eq!(handle.wait().expect("wait after interrupt").outcome, RunOutcome::Killed);
    handle.stop().expect("stop after interrupt is a no-op");
    wait_until_dead(pid);
}

#[test]
fn proc_timeout_kills_and_marks_killed() {
    let temp = TempDir::new().expect("temp dir");
    let (parser, _) = StubParser::new();
    let mut spawn = spec(&fake("fake-claude.sh"), temp.path(), &[("FAKE_HANG", "1".to_owned())]);
    spawn.timeout = Some(Duration::from_secs(1));
    let mut handle = spawn_test(spawn, Box::new(parser));

    let started = Instant::now();
    let summary = handle.wait().expect("wait for timeout");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);
}

#[test]
fn proc_env_is_filtered() {
    let temp = TempDir::new().expect("temp dir");
    let (parser, _) = StubParser::new();
    let spawn = SpawnSpec {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), "env".to_owned()],
        cwd: temp.path().to_path_buf(),
        env: child_env(&[("VOWP_TEST_PASSED".to_owned(), "yes".to_owned())]),
        stdin: None,
        tee_stdout: None,
        tee_stderr: None,
        timeout: None,
    };
    let mut handle = spawn_test(spawn, Box::new(parser));
    handle.wait().expect("wait for env");
    let events = handle.events().try_iter().collect::<Vec<_>>();
    let env = notes(&events)
        .into_iter()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            Some((name.to_owned(), value.to_owned()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
    assert_eq!(env.get("VOWP_TEST_PASSED").map(String::as_str), Some("yes"));
    assert!(env.contains_key("PATH"));

    let whitelist = [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ];
    let canary = std::env::vars()
        .find(|(name, _)| !whitelist.contains(&name.as_str()) && !name.starts_with("FAKE_"))
        .expect("the test process has at least one non-whitelisted variable");
    assert!(
        !env.contains_key(&canary.0),
        "non-whitelisted variable {} leaked into the child",
        canary.0
    );
}

#[test]
fn proc_spawn_failure_is_typed_error() {
    let temp = TempDir::new().expect("temp dir");
    let (parser, _) = StubParser::new();
    let spawn = spec(&temp.path().join("definitely-not-a-binary"), temp.path(), &[]);
    let error = match RunHandle::spawn(spawn, Box::new(parser)) {
        Ok(_) => panic!("missing binary must fail"),
        Err(error) => error,
    };
    assert!(
        matches!(error, AdapterError::Spawn { .. } | AdapterError::Unavailable(_)),
        "unexpected error: {error}"
    );
}

#[test]
fn resolve_binary_finds_on_path_and_rejects_non_executable() {
    assert!(resolve_binary("sh").is_some(), "sh should be on PATH");
    assert!(resolve_binary("/definitely/not/a/vowp-binary").is_none());

    let temp = TempDir::new().expect("temp dir");
    let plain = temp.path().join("plain-file");
    write_file(&plain, b"#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod plain");
    assert_eq!(resolve_binary(&plain.to_string_lossy()), None);

    let executable = temp.path().join("executable-file");
    write_file(&executable, b"#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("chmod executable");
    assert_eq!(resolve_binary(&executable.to_string_lossy()), Some(executable.clone()));
    assert!(executable.exists());
}

#[test]
fn signal_group_refuses_invalid_pids_and_own_group() {
    for pgid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
        let error = signal_group(pgid, Signal::Term).expect_err("must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "pgid {pgid}");
    }

    let own = own_process_group();
    assert!(own > 1, "test process group is {own}");
    let error = signal_group(own, Signal::Term).expect_err("must refuse own group");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn stdin_is_written_and_closed() {
    let temp = TempDir::new().expect("temp dir");
    let stdin_out = temp.path().join("stdin.txt");
    let (parser, _) = StubParser::new();
    let mut spawn = spec(
        &fake("fake-claude.sh"),
        temp.path(),
        &[("FAKE_STDIN_OUT", stdin_out.to_string_lossy().into_owned())],
    );
    spawn.stdin = Some("hello\nworld\n".as_bytes().to_vec());
    let mut handle = spawn_test(spawn, Box::new(parser));
    handle.wait().expect("wait for fake harness");
    assert_eq!(
        std::fs::read_to_string(&stdin_out).expect("read stdin capture"),
        "hello\nworld\n"
    );
}

#[test]
fn run_capture_collects_output_and_exit() {
    let argv = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'out'; printf 'err' >&2; exit 7".to_owned(),
    ];
    let (exit, stdout, stderr) = vowp_adapters::proc::run_capture(&argv, Duration::from_secs(2)).expect("run capture");
    assert_eq!(exit, Some(7));
    assert_eq!(stdout, "out");
    assert_eq!(stderr, "err");
}
