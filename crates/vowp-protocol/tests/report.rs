//! Report and brief front-matter tests. Spec: `docs/design/types.md` §8.1–8.2 and
//! `docs/design/crates/vowp-protocol.md` §6.

use pretty_assertions::assert_eq;

use vowp_protocol::gate::{CmdGate, Gate, ReportGate, ReviewGate};
use vowp_protocol::job::{Job, JobLoop};
use vowp_protocol::report::{Brief, Report, ReportStatus, TestsField};
use vowp_protocol::{JobId, JobKind, JobState, ProjectName, ProtocolError, Role, Timestamp};

#[test]
fn report_parse_good() {
    let markdown = include_str!("fixtures/report_ok.md");
    let report = Report::parse(markdown).expect("fixture parses");

    assert_eq!(report.front_matter.job.as_str(), "sample-job");
    assert_eq!(report.front_matter.status, ReportStatus::Done);
    assert_eq!(report.front_matter.branch, "job/sample-job");
    assert_eq!(report.front_matter.commits, vec!["abc1234", "def5678"]);
    assert_eq!(report.front_matter.files_changed, vec!["crates/vowp-graph/src/lib.rs"]);
    match &report.front_matter.tests {
        Some(TestsField::Structured {
            command,
            passed,
            failed,
        }) => {
            assert_eq!(command, "cargo test -p vowp-graph");
            assert_eq!(*passed, Some(31));
            assert_eq!(*failed, Some(0));
        }
        other => panic!("expected structured tests, got {other:?}"),
    }
    assert_eq!(
        report.body,
        "\n# sample-job — report\n\n`vowp-graph` is implemented and green.\n\n## What was built\n\nThe DAG and the state machine.\n"
    );
    assert_eq!(report.raw_front_matter["job"], "sample-job");

    let rendered = report.render();
    let reparsed = Report::parse(&rendered).expect("rendered report parses");
    assert_eq!(reparsed.front_matter, report.front_matter);
    assert_eq!(reparsed.body, report.body);

    let text = Report::parse(include_str!("fixtures/report_ok_text.md")).expect("free-string tests parse");
    assert_eq!(text.front_matter.status, ReportStatus::Partial);
    match &text.front_matter.tests {
        Some(TestsField::Text(value)) => {
            assert_eq!(value, "cargo test -p vowp-graph — 31 passed");
        }
        other => panic!("expected free-string tests, got {other:?}"),
    }
    assert!(text.body.contains("Body of the free-string report."));
}

#[test]
fn report_parse_bad_cases() {
    let cases = [
        ("no fence", "# report\n\nNo front matter here.\n"),
        (
            "unterminated fence",
            "---\njob: sample-job\nstatus: done\nbranch: job/sample-job\n",
        ),
        ("invalid YAML", "---\njob: [\nstatus: done\n---\nbody\n"),
        ("missing branch", "---\njob: sample-job\nstatus: done\n---\nbody\n"),
        (
            "invalid status",
            "---\njob: sample-job\nstatus: finished\nbranch: job/sample-job\n---\nbody\n",
        ),
        ("empty document", ""),
    ];
    for (label, markdown) in cases {
        match Report::parse(markdown) {
            Err(ProtocolError::FrontMatter(_)) => {}
            other => panic!("{label}: expected FrontMatter error, got {other:?}"),
        }
    }
}

#[test]
fn report_missing_fields() {
    let report = Report::parse(include_str!("fixtures/report_missing_tests.md")).expect("fixture parses");
    let required = ["status", "branch", "commits", "tests"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(report.missing_fields(&required), vec!["tests".to_owned()]);

    // An empty list counts as present; only an absent/null/empty-string field is missing.
    let with_tests = include_str!("fixtures/report_missing_tests.md").replace(
        "questions_for_owner: []",
        "questions_for_owner: []\ntests: \"cargo test — ok\"",
    );
    let report = Report::parse(&with_tests).expect("fixture with tests parses");
    assert_eq!(report.missing_fields(&required), Vec::<String>::new());

    let empty_branch =
        Report::parse("---\njob: sample-job\nstatus: done\nbranch: \"\"\ncommits: []\ntests: ok\n---\nbody\n")
            .expect("empty branch still decodes");
    assert_eq!(empty_branch.missing_fields(&required), vec!["branch".to_owned()]);

    let null_tests = Report::parse(
        "---\njob: sample-job\nstatus: done\nbranch: job/sample-job\ncommits: []\ntests: null\n---\nbody\n",
    )
    .expect("null optional tests still decode");
    assert_eq!(null_tests.missing_fields(&required), vec!["tests".to_owned()]);
}

#[test]
fn brief_roundtrip_and_from_job() {
    let markdown = include_str!("fixtures/brief_example.md");
    let brief = Brief::parse(markdown).expect("fixture parses");
    assert_eq!(brief.front_matter.job.as_str(), "sample-job");
    assert_eq!(brief.front_matter.owns, vec!["crates/vowp-graph/**"]);
    assert_eq!(brief.front_matter.must_not_touch, vec!["everything else"]);
    assert_eq!(
        brief.front_matter.needs_done,
        vec![JobId::parse("sample-dependency").unwrap()]
    );
    assert_eq!(
        brief.front_matter.gates,
        vec!["report", "cargo test -p vowp-graph", "owns_respected", "review"]
    );
    assert_eq!(
        brief.front_matter.done_when,
        "report has status: done and every gate passes"
    );
    assert_eq!(brief.body, "# sample-job\n\nImplement the DAG and the state machine.\n");

    let rendered = brief.render();
    let reparsed = Brief::parse(&rendered).expect("rendered brief parses");
    assert_eq!(reparsed, brief);

    let job = sample_job();
    let generated = Brief::from_job(&job, &brief.body);
    assert_eq!(generated, brief);
    assert_eq!(generated.front_matter.gates, brief.front_matter.gates);
    assert_eq!(generated.front_matter.must_not_touch, vec!["everything else"]);
}

fn sample_job() -> Job {
    let ts = Timestamp::parse("2026-03-10T19:50:00.000Z").unwrap();
    Job {
        id: JobId::parse("sample-job").unwrap(),
        project: ProjectName::parse("vowp").unwrap(),
        title: "vowp-graph".to_owned(),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state: JobState::Running,
        priority: 50,
        needs: vec![JobId::parse("sample-dependency").unwrap()],
        owns: vec!["crates/vowp-graph/**".to_owned()],
        reads: vec!["docs/design/**".to_owned()],
        brief: ".vowp/jobs/sample-job/brief.md".to_owned(),
        report: ".vowp/jobs/sample-job/report.md".to_owned(),
        gates: vec![
            Gate::Report(ReportGate::default()),
            Gate::Cmd(CmdGate {
                run: "cargo test -p vowp-graph".to_owned(),
                ..CmdGate::default()
            }),
            Gate::OwnsRespected(true),
            Gate::Review(ReviewGate::default()),
        ],
        loop_: JobLoop::default(),
        branch: Some("job/sample-job".to_owned()),
        worktree: Some("/home/user/project".to_owned()),
        attempts: 0,
        continues: 0,
        hang_restarts: 0,
        parked_reason: None,
        note: String::new(),
        created_by: "planner".to_owned(),
        last_progress_at: None,
        state_changed_at: ts,
        created_at: ts,
        updated_at: ts,
    }
}
