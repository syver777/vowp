//! The framework files embedded in the `vowp` binary.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §1 and `framework/README.md`. The source of truth is
//! the repository `framework/` directory; `vowp init` writes these exact bytes. `CLAUDE.md`
//! is a one-line import of `AGENTS.md`.

/// One embedded framework file.
#[derive(Debug, Clone, Copy)]
pub struct FrameworkFile {
    /// Repository-relative path the file is installed to.
    pub target: &'static str,
    /// File contents.
    pub content: &'static str,
}

/// Every file `vowp init` installs, in install order.
pub const FILES: &[FrameworkFile] = &[
    FrameworkFile {
        target: "AGENTS.md",
        content: include_str!("../../../framework/templates/AGENTS.md"),
    },
    FrameworkFile {
        target: "CLAUDE.md",
        content: include_str!("../../../framework/templates/CLAUDE.md"),
    },
    FrameworkFile {
        target: ".vowp/templates/brief.md",
        content: include_str!("../../../framework/templates/brief.md"),
    },
    FrameworkFile {
        target: ".vowp/templates/report.md",
        content: include_str!("../../../framework/templates/report.md"),
    },
    FrameworkFile {
        target: ".claude/skills/vowp-worker/SKILL.md",
        content: include_str!("../../../framework/skills/vowp-worker/SKILL.md"),
    },
    FrameworkFile {
        target: ".claude/skills/vowp-planner/SKILL.md",
        content: include_str!("../../../framework/skills/vowp-planner/SKILL.md"),
    },
    FrameworkFile {
        target: ".claude/skills/vowp-verifier/SKILL.md",
        content: include_str!("../../../framework/skills/vowp-verifier/SKILL.md"),
    },
    FrameworkFile {
        target: ".claude/skills/vowp-keeper/SKILL.md",
        content: include_str!("../../../framework/skills/vowp-keeper/SKILL.md"),
    },
    FrameworkFile {
        target: "opencode.jsonc",
        content: include_str!("../../../framework/opencode/opencode.jsonc"),
    },
];

/// Returns the body of the embedded brief template, without its example front-matter.
pub fn brief_template_body() -> &'static str {
    let content = include_str!("../../../framework/templates/brief.md");
    match content.match_indices("---").nth(1) {
        Some((end, _)) => content[end + 3..].trim_start_matches(['\r', '\n']),
        None => content,
    }
}
