//! Proves that every schema CHECK list matches the serde wire names of its Rust enum.
//! Spec: `docs/design/crates/vowp-store.md` §4.4 rule 10 and §6.

use std::collections::BTreeSet;

use serde::Serialize;
use vowp_protocol::{
    ActiveState, Actor, CapsuleTrigger, CostSource, EdgeKind, EventType, GateKind, InboxKind, InboxState, JobKind,
    JobState, LedgerKind, LimitKind, LimitRowKind, LoopKind, MissedPolicy, NewJob, ReleaseState, Role, RunOutcome,
    RunPurpose, RunRole, ScheduleAction, ScheduleSpecKind, SessionRole, SessionState, SwitchReason, SwitchState,
    WorkerKind, WorkerState,
};

const SCHEMAS: [&str; 2] = [
    include_str!("../src/schema/global.sql"),
    include_str!("../src/schema/project.sql"),
];

/// Returns the body of one `CREATE TABLE <table> (... ) STRICT;` statement.
fn table_block(table: &str) -> &'static str {
    let marker = format!("CREATE TABLE {table} (");
    for schema in SCHEMAS {
        if let Some(start) = schema.find(&marker) {
            let rest = &schema[start..];
            let end = rest
                .find(") STRICT;")
                .unwrap_or_else(|| panic!("table {table} does not end with STRICT"));
            return &rest[..end];
        }
    }
    panic!("schemas have no table {table}")
}

/// Extracts the quoted values of the first `column ... IN ('a','b',...)` CHECK in `table`.
fn schema_values(table: &str, column: &str) -> BTreeSet<String> {
    let block = table_block(table);
    let bytes = block.as_bytes();
    let mut search = 0;
    while let Some(offset) = block[search..].find("IN") {
        let index = search + offset;
        let before_ok = index == 0 || !is_identifier_byte(bytes[index - 1]);
        let mut open = index + 2;
        while open < bytes.len() && bytes[open].is_ascii_whitespace() {
            open += 1;
        }
        let after_ok = open < bytes.len() && bytes[open] == b'(';
        if before_ok && after_ok {
            let before = &block[..index];
            let identifier = before
                .trim_end()
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            if identifier == column {
                let close = block[open..]
                    .find(')')
                    .unwrap_or_else(|| panic!("unterminated IN list for {table}.{column}"))
                    + open;
                let list = &block[open + 1..close];
                let mut values = BTreeSet::new();
                let mut expecting_value = true;
                for part in list.split('\'').skip(1) {
                    if expecting_value {
                        values.insert(part.to_owned());
                    }
                    expecting_value = !expecting_value;
                }
                return values;
            }
        }
        search = index + 2;
    }
    panic!("schema has no enum CHECK for {table}.{column}");
}

/// Returns true for a byte that can continue an SQL identifier.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Collects the serde wire names of simple enum variants.
fn names<T: Serialize>(values: &[T]) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| {
            serde_json::to_value(value)
                .expect("serialise enum")
                .as_str()
                .expect("string wire name")
                .to_owned()
        })
        .collect()
}

/// Collects the `kind` tags of internally tagged action values.
fn tags(values: &[ScheduleAction]) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| {
            serde_json::to_value(value)
                .expect("serialise action")
                .get("kind")
                .expect("kind tag")
                .as_str()
                .expect("string tag")
                .to_owned()
        })
        .collect()
}

/// Asserts exact set equality between one schema CHECK and one Rust enum.
fn check(table: &str, column: &str, expected: BTreeSet<String>) {
    let actual = schema_values(table, column);
    assert_eq!(actual, expected, "{table}.{column} drifted from its Rust enum");
}

