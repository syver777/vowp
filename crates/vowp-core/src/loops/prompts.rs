//! Prompt templates for the L4 planner, L5 keeper and L6 probe turns.
//!
//! Spec: `docs/design/loops.md` §7 and §8.5. The planner plans the graph, the keeper
//! diagnoses stalled work and, while the planner's providers are limited, covers the
//! planner duties with the planner digest. Neither role edits the DB or merges; both act
//! only through the `vowp` CLI, which writes request rows for the next tick.

/// The single prompt of an L6 probe-back run.
pub const PROBE_PROMPT: &str = "Reply with the single word: ok";

/// Renders the planner turn prompt.
pub fn planner_prompt(digest: &str, project: &str) -> String {
    format!(
        "You are the planner for project {project}. Plan the work that keeps the project moving.\n\
         Read the digest below, then use the `vowp` CLI to act: `vowp job add`, `vowp job update`, \
         `vowp job retry|park|cancel`, and write brief files in the job worktrees. Add jobs with \
         small, disjoint `owns` paths.\n\
         Never edit the database directly, never merge, never push. Every state change goes through \
         the CLI (it writes a request row that the next tick applies). One turn at a time; when the \
         plan is clear, stop. The keeper covers you while your provider is limited.\n\
         \n\
         # Digest\n\
         {digest}\n"
    )
}

/// Renders the keeper turn prompt.
///
/// When `planner_digest` is present the planner's providers are limited and this keeper turn
/// covers the planner duties as well; the planner digest is appended after the keeper digest.
pub fn keeper_prompt(digest: &str, planner_digest: Option<&str>) -> String {
    let mut text = String::from(
        "You are the keeper for this project. Diagnose and unblock work that stopped moving.\n\
         Read the digest below. Act only through the `vowp` CLI: `vowp job nudge <id>`, \
         `vowp job restart <id>`, `vowp job replan <id>`, `vowp job fail <id>`, \
         `vowp job retry|park|cancel <id>`, and `vowp inbox ask`. Never edit the database, \
         never merge and never push. One turn at a time; stop when the night is healthy or the \
         problem needs a human.\n\
         \n\
         # Digest\n",
    );
    text.push_str(digest);
    text.push('\n');
    if let Some(planner_digest) = planner_digest {
        text.push_str(
            "\n# Planner coverage\n\
             The planner's providers are limited, so you also cover the planner duties this turn: \
             plan the job graph, add or update jobs with small disjoint `owns` paths, and write \
             brief files. Use the same CLI-only rule.\n\
             \n# Planner digest\n",
        );
        text.push_str(planner_digest);
        text.push('\n');
    }
    text
}
