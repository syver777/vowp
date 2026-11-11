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

/// The orchestrator skill, with the phone join qualified for Claude Remote Control.
///
/// The repository skill is the source. The appended note keeps the copy installed by
/// `vowp init` from telling a Claude Remote Control owner to use the opencode SSH
/// command for a phone join.
const ORCHESTRATOR_SKILL: &str = concat!(
    include_str!("../../../framework/skills/vowp-orchestrator/SKILL.md"),
    "\n\n## Phone join by app\n\n\
     The SSH `join.phone` command is for opencode and `vowp chat`. For Claude Remote Control,\n\
     open the Claude mobile app and choose the same Remote Control session instead; do not run\n\
     the opencode SSH command there.\n"
);

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
        target: ".claude/skills/vowp-capacity-advisor/SKILL.md",
        content: include_str!("../../../framework/skills/vowp-capacity-advisor/SKILL.md"),
    },
    FrameworkFile {
        target: ".claude/skills/vowp-orchestrator/SKILL.md",
        content: ORCHESTRATOR_SKILL,
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

#[cfg(test)]
mod tests {
    use super::FILES;

    #[test]
    fn capacity_advisor_skill_is_embedded() {
        let skill = FILES
            .iter()
            .find(|file| file.target == ".claude/skills/vowp-capacity-advisor/SKILL.md")
            .expect("the capacity advisor skill is installed by vowp init");
        assert!(skill.content.contains("name: vowp-capacity-advisor"));
        assert!(skill.content.contains("vowp capacity set --target"));
        assert!(skill.content.contains("never change"));
    }

    #[test]
    fn orchestrator_skill_and_instructions_are_embedded() {
        let skill = FILES
            .iter()
            .find(|file| file.target == ".claude/skills/vowp-orchestrator/SKILL.md")
            .expect("the orchestrator skill is installed by vowp init");
        assert!(skill.content.contains("name: vowp-orchestrator"));
        assert!(skill.content.contains("fleet_status"));
        assert!(skill.content.contains("session_resume"));
        assert!(skill.content.contains("control-machine"));
        assert!(!skill.content.contains("they include the tunnel"));
        assert!(
            skill.content.contains("For Claude Remote Control"),
            "the installed skill must qualify the phone join for Claude Remote Control"
        );
        assert!(
            skill.content.contains("open the Claude mobile app"),
            "the installed skill must say to use the Claude app, not SSH, for Remote Control"
        );
        let instructions = vowp_core::surface::orchestrator::ORCHESTRATOR_INSTRUCTIONS;
        assert!(instructions.contains("Never open, query or edit a database"));
        assert!(instructions.contains("Never read, print, log or commit a secret"));
        assert!(instructions.contains("control machine"));
        assert!(instructions.contains("cannot open tunnels itself"));
        assert_eq!(
            instructions,
            include_str!("../../../framework/templates/ORCHESTRATOR.md"),
            "the crate instructions and the framework template drifted"
        );
    }
}
