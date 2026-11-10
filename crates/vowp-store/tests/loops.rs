//! Loop and schedule repository tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vowp_protocol::{
    ActiveState, Clock, FakeClock, Loop, LoopBounds, LoopKind, MissedPolicy, Schedule, ScheduleAction, ScheduleId,
    ScheduleSpecKind, Timestamp,
};
use vowp_store::{NodePaths, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A file-backed store with a controllable clock.
struct Fixture {
    _tmp: TempDir,
    store: Store,
    clock: FakeClock,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        Self {
            _tmp: tmp,
            store,
            clock,
        }
    }
}

/// Builds one loop row.
fn sample_loop(now: Timestamp) -> Loop {
    Loop {
        name: "l4-planner".to_owned(),
        kind: LoopKind::L4,
        project: None,
        interval_secs: 1800,
        state: ActiveState::Active,
        bounds: LoopBounds {
            same_action_max: Some(3),
            ..LoopBounds::default()
        },
        last_run_at: None,
        next_run_at: None,
        last_status: String::new(),
        fail_streak: 0,
        updated_at: now,
    }
}

/// Builds one schedule row.
fn sample_schedule(name: &str, state: ActiveState, next_run_at: Option<Timestamp>, created_at: Timestamp) -> Schedule {
    Schedule {
        id: ScheduleId::new(),
        project: None,
        name: name.to_owned(),
        spec_kind: ScheduleSpecKind::Interval,
        spec: "every 30m".to_owned(),
        tz: "UTC".to_owned(),
        action: ScheduleAction::Loop {
            loop_name: "l4-planner".to_owned(),
        },
        missed_policy: MissedPolicy::RunOnce,
        state,
        last_run_at: None,
        next_run_at,
        last_status: String::new(),
        created_at,
    }
}

#[test]
fn due_schedules_selects_active_due_only() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let earlier = now.minus(Duration::from_secs(70));
    let later = now.minus(Duration::from_secs(10));
    let future = now.plus(Duration::from_secs(60));

    let paused = sample_schedule("paused", ActiveState::Paused, Some(earlier), now);
    let future = sample_schedule("future", ActiveState::Active, Some(future), now);
    let due_later = sample_schedule("due-later", ActiveState::Active, Some(later), now);
    let due_earlier = sample_schedule("due-earlier", ActiveState::Active, Some(earlier), now);

    for schedule in [&paused, &future, &due_later, &due_earlier] {
        fixture.store.insert_schedule(schedule).expect("insert schedule");
    }
    assert_eq!(
        fixture.store.schedule(due_later.id.as_str()).expect("by id"),
        Some(due_later.clone())
    );
    assert_eq!(
        fixture.store.schedule("due-later").expect("by name"),
        Some(due_later.clone())
    );
    assert_eq!(fixture.store.schedule("missing").expect("missing schedule"), None);

    let due = fixture.store.due_schedules(now).expect("due schedules");
    assert_eq!(
        due.iter().map(|schedule| schedule.name.as_str()).collect::<Vec<_>>(),
        vec!["due-earlier", "due-later"],
        "only active, due schedules in next_run order"
    );

    let duplicate = sample_schedule("due-later", ActiveState::Active, Some(now), now);
    let error = fixture
        .store
        .insert_schedule(&duplicate)
        .expect_err("duplicate schedule name");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");

    let next = now.plus(Duration::from_secs(1800));
    fixture
        .store
        .record_schedule_run(&due_earlier.id, now, Some(next), "ok")
        .expect("record run");
    let recorded = fixture.store.schedule("due-earlier").expect("schedule").expect("row");
    assert_eq!(recorded.last_run_at, Some(now));
    assert_eq!(recorded.next_run_at, Some(next));
    assert_eq!(recorded.last_status, "ok");

    fixture
        .store
        .set_schedule_state(&due_earlier.id, ActiveState::Paused)
        .expect("pause schedule");
    let due = fixture.store.due_schedules(now).expect("due schedules");
    assert_eq!(due, vec![due_later.clone()]);

    assert!(fixture.store.remove_schedule("due-later").expect("remove by name"));
    assert!(!fixture.store.remove_schedule("due-later").expect("already gone"));
    assert_eq!(fixture.store.schedule("due-later").expect("lookup"), None);
    assert_eq!(fixture.store.list_schedules().expect("list").len(), 3);

    let missing = ScheduleId::new();
    let error = fixture
        .store
        .record_schedule_run(&missing, now, None, "ok")
        .expect_err("missing schedule run");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
    let error = fixture
        .store
        .set_schedule_state(&missing, ActiveState::Paused)
        .expect_err("missing schedule state");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn loop_fail_streak() {
    let fixture = Fixture::new();
    let now = fixture.clock.now();
    let loop_ = sample_loop(now);
    fixture.store.upsert_loop(&loop_).expect("upsert loop");
    assert_eq!(fixture.store.loop_("l4-planner").expect("loop"), Some(loop_.clone()));
    assert_eq!(fixture.store.list_loops().expect("loops"), vec![loop_.clone()]);
    assert_eq!(fixture.store.loop_("missing").expect("missing loop"), None);

    let next = now.plus(Duration::from_secs(1800));
    fixture
        .store
        .record_loop_run("l4-planner", now, Some(next), "failed: boom", false)
        .expect("first failure");
    assert_eq!(
        fixture
            .store
            .loop_("l4-planner")
            .expect("loop")
            .expect("row")
            .fail_streak,
        1
    );
    fixture
        .store
        .record_loop_run("l4-planner", now, Some(next), "failed: boom", false)
        .expect("second failure");
    let failed = fixture.store.loop_("l4-planner").expect("loop").expect("row");
    assert_eq!(failed.fail_streak, 2);
    assert_eq!(failed.last_status, "failed: boom");
    assert_eq!(failed.last_run_at, Some(now));
    assert_eq!(failed.next_run_at, Some(next));

    fixture
        .store
        .record_loop_run("l4-planner", now, Some(next), "ok", true)
        .expect("success resets the streak");
    let healthy = fixture.store.loop_("l4-planner").expect("loop").expect("row");
    assert_eq!(healthy.fail_streak, 0);
    assert_eq!(healthy.last_status, "ok");

    fixture
        .store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause loop");
    assert_eq!(
        fixture.store.loop_("l4-planner").expect("loop").expect("row").state,
        ActiveState::Paused
    );

    let error = fixture
        .store
        .set_loop_state("missing", ActiveState::Paused)
        .expect_err("missing loop state");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
    let error = fixture
        .store
        .record_loop_run("missing", now, None, "ok", true)
        .expect_err("missing loop run");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}
