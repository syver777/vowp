//! Secret scanning: the canonical seven content rules, secret-file paths and added diff lines.
//! Spec: `docs/design/loops.md` §4.
//!
//! Hits carry only the rule name, path and line. Matched text never leaves this module.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use regex::Regex;
use vowp_protocol::{Evidence, SecretHit};

use super::report::elapsed_ms;
use super::{GateCtx, Outcome};

/// One content rule: a stable name and its regex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Name stored in [`SecretHit::rule`].
    pub name: &'static str,
    /// Regex applied to each line.
    pub regex: &'static str,
}

/// The seven content rules, in the order fixed by `loops.md` §4.
const RULES: [Rule; 7] = [
    Rule {
        name: "sk_api_key",
        regex: r"sk-[A-Za-z0-9_-]{20,}",
    },
    Rule {
        name: "supabase_secret",
        regex: r"sb_secret_[A-Za-z0-9_-]{10,}",
    },
    Rule {
        name: "github_token",
        regex: r"ghp_[A-Za-z0-9]{20,}",
    },
    Rule {
        name: "github_pat",
        regex: r"github_pat_[A-Za-z0-9_]{20,}",
    },
    Rule {
        name: "aws_access_key",
        regex: r"AKIA[0-9A-Z]{16}",
    },
    Rule {
        name: "private_key_block",
        regex: r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    },
    Rule {
        name: "slack_token",
        regex: r"xox[abp]-[A-Za-z0-9-]{10,}",
    },
];

/// Returns the seven content rules in canonical order.
pub fn rules() -> &'static [Rule] {
    &RULES
}

/// Evaluates the `secret_scan` gate over the job's branch diff.
pub(crate) fn evaluate(ctx: &GateCtx<'_>) -> Outcome {
    let base = ctx.deps.config.project.default_branch.clone();
    evaluate_with_base(ctx, &base)
}

/// Evaluates the `secret_scan` gate over a rebased merge candidate.
pub(crate) fn evaluate_merge(ctx: &GateCtx<'_>) -> Outcome {
    let base = format!(
        "{}/{}",
        ctx.deps.config.project.remote, ctx.deps.config.project.default_branch
    );
    evaluate_with_base(ctx, &base)
}

/// Scans the changed paths and added lines of `base...job/<id>`.
fn evaluate_with_base(ctx: &GateCtx<'_>, base: &str) -> Outcome {
    let start = Instant::now();
    let branch = ctx.job.branch.clone().unwrap_or_else(|| format!("job/{}", ctx.job.id));
    let changed = match ctx.deps.git.diff_names(ctx.worktree, base, &branch) {
        Ok(paths) => paths,
        Err(err) => return scan_error(ctx, err, start),
    };
    let diff = match ctx.deps.git.diff_text(ctx.worktree, base, &branch) {
        Ok(diff) => diff,
        Err(err) => return scan_error(ctx, err, start),
    };
    let mut hits = scan_paths(&changed);
    hits.extend(scan_diff(&diff));
    Outcome {
        passed: hits.is_empty(),
        evidence: Evidence::SecretScan {
            files_scanned: u64::try_from(changed.len()).unwrap_or(u64::MAX),
            hits,
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Records a failed scan when the diff cannot be read; the reason goes to the supervisor log.
fn scan_error(ctx: &GateCtx<'_>, err: crate::CoreError, start: Instant) -> Outcome {
    ctx.deps.log.log(
        vowp_protocol::Level::Warn,
        "gates",
        &format!("secret scan could not read the job diff: {err}"),
    );
    Outcome {
        passed: false,
        evidence: Evidence::SecretScan {
            files_scanned: 0,
            hits: Vec::new(),
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Returns the compiled rules, built once per process.
fn compiled() -> &'static [Regex] {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        rules()
            .iter()
            .map(|rule| {
                // invariant: the RULES table is a fixed set of valid regex literals, checked
                // by secret_scan_detects_each_rule_by_exact_name_and_never_stores_value.
                Regex::new(rule.regex).expect("invariant: secret rule patterns are valid regexes")
            })
            .collect()
    })
}

/// Scans one text file and returns hits with the rule name and one-based line number.
pub fn scan_text(path: &str, text: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        for (rule, regex) in rules().iter().zip(compiled()) {
            if regex.is_match(line) {
                hits.push(SecretHit {
                    path: path.to_owned(),
                    line: line_number,
                    rule: rule.name.to_owned(),
                });
            }
        }
    }
    hits
}

/// Flags paths whose file name matches `.env*`, `*.pem`, `id_rsa*` or `id_ed25519*`.
///
/// Hits use the rule `secret_file` and line `0`, because the finding is the path itself.
pub fn scan_paths(paths: &[String]) -> Vec<SecretHit> {
    paths
        .iter()
        .filter(|path| is_secret_file(path))
        .map(|path| SecretHit {
            path: path.clone(),
            line: 0,
            rule: "secret_file".to_owned(),
        })
        .collect()
}

/// Scans only added lines of a unified diff; removed and context lines are ignored.
pub fn scan_diff(diff: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    let mut path: Option<String> = None;
    let mut new_line = 0_u32;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            path = None;
            new_line = 0;
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            if let Some(rest) = line.strip_prefix("+++ ") {
                path = if rest == "/dev/null" {
                    None
                } else {
                    Some(rest.strip_prefix("b/").unwrap_or(rest).to_owned())
                };
                continue;
            }
            if line.starts_with("--- ") {
                continue;
            }
        }
        if line.starts_with("@@") {
            new_line = hunk_new_start(line).unwrap_or(0);
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if let Some(path) = &path {
                if new_line > 0 {
                    scan_added_line(path, added, new_line, &mut hits);
                }
            }
            new_line = new_line.saturating_add(1);
            continue;
        }
        if line.starts_with('-') || line.starts_with('\\') {
            continue;
        }
        if path.is_some() && new_line > 0 {
            new_line = new_line.saturating_add(1);
        }
    }
    hits
}

/// Adds hits for one added line.
fn scan_added_line(path: &str, text: &str, line: u32, hits: &mut Vec<SecretHit>) {
    for (rule, regex) in rules().iter().zip(compiled()) {
        if regex.is_match(text) {
            hits.push(SecretHit {
                path: path.to_owned(),
                line,
                rule: rule.name.to_owned(),
            });
        }
    }
}

/// Returns the new-file line number from a `@@ -a,b +c,d @@` header.
fn hunk_new_start(header: &str) -> Option<u32> {
    let after_plus = header.split_once('+')?.1;
    let digits: String = after_plus.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Returns true when a path is a secret-bearing file name.
fn is_secret_file(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    name.starts_with(".env") || name.ends_with(".pem") || name.starts_with("id_rsa") || name.starts_with("id_ed25519")
}
