//! Report and brief front-matter parsing, rendering and the `report` gate's field check.
//! Spec: `docs/design/types.md` §8.1–8.2 and `docs/design/crates/vowp-protocol.md` §4 rule 9.
//!
//! Both files are Markdown with a YAML front-matter block delimited by lines that are exactly
//! `---`. Parsing keeps the raw YAML as JSON so [`Report::missing_fields`] can distinguish an
//! absent key from an empty list, then decodes the typed front matter.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::gate::Gate;
use crate::ids::JobId;
use crate::job::Job;

/// Status a worker reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// All work is complete.
    Done,
    /// Some work is complete; open items remain.
    Partial,
    /// Work could not continue.
    Blocked,
}

/// Verifier verdict for a reviewed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The work may proceed.
    Pass,
    /// The work needs a fix round.
    Fail,
}

/// Structured test result for a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TestsField {
    /// The plan's structured form.
    Structured {
        /// Command that ran.
        command: String,
        /// Number of tests that passed.
        #[serde(default)]
        passed: Option<u32>,
        /// Number of tests that failed.
        #[serde(default)]
        failed: Option<u32>,
    },
    /// The free-string form used by early briefs.
    Text(String),
}

/// YAML front matter of a job report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportFrontMatter {
    /// Job the report belongs to.
    pub job: JobId,
    /// Report status.
    pub status: ReportStatus,
    /// Branch the work was committed on.
    pub branch: String,
    /// Commits produced by the job.
    #[serde(default)]
    pub commits: Vec<String>,
    /// Test command and result, structured or free text.
    #[serde(default)]
    pub tests: Option<TestsField>,
    /// Files changed by the job.
    #[serde(default)]
    pub files_changed: Vec<String>,
    /// Known remaining work.
    #[serde(default)]
    pub open_items: Vec<String>,
    /// Questions queued for the owner.
    #[serde(default)]
    pub questions_for_owner: Vec<String>,
    /// Verifier verdict, when this is a verify report.
    #[serde(default)]
    pub verdict: Option<Verdict>,
    /// Verifier findings, when this is a verify report.
    #[serde(default)]
    pub findings: Vec<String>,
}

/// A parsed report: typed front matter, raw front matter and the Markdown body.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Decoded front matter.
    pub front_matter: ReportFrontMatter,
    /// Markdown after the closing fence, with its leading newline stripped.
    pub body: String,
    /// Raw YAML front matter converted to JSON.
    pub raw_front_matter: serde_json::Value,
}

impl Report {
    /// Parses `---\n<yaml>\n---\n<body>`.
    ///
    /// Errors are [`ProtocolError::FrontMatter`]: missing opening or closing fence, invalid
    /// YAML, an invalid enum value, or a missing required field (`job`, `status`, `branch`).
    pub fn parse(markdown: &str) -> Result<Self, ProtocolError> {
        let (yaml, body) = split_front_matter(markdown)?;
        let raw_front_matter: serde_json::Value =
            serde_yaml_ng::from_str(yaml).map_err(|err| ProtocolError::FrontMatter(format!("invalid YAML: {err}")))?;
        let front_matter: ReportFrontMatter = serde_json::from_value(raw_front_matter.clone())
            .map_err(|err| ProtocolError::FrontMatter(err.to_string()))?;
        Ok(Self {
            front_matter,
            body: body.to_owned(),
            raw_front_matter,
        })
    }

    /// Renders the report as Markdown with a YAML front-matter block.
    pub fn render(&self) -> String {
        // invariant: ReportFrontMatter contains only YAML-serialisable values.
        let yaml = serde_yaml_ng::to_string(&self.front_matter).expect("front matter is serialisable");
        format!("---\n{}\n---\n{}", yaml.trim_end_matches('\n'), self.body)
    }

