//! Recorded-fixture helpers for adapter tests.
//! Spec: `docs/design/adapters.md` §9 and `docs/design/crates/vowp-adapters.md` §1.
//!
//! [`parse_fixture`] feeds a recorded `*.jsonl` file (and its optional
//! `<name>.meta.json` sidecar carrying `exit` and `stderr`) through the matching parser.
//! It is available under `cfg(test)` and under the `testing` cargo feature so `vowp-core`
//! tests can reuse recorded runs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use vowp_protocol::{
    Cost, EventBody, Executor, HarnessEvent, HarnessKind, Note, RunOutcome, RunSummary, StopReason, Timestamp,
};

use crate::claude_code::ClaudeParser;
use crate::cli::{CliParser, Mapping};
use crate::opencode::OpencodeParser;
use crate::stream::{ParsedRun, StreamParser};

/// Which adapter parser should consume a recorded fixture.
///
/// The CLI parser needs its mapping, so that variant carries one.
pub enum FixtureKind {
    /// Claude Code `stream-json` fixture.
    ClaudeCode,
    /// opencode `--format json` fixture.
    Opencode,
    /// Generic CLI fixture parsed with the supplied mapping.
    Cli(Mapping),
}

/// Parses a recorded run, applying its metadata sidecar when present.
pub fn parse_fixture(kind: FixtureKind, path: &Path) -> ParsedRun {
    let now = Timestamp::from_unix_secs(0);
    let mut parser: Box<dyn StreamParser> = match kind {
        FixtureKind::ClaudeCode => Box::new(ClaudeParser::new(now, 0, fixture_executor(HarnessKind::ClaudeCode))),
        FixtureKind::Opencode => Box::new(OpencodeParser::new(now, 0, fixture_executor(HarnessKind::Opencode))),
        FixtureKind::Cli(mapping) => match CliParser::new(mapping, now) {
            Ok(parser) => Box::new(parser),
            Err(error) => return error_run(error.to_string()),
        },
    };

    let meta = read_meta(path);
    let exit = meta
        .as_ref()
        .and_then(|value| value.get("exit"))
        .and_then(serde_json::Value::as_i64);
    let exit = exit.and_then(|code| i32::try_from(code).ok());
    let stderr = meta
        .as_ref()
        .and_then(|value| value.get("stderr"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return error_run(format!("cannot read fixture {}: {error}", path.display())),
    };

    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        match line {
            Ok(line) => events.extend(parser.feed_line(&line)),
            Err(error) => return error_run(format!("cannot read fixture {}: {error}", path.display())),
        }
    }
    let (terminal, summary) = parser.finish(exit, false, &stderr);
    events.extend(terminal);
    ParsedRun { events, summary }
}

fn fixture_executor(harness: HarnessKind) -> Executor {
    Executor {
        harness,
        model: "fixture".to_owned(),
        effort: None,
        variant: None,
    }
}

/// Reads `<name>.meta.json` for a fixture, trying both common sidecar spellings.
fn read_meta(path: &Path) -> Option<serde_json::Value> {
    let candidates = [path.with_extension("meta.json"), appended(path, ".meta.json")];
    for candidate in candidates {
        if let Ok(text) = std::fs::read_to_string(candidate) {
            return serde_json::from_str(&text).ok();
        }
    }
    None
}

fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn error_run(message: String) -> ParsedRun {
    ParsedRun {
        events: vec![HarnessEvent {
            ts: None,
            actor: vowp_protocol::Actor::System,
            body: EventBody::Note(Note { text: message.clone() }),
            raw: None,
        }],
        summary: RunSummary {
            exit_code: None,
            outcome: RunOutcome::Error,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: Some(message),
            stop_reason: StopReason::Crash,
        },
    }
}
