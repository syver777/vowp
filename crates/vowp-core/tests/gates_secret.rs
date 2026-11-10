//! Secret-scan tests: canonical rule names, file paths and added-only diff scanning.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

use vowp_core::gates::secret_scan::{rules, scan_diff, scan_paths, scan_text};

/// Builds one sample value for each content rule without writing a matchable literal.
fn samples() -> Vec<(&'static str, String)> {
    vec![
        ("sk_api_key", format!("sk-{}", "A".repeat(24))),
        ("supabase_secret", format!("sb_secret_{}", "B".repeat(12))),
        ("github_token", format!("ghp_{}", "C".repeat(24))),
        ("github_pat", format!("github_pat_{}", "D".repeat(24))),
        ("aws_access_key", format!("AKIA{}", "E".repeat(16))),
        (
            "private_key_block",
            format!("{}RSA {}PRIVATE KEY-----", "-----BEGIN ", ""),
        ),
        ("slack_token", format!("xoxb-{}", "F".repeat(12))),
    ]
}

#[test]
fn secret_scan_detects_each_rule_by_exact_name_and_never_stores_value() {
    let expected_names = [
        "sk_api_key",
        "supabase_secret",
        "github_token",
        "github_pat",
        "aws_access_key",
        "private_key_block",
        "slack_token",
    ];
    let names: Vec<&str> = rules().iter().map(|rule| rule.name).collect();
    assert_eq!(names, expected_names, "the rule table is canonical and ordered");

    let samples = samples();
    let text = samples
        .iter()
        .map(|(_, value)| format!("line with {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let hits = scan_text("src/config.rs", &text);

    let hit_names: Vec<&str> = hits.iter().map(|hit| hit.rule.as_str()).collect();
    assert_eq!(hit_names, expected_names, "each rule fires exactly once, in order");
    let lines: Vec<u32> = hits.iter().map(|hit| hit.line).collect();
    assert_eq!(lines, vec![1, 2, 3, 4, 5, 6, 7]);
    assert!(hits.iter().all(|hit| hit.path == "src/config.rs"));

    let serialised = serde_json::to_string(&hits).unwrap();
    for (rule, value) in &samples {
        assert!(
            !serialised.contains(value),
            "hit for {rule} leaked the matched value: {serialised}"
        );
    }
}

#[test]
fn secret_scan_flags_secret_file_paths() {
    let paths = vec![
        ".env".to_owned(),
        "config/.env.local".to_owned(),
        "certs/server.pem".to_owned(),
        "keys/id_rsa".to_owned(),
        "keys/id_rsa.pub".to_owned(),
        "keys/id_ed25519".to_owned(),
        "id_ed25519.pub".to_owned(),
        "src/main.rs".to_owned(),
        "docs/environment.md".to_owned(),
    ];

    let hits = scan_paths(&paths);

    let flagged: Vec<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
    assert_eq!(
        flagged,
        vec![
            ".env",
            "config/.env.local",
            "certs/server.pem",
            "keys/id_rsa",
            "keys/id_rsa.pub",
            "keys/id_ed25519",
            "id_ed25519.pub",
        ]
    );
    assert!(hits.iter().all(|hit| hit.line == 0));
    assert!(hits.iter().all(|hit| hit.rule == "secret_file"));
}

#[test]
fn secret_scan_only_added_lines() {
    let secret = format!("sk-{}", "A".repeat(24));
    let diff = format!(
        "diff --git a/src/app.rs b/src/app.rs\n\
         index 1111111..2222222 100644\n\
         --- a/src/app.rs\n\
         +++ b/src/app.rs\n\
         @@ -1,2 +1,3 @@\n\
         \x20let keep = 1;\n\
         -let old = \"{secret}\";\n\
         +let leaked = \"{secret}\";\n\
         +let safe = \"nothing\";\n"
    );

    let hits = scan_diff(&diff);

    assert_eq!(hits.len(), 1, "removed and context lines must not hit: {hits:?}");
    assert_eq!(hits[0].path, "src/app.rs");
    assert_eq!(hits[0].line, 2, "the added line is new-file line 2");
    assert_eq!(hits[0].rule, "sk_api_key");
    assert!(!serde_json::to_string(&hits).unwrap().contains(&secret));

    let removed_only = format!(
        "--- a/src/app.rs\n+++ b/src/app.rs\n@@ -1 +1 @@\n-let old = \"{secret}\";\n+let safe = \"nothing\";\n"
    );
    assert!(scan_diff(&removed_only).is_empty(), "a removed secret is not a leak");
}

#[test]
fn secret_scan_does_not_skip_added_line_starting_with_plus_plus() {
    let secret = format!("sk-{}", "A".repeat(24));
    // The added file line is "++ <secret>", so its diff line is "+++ <secret>".
    let diff = format!(
        "diff --git a/src/app.rs b/src/app.rs\n\
         index 1111111..2222222 100644\n\
         --- a/src/app.rs\n\
         +++ b/src/app.rs\n\
         @@ -1,1 +1,2 @@\n\
         \x20let keep = 1;\n\
         +++ {secret}\n"
    );

    let hits = scan_diff(&diff);

    assert_eq!(
        hits.len(),
        1,
        "the added line must be scanned, not treated as a header: {hits:?}"
    );
    assert_eq!(hits[0].path, "src/app.rs");
    assert_eq!(hits[0].line, 2, "the added line is new-file line 2");
    assert_eq!(hits[0].rule, "sk_api_key");
    assert!(!serde_json::to_string(&hits).unwrap().contains(&secret));
}
