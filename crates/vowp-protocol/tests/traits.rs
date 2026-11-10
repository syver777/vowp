//! Adapter and service contract tests.
//! Spec: `docs/design/types.md` §12–13 and `docs/design/crates/vowp-protocol.md` §6.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use pretty_assertions::assert_eq;

use vowp_protocol::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterRun, ProbeResult, RunSummary, StartInput,
};
use vowp_protocol::cost::Cost;
use vowp_protocol::event::{Actor, EventBody, HarnessEvent, Note, StopReason};
use vowp_protocol::executor::{Executor, HarnessKind};
use vowp_protocol::ids::SessionId;
use vowp_protocol::run::RunOutcome;
use vowp_protocol::services::{CancelToken, ServiceError};
use vowp_protocol::time::Timestamp;

/// A fake adapter run that already produced one event and then closed its channel.
struct FakeRun {
    pid: Option<u32>,
    events: Receiver<HarnessEvent>,
}

impl AdapterRun for FakeRun {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.events
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(RunSummary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: Some("native-1".to_owned()),
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
        })
    }
}

/// A fake adapter that echoes the prompt as a `note` event.
struct FakeAdapter;

impl Adapter for FakeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Cli
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("1.0.0".to_owned()),
            path: Some("/bin/fake".to_owned()),
            detail: "fake harness".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: true,
            ..AdapterCapabilities::default()
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(HarnessEvent {
                ts: Some(Timestamp::from_unix_secs(0)),
                actor: Actor::System,
                body: EventBody::Note(Note { text: input.prompt }),
                raw: None,
            })
            .expect("receiver is alive");
        drop(sender);
        Ok(Box::new(FakeRun {
            pid: Some(4242),
            events: receiver,
        }))
    }
}

#[test]
fn adapter_roundtrips_through_trait_object() {
    let adapter: Box<dyn Adapter> = Box::new(FakeAdapter);
    assert_eq!(adapter.kind(), HarnessKind::Cli);
    let probe = adapter.probe();
    assert!(probe.ok);
    assert_eq!(probe.version.as_deref(), Some("1.0.0"));
    let capabilities = adapter.capabilities();
    assert!(capabilities.can_resume_native);
    assert!(!capabilities.live_input);

    let input = StartInput {
        session_id: SessionId::new(),
        executor: Executor::parse("cli:fake").expect("executor parses"),
        cwd: PathBuf::from("/tmp"),
        prompt: "hello".to_owned(),
        resume: None,
        new_native_id: None,
        env: Vec::new(),
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    };
    let mut run = adapter.start(input).expect("start succeeds");
    assert_eq!(run.pid(), Some(4242));

    let event = run.events().recv().expect("one event");
    assert_eq!(event.actor, Actor::System);
    assert_eq!(
        event.body,
        EventBody::Note(Note {
            text: "hello".to_owned()
        })
    );
    assert!(run.events().recv().is_err(), "the run closes its event channel");

    let summary = run.wait().expect("wait succeeds");
    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(summary.cost, Cost::unknown());
    assert_eq!(summary.native_session_id.as_deref(), Some("native-1"));
    assert_eq!(summary.result_text.as_deref(), Some("done"));
}

#[test]
fn service_cancel_token() {
    let token = CancelToken::default();
    let clone = token.clone();
    assert!(!token.is_cancelled());
    assert!(!clone.is_cancelled());

    clone.cancel();
    assert!(token.is_cancelled(), "clones share the cancellation flag");
    assert!(clone.is_cancelled());

    let later = token.clone();
    assert!(later.is_cancelled(), "a clone made after cancel is cancelled");

    assert_eq!(ServiceError::Failed("boom".to_owned()).to_string(), "boom");
    assert_eq!(
        ServiceError::NotFound("job sample".to_owned()).to_string(),
        "not found: job sample"
    );
    assert_eq!(ServiceError::Cancelled.to_string(), "cancelled");
}

#[test]
fn adapter_error_display_strings() {
    assert_eq!(
        AdapterError::Unavailable("claude".to_owned()).to_string(),
        "harness not found or not runnable: claude"
    );
    assert_eq!(
        AdapterError::Unsupported("live input").to_string(),
        "unsupported: live input"
    );
    assert_eq!(
        AdapterError::Protocol("bad json".to_owned()).to_string(),
        "protocol violation from harness: bad json"
    );
    assert_eq!(
        AdapterError::Mapping {
            path: "x.toml".to_owned(),
            message: "missing argv".to_owned()
        }
        .to_string(),
        "invalid mapping file x.toml: missing argv"
    );
}
