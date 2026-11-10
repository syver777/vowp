//! Ownership overlap, path permission, literal prefixes and lock groups.
//! Spec: `docs/design/crates/vowp-graph.md` §4 and §10.

use proptest::prelude::*;
use vowp_graph::{Graph, OwnsLocks, literal_prefix, owns_overlap, path_allowed};
use vowp_protocol::{Edge, EdgeKind, Job, JobId, JobKind, JobLoop, JobState, ProjectName, Role, Timestamp};

/// Builds a fixed timestamp for deterministic rows.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Builds a job row; the ownership and kind are all these tests vary.
fn job(id: &str, state: JobState, owns: &[&str], kind: JobKind) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    Job {
        id: id.clone(),
        project,
        title: format!("job {id}"),
        kind,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state,
        priority: 50,
        needs: Vec::new(),
        owns: owns.iter().map(|pattern| (*pattern).to_owned()).collect(),
        reads: Vec::new(),
        brief: format!(".vowp/jobs/{id}/brief.md"),
        report: format!(".vowp/jobs/{id}/report.md"),
        gates: Vec::new(),
        loop_: JobLoop::default(),
        branch: Some(format!("job/{id}")),
        worktree: Some(format!("/tmp/wt/{id}")),
        attempts: 0,
        continues: 0,
        hang_restarts: 0,
        parked_reason: None,
        note: String::new(),
        created_by: "test".to_owned(),
        last_progress_at: None,
        state_changed_at: at(1_700_000_000),
        created_at: at(1_700_000_000),
        updated_at: at(1_700_000_000),
    }
}

/// Builds an edge from ids.
fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
    Edge {
        src: JobId::parse(src).expect("test edge src is valid"),
        dst: JobId::parse(dst).expect("test edge dst is valid"),
        kind,
    }
}

