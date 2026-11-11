//! Cross-database reconcile for the global + project store split.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.7.
//!
//! SQLite transactions are atomic per file, not across an attached database. Writes follow a
//! fixed order and this startup pass repairs the half-done pairs a crash can leave behind.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};
use vowp_protocol::{
    Actor, Cost, EventBody, EventId, EventIndexRow, EventSource, EventType, HarnessKind, LedgerEntry, LedgerKind, Note,
    Run, RunOutcome, SessionId, SessionState, Timestamp,
};

use crate::db::{RunFinish, Store};
use crate::error::StoreError;
use crate::session_log::{NewEvent, SessionLog, index_row};

/// What one [`Store::reconcile_cross_db`] pass repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossDbReconcile {
    /// Open runs whose session already recorded `turn.ended`.
    pub runs_closed: u32,
    /// Open runs whose session row is gone; closed as an error.
    pub runs_missing_session: u32,
    /// Sessions in `running` with no open run.
    pub sessions_repaired: u32,
    /// Closed runs whose session has no terminal event.
    pub terminal_events_repaired: u32,
    /// Finished runs with no cost ledger row.
    pub ledgers_repaired: u32,
    /// Leases whose session no longer exists.
    pub leases_dropped: u32,
}

/// Backwards-compatible alias used by early v0.2 call sites.
pub type ReconcileCounts = CrossDbReconcile;

impl Store {
    /// Repairs half-done cross-file pairs and returns what changed.
    ///
    /// The method is idempotent: a second pass on a healthy pair returns all zeros.
    pub fn reconcile_cross_db(&self) -> Result<CrossDbReconcile, StoreError> {
        let bound = self.require_bound()?;
        let mut report = CrossDbReconcile::default();

        // 1. An open run whose session already ended is closed as stalled.
        for run in self.open_runs()? {
            let Some(session_id) = &run.session_id else {
                continue;
            };
            match self.session(session_id)? {
                None => {
                    self.finish_reconcile_run(&run, RunOutcome::Error, "session missing during crash recovery")?;
                    report.runs_missing_session += 1;
                }
                Some(session) => {
                    if session.state == SessionState::Running
                        && self.has_event_since(session_id, EventType::TurnEnded, run.started_at)?
                    {
                        self.finish_reconcile_run(&run, RunOutcome::Stalled, "reconciled after crash")?;
                        report.runs_closed += 1;
                    }
                }
            }
        }

        // 2. A running session with no open run: note it and let dispatch resume.
        let open_runs = self.open_runs()?;
        let open_sessions: HashSet<SessionId> = open_runs.iter().filter_map(|run| run.session_id.clone()).collect();
        let running = self.sessions_for_reconcile(&bound, SessionState::Running)?;
        for session in running {
            if open_sessions.contains(&session.id) {
                continue;
            }
            self.append_reconcile_note(&session.id, "turn interrupted during crash recovery")?;
            self.set_reconcile_state(&session.id, SessionState::Idle)?;
            report.sessions_repaired += 1;
        }

        // 3. A closed run without turn.ended gets a note and an idle/blocked session.
        let closed_runs = self.closed_runs()?;
        for run in &closed_runs {
            let Some(session_id) = &run.session_id else {
                continue;
            };
            if self.session(session_id)?.is_none() {
                continue;
            }
            if self.has_event_since(session_id, EventType::TurnEnded, run.started_at)?
                || self.has_note_since(session_id, "run ended during crash recovery", run.started_at)?
            {
                continue;
            }
            self.append_reconcile_note(session_id, "run ended during crash recovery")?;
            let state = if run.limit_kind.is_some() {
                SessionState::Blocked
            } else {
                SessionState::Idle
            };
            self.set_reconcile_state(session_id, state)?;
            report.terminal_events_repaired += 1;
        }

        // 4. A finished run with no cost row gets one derived repair note.
        for run in &closed_runs {
            if self.ledger_cost_exists(run)? || self.ledger_repair_exists(run)? {
                continue;
            }
            let outcome = run.outcome.as_ref().map(crate::convert::enum_to_text).transpose()?;
            let entry = LedgerEntry {
                id: None,
                ts: self.clock.now(),
                kind: LedgerKind::Note,
                project: Some(bound.clone()),
                job_id: run.job_id.clone(),
                run_id: Some(run.id.clone()),
                session_id: run.session_id.clone(),
                role: Some(run.role),
                provider: run.provider.clone(),
                harness: run.executor.as_ref().map(|executor| executor.harness),
                model: run.executor.as_ref().map(|executor| executor.model.clone()),
                cost: None,
                summary: "cost missing after crash".to_owned(),
                detail: serde_json::json!({
                    "outcome": outcome,
                    "detail": run.detail,
                }),
            };
            self.append_ledger(&entry)?;
            report.ledgers_repaired += 1;
        }

        // 5. Leases for sessions that no longer exist are meaningless.
        let dropped = self.conn.execute(
            "DELETE FROM main.leases WHERE session_id NOT IN (SELECT id FROM main.sessions)",
            [],
        )?;
        report.leases_dropped = u32::try_from(dropped).unwrap_or(u32::MAX);

        Ok(report)
    }