    /// Returns the required field names that are absent or empty in the raw front matter.
    ///
    /// An absent key, `null` and `""` are missing. An empty list counts as present for list
    /// fields such as `commits`, `open_items`, `questions_for_owner` and `files_changed`.
    pub fn missing_fields(&self, required: &[String]) -> Vec<String> {
        required
            .iter()
            .filter(|name| match self.raw_front_matter.get(name.as_str()) {
                None | Some(serde_json::Value::Null) => true,
                Some(serde_json::Value::String(value)) => value.is_empty(),
                Some(_) => false,
            })
            .cloned()
            .collect()
    }
}

/// YAML front matter of a job brief.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BriefFrontMatter {
    /// Job the brief belongs to.
    pub job: JobId,
    /// Paths the job may change.
    pub owns: Vec<String>,
    /// Paths the job must not change.
    #[serde(default)]
    pub must_not_touch: Vec<String>,
    /// Jobs that must be done first.
    #[serde(default)]
    pub needs_done: Vec<JobId>,
    /// Gate short forms.
    #[serde(default)]
    pub gates: Vec<String>,
    /// The condition under which the job is done.
    #[serde(default)]
    pub done_when: String,
}

/// A parsed brief: typed front matter and the Markdown body.
#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    /// Decoded front matter.
    pub front_matter: BriefFrontMatter,
    /// Markdown after the closing fence, with its leading newline stripped.
    pub body: String,
}

impl Brief {
    /// Parses `---\n<yaml>\n---\n<body>` with the same rules as [`Report::parse`].
    pub fn parse(markdown: &str) -> Result<Self, ProtocolError> {
        let (yaml, body) = split_front_matter(markdown)?;
        let front_matter: BriefFrontMatter =
            serde_yaml_ng::from_str(yaml).map_err(|err| ProtocolError::FrontMatter(format!("invalid YAML: {err}")))?;
        Ok(Self {
            front_matter,
            body: body.to_owned(),
        })
    }

    /// Renders the brief as Markdown with a YAML front-matter block.
    pub fn render(&self) -> String {
        // invariant: BriefFrontMatter contains only YAML-serialisable values.
        let yaml = serde_yaml_ng::to_string(&self.front_matter).expect("front matter is serialisable");
        format!("---\n{}\n---\n{}", yaml.trim_end_matches('\n'), self.body)
    }

    /// Builds a brief from a graph node and the planner's body text.
    ///
    /// Ownership, needs and gates come from the job; the remaining fields use the standard
    /// brief wording.
    pub fn from_job(job: &Job, body: &str) -> Brief {
        Brief {
            front_matter: BriefFrontMatter {
                job: job.id.clone(),
                owns: job.owns.clone(),
                must_not_touch: vec!["everything else".to_owned()],
                needs_done: job.needs.clone(),
                gates: job.gates.iter().map(Gate::short).collect(),
                done_when: "report has status: done and every gate passes".to_owned(),
            },
            body: body.to_owned(),
        }
    }
}

/// Splits a Markdown front-matter block into its YAML and body.
///
/// The opening line must be exactly `---`; the block ends at the next line that is exactly
/// `---`. The body starts after that line's newline, so its leading newline is stripped.
fn split_front_matter(markdown: &str) -> Result<(&str, &str), ProtocolError> {
    let rest = markdown
        .strip_prefix("---\n")
        .or_else(|| markdown.strip_prefix("---\r\n"))
        .ok_or_else(|| ProtocolError::FrontMatter("missing opening '---' fence".to_owned()))?;

    let mut pos = 0;
    while pos <= rest.len() {
        let line_end = rest[pos..]
            .find('\n')
            .map(|offset| pos + offset + 1)
            .unwrap_or(rest.len());
        let line = &rest[pos..line_end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "---" {
            return Ok((&rest[..pos], &rest[line_end..]));
        }
        if line_end == rest.len() {
            break;
        }
        pos = line_end;
    }
    Err(ProtocolError::FrontMatter("missing closing '---' fence".to_owned()))
}