/// Builds a pattern list from literals.
fn patterns(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Builds the expected component vector.
fn components(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn overlap_table() {
    let cases: &[(&[&str], &[&str], bool)] = &[
        (&["crates/vowp-graph/**"], &["crates/vowp-store/**"], false),
        (&["crates/**"], &["crates/vowp-graph/src/lib.rs"], true),
        (&["docs/*.md"], &["docs/design/**"], true),
        (&["**/*.rs"], &["anything/at/all"], true),
        (&["Cargo.toml"], &["Cargo.lock"], false),
        (&["crates/a"], &["crates/a/src/lib.rs"], true),
        (&["crates/a"], &["crates/b"], false),
        (&["a/b.rs"], &["a/c.rs"], false),
        (&["a/b.rs"], &["a/b.rs"], true),
        (&["a/*"], &["a/b"], true),
        (&["a/**"], &["a"], true),
        (&["a/"], &["a/b/c"], true),
        (&["./a"], &["a/b"], true),
        (&[r"a\b"], &["a/b/c"], true),
        (&["a//b"], &["a/b/c"], true),
        (&["*.rs"], &["Cargo.toml"], true),
        (&["a?c"], &["abc"], true),
        (&["a/[bc]"], &["a/b"], true),
        (&["a/{b,c}"], &["a/d"], true),
        (&["a/b"], &["a/b/c/d"], true),
        (&["a/b/c"], &["a/b"], true),
        (&["a/b.rs"], &["a/b.rs/child"], true),
        (&[""], &["totally/unrelated"], true),
        (&["   "], &["totally/unrelated"], true),
    ];

    for (left, right, expected) in cases {
        let left = patterns(left);
        let right = patterns(right);
        assert_eq!(
            owns_overlap(&left, &right),
            *expected,
            "owns_overlap({left:?}, {right:?})"
        );
        assert_eq!(
            owns_overlap(&right, &left),
            *expected,
            "owns_overlap is symmetric for {left:?}, {right:?}"
        );
    }
}

#[test]
fn overlap_empty_owns_never_overlaps() {
    assert!(!owns_overlap(&[], &[]));
    assert!(!owns_overlap(&[], &patterns(&["**"])));
    assert!(!owns_overlap(&patterns(&["crates/**"]), &[]));
    assert!(!owns_overlap(&[], &patterns(&["../global"])));
}

#[test]
fn overlap_dotdot_and_absolute_are_global() {
    let global = patterns(&["../x", "a/../b", "/etc/x", "/", ""]);
    let other = patterns(&["docs/a.md"]);

    for pattern in &global {
        assert!(
            owns_overlap(&patterns(&[pattern]), &other),
            "{pattern:?} must overlap everything"
        );
        assert!(owns_overlap(&patterns(&[pattern]), &patterns(&[pattern])));
        assert!(literal_prefix(pattern).is_empty(), "{pattern:?} has no literal prefix");
    }
    assert!(owns_overlap(&global, &other));
}

#[test]
fn literal_prefix_examples() {
    assert_eq!(literal_prefix("crates/x/**"), components(&["crates", "x"]));
    assert_eq!(literal_prefix("Cargo.toml"), components(&["Cargo.toml"]));
    assert_eq!(literal_prefix("**/a"), components(&[]));
    assert_eq!(literal_prefix("docs/*.md"), components(&["docs"]));
    assert_eq!(literal_prefix("a/b/c"), components(&["a", "b", "c"]));
    assert_eq!(literal_prefix("a/*/c"), components(&["a"]));
    assert_eq!(literal_prefix("a?c"), components(&[]));
    assert_eq!(literal_prefix("a/[bc]/d"), components(&["a"]));
    assert_eq!(literal_prefix("a/{b,c}/d"), components(&["a"]));
    assert_eq!(literal_prefix("../a"), components(&[]));
    assert_eq!(literal_prefix("/a/b"), components(&[]));
    assert_eq!(literal_prefix(""), components(&[]));
    assert_eq!(literal_prefix("  ./a/b/  "), components(&["a", "b"]));
}

#[test]
fn path_allowed_matches_glob_semantics() {
    // `*` stays inside one component, `**` crosses separators.
    assert!(path_allowed(&patterns(&["docs/*.md"]), &[], "docs/a.md"));
    assert!(!path_allowed(&patterns(&["docs/*.md"]), &[], "docs/x/a.md"));
    assert!(path_allowed(&patterns(&["docs/**"]), &[], "docs/x/a.md"));
    assert!(path_allowed(&patterns(&["docs/**"]), &[], "docs/a.md"));
    assert!(path_allowed(&patterns(&["*.rs"]), &[], "a.rs"));
    assert!(!path_allowed(&patterns(&["*.rs"]), &[], "src/a.rs"));

    // A bare directory name allows the directory and everything below it.
    assert!(path_allowed(&patterns(&["docs"]), &[], "docs"));
    assert!(path_allowed(&patterns(&["docs"]), &[], "docs/a/b.md"));
    assert!(path_allowed(&patterns(&["docs/"]), &[], "docs/a/b.md"));
    assert!(path_allowed(&patterns(&["crates/*/"]), &[], "crates/x/src/lib.rs"));

    // `always_allowed` is a union with `owns`, including with an empty owns list.
    assert!(path_allowed(
        &patterns(&["src/**"]),
        &patterns(&["Cargo.toml"]),
        "Cargo.toml"
    ));
    assert!(path_allowed(&patterns(&[]), &patterns(&["Cargo.toml"]), "Cargo.toml"));
    assert!(!path_allowed(&patterns(&[]), &[], "src/a.rs"));

    // Absolute and parent-relative paths are refused even when a glob would match.
    assert!(!path_allowed(&patterns(&["src/**"]), &[], "../src/a.rs"));
    assert!(!path_allowed(&patterns(&["etc/**"]), &[], "/etc/x"));
    assert!(!path_allowed(&patterns(&["**"]), &[], "/etc/x"));

    // Normalisation: leading `./`, doubled and backslash separators.
    assert!(path_allowed(&patterns(&["src/**"]), &[], "./src/a.rs"));
    assert!(path_allowed(&patterns(&["src/**"]), &[], "src//a.rs"));
    assert!(path_allowed(&patterns(&["src/**"]), &[], r"src\a.rs"));

    // An invalid glob is treated as no-match rather than a panic.
    assert!(!path_allowed(&patterns(&["["]), &[], "a"));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn prop_overlap_is_conservative(
        components in prop::collection::vec(
            prop_oneof![Just("a"), Just("b"), Just("c"), Just("src"), Just("lib.rs")],
            1..5,
        ),
        prefix_len in 0usize..=4,
        exact in any::<bool>(),
    ) {
        let path = components.join("/");
        let prefix_len = prefix_len.min(components.len());
        let pattern = if exact || prefix_len == components.len() {
            path.clone()
        } else if prefix_len == 0 {
            "**".to_owned()
        } else {
            format!("{}/**", components[..prefix_len].join("/"))
        };

        prop_assert!(
            path_allowed(std::slice::from_ref(&pattern), &[], &path),
            "pattern {pattern:?} should allow {path:?}"
        );
        prop_assert!(
            owns_overlap(std::slice::from_ref(&pattern), std::slice::from_ref(&path)),
            "allowed pattern {pattern:?} must overlap path {path:?}"
        );
        prop_assert!(owns_overlap(std::slice::from_ref(&path), std::slice::from_ref(&pattern)));
    }
}

#[test]
fn locks_fix_job_shares_parent_group() {
    let parent = job("orig", JobState::Fixing, &["crates/x/**"], JobKind::Build);
    let fix = job("orig-fix", JobState::Running, &["crates/x/**"], JobKind::Fix);
    let graph = Graph::new(
        vec![parent.clone(), fix.clone()],
        vec![edge("orig-fix", "orig", EdgeKind::Fixes)],
    )
    .expect("fix graph is valid");

    let locks = OwnsLocks::from_running(&[parent.clone(), fix.clone()], &graph);
    assert_eq!(
        locks.holders(),
        vec![parent.id.clone(), fix.id.clone()],
        "both parent and fix hold their shared group's lock"
    );

    // A different owner of the same paths conflicts with both members of the group.
    let other = job("other", JobState::Ready, &["crates/x/src/lib.rs"], JobKind::Build);
    let mut locks = locks;
    assert_eq!(
        locks.try_acquire(&other, &graph),
        Err(vec![parent.id.clone(), fix.id.clone()])
    );

    // The fix job may acquire the lock its parent already holds.
    let mut parent_only = OwnsLocks::from_running(std::slice::from_ref(&parent), &graph);
    assert_eq!(parent_only.try_acquire(&fix, &graph), Ok(()));
    assert_eq!(parent_only.holders(), vec![parent.id, fix.id]);
}

#[test]
fn locks_release_frees_conflict() {
    let first = job("first", JobState::Running, &["crates/x/**"], JobKind::Build);
    let second = job("second", JobState::Ready, &["crates/x/src/lib.rs"], JobKind::Build);
    let graph = Graph::new(vec![first.clone(), second.clone()], Vec::new()).expect("graph is valid");

    let mut locks = OwnsLocks::from_running(std::slice::from_ref(&first), &graph);
    assert_eq!(locks.conflicts(&second, &graph), vec![first.id.clone()]);
    assert_eq!(locks.try_acquire(&second, &graph), Err(vec![first.id.clone()]));

    locks.release(&first.id);
    assert!(locks.holders().is_empty());
    assert_eq!(locks.try_acquire(&second, &graph), Ok(()));
    assert_eq!(locks.holders(), vec![second.id.clone()]);

    // Releasing an unknown id is a no-op.
    locks.release(&JobId::parse("ghost").expect("test id is valid"));
    assert_eq!(locks.holders(), vec![second.id]);
}
