//! Integration test for rollback of a live promotion.
//! Spec: `docs/design/crates/vowp-core.md` §8 and `docs/design/concepts/work-to-production.md`.

#[path = "promote_support.rs"]
mod support;

use tempfile::TempDir;
use vowp_core::promote::{PromoteOpts, Promoter};
use vowp_protocol::{Approval, ReleaseState};

use support::*;

#[test]
fn rollback_runs_banked_command_and_links_releases() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let repo = root.join("repo");
    init_repo(&repo);
    let base = head(&repo);
    write(&repo.join("change.txt"), "new\n");
    let candidate = commit_all(&repo, "candidate");
    let checkout = root.join("checkout");
    clone_at(&repo, &checkout, &base);

    let log = root.join("order.log");
    let bank = root.join("bank.sh");
    script(
        &bank,
        &format!("echo bank >> {}\necho snapshot > \"$VOWP_BANK_PATH\"", log.display()),
    );
    let deploy = root.join("deploy.sh");
    script(&deploy, &format!("echo deploy >> {}", log.display()));
    let probe = root.join("probe.sh");
    script(&probe, &format!("echo probe >> {}", log.display()));
    let rollback = root.join("rollback.sh");
    script(&rollback, &format!("echo \"rollback $1\" >> {}", log.display()));

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", bank.display()));
    env.deploy = vec![deploy.display().to_string()];
    env.probe = Some(probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let live = promoter
        .promote(
            &store,
            &project,
            "prod",
            &candidate,
            PromoteOpts {
                yes: true,
                ..PromoteOpts::default()
            },
        )
        .unwrap();
    assert_eq!(live.state, ReleaseState::Live);
    let snapshot = live.snapshot_path.clone().expect("snapshot path");

    let rolled_back = promoter.rollback(&store, &live.id, PromoteOpts::default()).unwrap();

    assert_eq!(rolled_back.state, ReleaseState::RolledBack);
    assert_eq!(rolled_back.from_commit.as_deref(), Some(candidate.as_str()));
    assert_eq!(rolled_back.to_commit.as_deref(), Some(base.as_str()));
    assert!(rolled_back.note.contains(&live.id.to_string()));

    let original = store.release(&live.id).unwrap().unwrap();
    assert_eq!(original.state, ReleaseState::RolledBack);
    assert_eq!(original.rolled_back_by.as_ref(), Some(&rolled_back.id));

    let recorded = read(&log);
    assert!(
        recorded.contains(&format!("rollback {snapshot}")),
        "rollback command received the banked path: {recorded}"
    );
    let release_log = read(&node.join("releases.log"));
    assert!(release_log.contains("rolled_back"));
    assert!(release_log.contains(&rolled_back.id.to_string()));

    let listed = promoter.releases(&store, &project, Some("prod"), 10).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, rolled_back.id);
}