    /// Finishes one open run during reconcile, tolerating a concurrent repair.
    fn finish_reconcile_run(&self, run: &Run, outcome: RunOutcome, detail: &str) -> Result<(), StoreError> {
        let finish = RunFinish {
            outcome,
            exit_code: run.exit_code,
            limit_kind: run.limit_kind,
            cost: Cost::unknown(),
            native_session_id: run.native_session_id.clone(),
            detail: detail.to_owned(),
        };
        match self.finish_run(&run.id, &finish) {
            Ok(()) | Err(StoreError::Conflict(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Returns whether an event of `event_type` exists at or after `since`.
    fn has_event_since(
        &self,
        session: &SessionId,
        event_type: EventType,
        since: Timestamp,
    ) -> Result<bool, StoreError> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM main.session_events
                 WHERE session_id = ?1 AND type = ?2 AND ts >= ?3 LIMIT 1",
                params![session.as_str(), event_type.as_str(), since.as_rfc3339()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// Returns whether a repair note with this text already exists at or after `since`.
    ///
    /// Real log events store the note summary as `note: <text>`; the in-memory fallback stores
    /// the bare text. Accept both so a second reconcile cannot append a duplicate note.
    fn has_note_since(&self, session: &SessionId, text: &str, since: Timestamp) -> Result<bool, StoreError> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM main.session_events
                 WHERE session_id = ?1 AND type = 'note' AND (summary = ?2 OR summary = ?3)
                   AND ts >= ?4 LIMIT 1",
                params![session.as_str(), text, format!("note: {text}"), since.as_rfc3339()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// Appends a system note to the session log and its index.
    fn append_reconcile_note(&self, session: &SessionId, text: &str) -> Result<(), StoreError> {
        let row = self.session(session)?;
        let Some(row) = row else {
            return Ok(());
        };
        let now = self.clock.now();
        let event = NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(row.node.clone()),
                native_session_id: None,
            },
            body: EventBody::Note(Note { text: text.to_owned() }),
            raw: None,
            ts: Some(now),
        };
        match &self.paths {
            Some(paths) if paths.log_path(session).is_file() => {
                let log = SessionLog::open(paths, session, self.clock.clone())?;
                let appended = log.append(event)?;
                self.index_event(&index_row(&appended))?;
            }
            _ => {
                let index = EventIndexRow {
                    session_id: session.clone(),
                    seq: row.last_seq.saturating_add(1),
                    id: EventId::new(),
                    ts: now,
                    event_type: EventType::Note,
                    actor: Actor::System,
                    harness: HarnessKind::Vowp,
                    model: None,
                    byte_offset: 0,
                    byte_len: 0,
                    summary: text.to_owned(),
                };
                self.index_event(&index)?;
            }
        }
        Ok(())
    }

    /// Sets a session state without detaching its executor.
    fn set_reconcile_state(&self, session: &SessionId, state: SessionState) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE main.sessions SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                crate::convert::enum_to_text(&state)?,
                self.clock.now().as_rfc3339(),
                session.as_str()
            ],
        )?;
        Ok(())
    }

    /// Lists closed runs, oldest first.
    fn closed_runs(&self) -> Result<Vec<Run>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM project.runs WHERE ended_at IS NOT NULL ORDER BY started_at, id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut runs = Vec::new();
        for row in rows {
            let id = vowp_protocol::RunId::parse(&row?)?;
            if let Some(run) = self.run(&id)? {
                runs.push(run);
            }
        }
        Ok(runs)
    }

    /// Returns whether the run already has its cost ledger row.
    fn ledger_cost_exists(&self, run: &Run) -> Result<bool, StoreError> {
        let project = self
            .conn
            .query_row(
                "SELECT 1 FROM project.ledger WHERE kind = 'cost' AND run_id = ?1 LIMIT 1",
                [run.id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if project {
            return Ok(true);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM main.node_ledger WHERE kind = 'cost' AND run_id = ?1 LIMIT 1",
                [run.id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Returns whether a prior repair note already covers this run.
    fn ledger_repair_exists(&self, run: &Run) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM project.ledger
                 WHERE kind = 'note' AND run_id = ?1 AND summary = 'cost missing after crash' LIMIT 1",
                [run.id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
