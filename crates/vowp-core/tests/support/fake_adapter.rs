//! Shared fake adapter for `vowp-core` integration tests.
//!
//! It replays a fixed event list through the real [`Adapter`] trait and records every
//! [`StartInput`], so tests can assert what the session layer passed to a harness without
//! spawning a process.

#![allow(dead_code)]

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, Cost, EventBody, HarnessEvent, HarnessKind,
    ProbeResult, RunOutcome, RunSummary, StartInput, StopReason, Timestamp,
};

/// An adapter that replays `events` and returns a canned summary.
pub struct FakeAdapter {
    kind: HarnessKind,
    events: Vec<HarnessEvent>,
    summary: RunSummary,
    probe: ProbeResult,
    start_error: Option<String>,
    starts: Arc<Mutex<Vec<StartInput>>>,
}

impl FakeAdapter {
    /// Creates a fake adapter with a successful default summary.
    pub fn new(kind: HarnessKind, events: Vec<HarnessEvent>) -> Self {
        Self {
            kind,
            events,
            summary: RunSummary {
                exit_code: Some(0),
                outcome: RunOutcome::Ok,
                native_session_id: None,
                cost: Cost::unknown(),
                limit: None,
                result_text: None,
                stop_reason: StopReason::EndTurn,
            },
            probe: ProbeResult {
                ok: true,
                version: Some("fake-1.0".to_owned()),
                path: Some("fake-harness".to_owned()),
                detail: "fake adapter".to_owned(),
            },
            start_error: None,
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Creates a fake adapter from typed event bodies, timestamped at `ts`.
    pub fn from_bodies(kind: HarnessKind, ts: Timestamp, bodies: Vec<EventBody>) -> Self {
        let events = bodies
            .into_iter()
            .map(|body| HarnessEvent {
                ts: Some(ts),
                actor: Actor::Assistant,
                body,
                raw: None,
            })
            .collect();
        Self::new(kind, events)
    }

    /// Replaces the summary returned by [`AdapterRun::wait`].
    pub fn with_summary(mut self, summary: RunSummary) -> Self {
        self.summary = summary;
        self
    }

    /// Replaces the probe result.
    pub fn with_probe(mut self, probe: ProbeResult) -> Self {
        self.probe = probe;
        self
    }

    /// Makes [`Adapter::start`] fail with [`AdapterError::Unavailable`].
    pub fn with_start_error(mut self, message: impl Into<String>) -> Self {
        self.start_error = Some(message.into());
        self
    }

    /// Returns every [`StartInput`] passed to [`Adapter::start`], in call order.
    pub fn starts(&self) -> Vec<StartInput> {
        self.starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Adapter for FakeAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        self.probe.clone()
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input);
        if let Some(message) = &self.start_error {
            return Err(AdapterError::Unavailable(message.clone()));
        }
        let (sender, receiver) = channel();
        for event in &self.events {
            let _ = sender.send(event.clone());
        }
        drop(sender);
        Ok(Box::new(FakeRun {
            receiver,
            summary: self.summary.clone(),
        }))
    }
}

/// One replay run returned by [`FakeAdapter::start`].
struct FakeRun {
    receiver: Receiver<HarnessEvent>,
    summary: RunSummary,
}

impl AdapterRun for FakeRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("fake adapter has no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(self.summary.clone())
    }
}