#[test]
fn schema_matches_enums() {
    check(
        "jobs",
        "kind",
        names(&[
            JobKind::Plan,
            JobKind::Spec,
            JobKind::Build,
            JobKind::Test,
            JobKind::Verify,
            JobKind::Fix,
            JobKind::Merge,
            JobKind::Docs,
            JobKind::Research,
            JobKind::Promote,
            JobKind::Release,
            JobKind::Decision,
        ]),
    );
    check(
        "jobs",
        "role",
        names(&[Role::Planner, Role::Keeper, Role::Worker, Role::Verifier]),
    );
    check(
        "jobs",
        "state",
        names(&[
            JobState::Pending,
            JobState::Ready,
            JobState::Running,
            JobState::Stalled,
            JobState::Gating,
            JobState::Reviewing,
            JobState::Fixing,
            JobState::Merging,
            JobState::Done,
            JobState::Failed,
            JobState::Parked,
            JobState::Replanned,
            JobState::Cancelled,
        ]),
    );
    check(
        "edges",
        "kind",
        names(&[
            EdgeKind::Needs,
            EdgeKind::Verifies,
            EdgeKind::Fixes,
            EdgeKind::Escalates,
            EdgeKind::Blocks,
        ]),
    );
    check(
        "runs",
        "role",
        names(&[
            RunRole::Planner,
            RunRole::Keeper,
            RunRole::Worker,
            RunRole::Verifier,
            RunRole::Capsule,
            RunRole::Probe,
            RunRole::Merge,
            RunRole::Gate,
        ]),
    );
    check(
        "runs",
        "purpose",
        names(&[
            RunPurpose::Start,
            RunPurpose::Continue,
            RunPurpose::Fix,
            RunPurpose::Verify,
            RunPurpose::Plan,
            RunPurpose::Keep,
            RunPurpose::Capsule,
            RunPurpose::Probe,
            RunPurpose::Merge,
            RunPurpose::Gate,
        ]),
    );
    check(
        "runs",
        "outcome",
        names(&[
            RunOutcome::Ok,
            RunOutcome::Limit,
            RunOutcome::Credits,
            RunOutcome::Auth,
            RunOutcome::Context,
            RunOutcome::Rate,
            RunOutcome::Crash,
            RunOutcome::Stalled,
            RunOutcome::Killed,
            RunOutcome::Error,
        ]),
    );
    check(
        "runs",
        "limit_kind",
        names(&[
            LimitKind::Quota,
            LimitKind::Credits,
            LimitKind::Rate,
            LimitKind::Context,
            LimitKind::Auth,
        ]),
    );
    check(
        "runs",
        "cost_source",
        names(&[CostSource::Receipt, CostSource::PriceTable, CostSource::Unknown]),
    );
    check(
        "gate_results",
        "gate_kind",
        names(&[
            GateKind::Report,
            GateKind::Cmd,
            GateKind::OwnsRespected,
            GateKind::Pushed,
            GateKind::Review,
            GateKind::Script,
            GateKind::Deploy,
            GateKind::SecretScan,
        ]),
    );
    check(
        "session_events",
        "type",
        names(&[
            EventType::SessionCreated,
            EventType::SessionExecutor,
            EventType::TurnStarted,
            EventType::TurnEnded,
            EventType::UserMessage,
            EventType::AssistantMessage,
            EventType::AssistantReasoning,
            EventType::ToolCall,
            EventType::ToolResult,
            EventType::FileChange,
            EventType::Plan,
            EventType::Limit,
            EventType::Cost,
            EventType::Switch,
            EventType::Capsule,
            EventType::Error,
            EventType::Note,
        ]),
    );
    check(
        "session_events",
        "actor",
        names(&[Actor::User, Actor::Assistant, Actor::System, Actor::Tool]),
    );
    check(
        "sessions",
        "state",
        names(&[
            SessionState::Idle,
            SessionState::Running,
            SessionState::Blocked,
            SessionState::Switching,
            SessionState::Closed,
        ]),
    );
    let mut session_roles = names(&[
        SessionRole::Planner,
        SessionRole::Keeper,
        SessionRole::Worker,
        SessionRole::Verifier,
        SessionRole::User,
    ]);
    // The schema includes the additive `SessionRole::Orchestrator` variant.
    session_roles.insert("orchestrator".to_owned());
    check("sessions", "role", session_roles);
    check(
        "capsules",
        "trigger",
        names(&[
            CapsuleTrigger::Switch,
            CapsuleTrigger::Manual,
            CapsuleTrigger::TurnEnd,
            CapsuleTrigger::Limit,
            CapsuleTrigger::Context,
            CapsuleTrigger::Export,
            CapsuleTrigger::JobEnd,
        ]),
    );
    let mut switch_reasons = names(&[
        SwitchReason::User,
        SwitchReason::Limit,
        SwitchReason::Crash,
        SwitchReason::Context,
        SwitchReason::Failover,
        SwitchReason::ProbeBack,
    ]);
    // The schema includes the additive `SwitchReason::Takeover` variant.
    switch_reasons.insert("takeover".to_owned());
    check("switches", "reason", switch_reasons);
    check(
        "switches",
        "state",
        names(&[SwitchState::Started, SwitchState::Completed, SwitchState::Aborted]),
    );
    check(
        "loops",
        "kind",
        names(&[
            LoopKind::L1,
            LoopKind::L2,
            LoopKind::L3,
            LoopKind::L4,
            LoopKind::L5,
            LoopKind::L6,
            LoopKind::L7,
            LoopKind::L8,
            LoopKind::L9,
        ]),
    );
    check("loops", "state", names(&[ActiveState::Active, ActiveState::Paused]));
    check(
        "schedules",
        "spec_kind",
        names(&[ScheduleSpecKind::Cron, ScheduleSpecKind::Interval]),
    );
    check(
        "schedules",
        "action_kind",
        tags(&[
            ScheduleAction::JobTemplate {
                template: minimal_new_job(),
            },
            ScheduleAction::Loop {
                loop_name: "l4-planner".to_owned(),
            },
            ScheduleAction::Command {
                argv: vec!["true".to_owned()],
                cwd: None,
                timeout_secs: 1,
            },
        ]),
    );
    check(
        "schedules",
        "missed_policy",
        names(&[MissedPolicy::RunOnce, MissedPolicy::Skip]),
    );
    check("schedules", "state", names(&[ActiveState::Active, ActiveState::Paused]));
    check(
        "ledger",
        "kind",
        names(&[
            LedgerKind::Dispatch,
            LedgerKind::Gate,
            LedgerKind::Merge,
            LedgerKind::Cost,
            LedgerKind::Switch,
            LedgerKind::Limit,
            LedgerKind::Promote,
            LedgerKind::Rollback,
            LedgerKind::Inbox,
            LedgerKind::Schedule,
            LedgerKind::State,
            LedgerKind::Note,
        ]),
    );
    check(
        "inbox",
        "kind",
        names(&[
            InboxKind::Question,
            InboxKind::Approval,
            InboxKind::Decision,
            InboxKind::Notice,
        ]),
    );
    check(
        "inbox",
        "state",
        names(&[InboxState::Open, InboxState::Answered, InboxState::Dismissed]),
    );
    check(
        "node_inbox",
        "kind",
        names(&[
            InboxKind::Question,
            InboxKind::Approval,
            InboxKind::Decision,
            InboxKind::Notice,
        ]),
    );
    check(
        "node_inbox",
        "state",
        names(&[InboxState::Open, InboxState::Answered, InboxState::Dismissed]),
    );
    check(
        "node_ledger",
        "kind",
        names(&[
            LedgerKind::Dispatch,
            LedgerKind::Gate,
            LedgerKind::Merge,
            LedgerKind::Cost,
            LedgerKind::Switch,
            LedgerKind::Limit,
            LedgerKind::Promote,
            LedgerKind::Rollback,
            LedgerKind::Inbox,
            LedgerKind::Schedule,
            LedgerKind::State,
            LedgerKind::Note,
        ]),
    );
    check(
        "node_ledger",
        "role",
        names(&[
            RunRole::Planner,
            RunRole::Keeper,
            RunRole::Worker,
            RunRole::Verifier,
            RunRole::Capsule,
            RunRole::Probe,
            RunRole::Merge,
            RunRole::Gate,
        ]),
    );
    check(
        "node_ledger",
        "cost_source",
        names(&[CostSource::Receipt, CostSource::PriceTable, CostSource::Unknown]),
    );
    check(
        "limits",
        "kind",
        names(&[
            LimitRowKind::Quota,
            LimitRowKind::Credits,
            LimitRowKind::Rate,
            LimitRowKind::Context,
            LimitRowKind::Auth,
            LimitRowKind::Unavailable,
        ]),
    );
    check(
        "releases",
        "state",
        names(&[
            ReleaseState::Planned,
            ReleaseState::Guarded,
            ReleaseState::Banked,
            ReleaseState::Approved,
            ReleaseState::Deploying,
            ReleaseState::Probing,
            ReleaseState::Live,
            ReleaseState::Failed,
            ReleaseState::RolledBack,
            ReleaseState::Refused,
        ]),
    );
    check("workers", "kind", names(&[WorkerKind::Worker, WorkerKind::Verifier]));
    check(
        "workers",
        "state",
        names(&[WorkerState::Idle, WorkerState::Busy, WorkerState::Draining]),
    );
}

/// Minimal template used only to read the `job_template` tag.
fn minimal_new_job() -> NewJob {
    NewJob {
        id: vowp_protocol::JobId::parse("tag-job").expect("id"),
        title: "tag job".to_owned(),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    }
}
