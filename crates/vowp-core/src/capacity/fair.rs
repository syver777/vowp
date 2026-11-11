//! Fair ordering of projects: weight first, then the least recently admitted project.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.2 rule 7. Fairness is ordering only:
//! admission never blocks a run because another project should go first.

use vowp_protocol::{ProjectEntry, ProjectName};

/// Orders projects by fairness weight and last admission.
///
/// Higher weights come first. Inside one weight the project admitted least recently (the
/// one after `last` in the current rotation) comes first, which round-robins equal projects
/// each tick instead of letting one of them keep the front slot forever.
#[allow(clippy::ptr_arg)] // The signature is part of the public capacity API.
pub fn order(projects: &mut Vec<ProjectEntry>, last: Option<&ProjectName>) {
    projects.sort_by(|left, right| right.weight.cmp(&left.weight).then_with(|| left.name.cmp(&right.name)));

    let mut start = 0;
    while start < projects.len() {
        let weight = projects[start].weight;
        let mut end = start + 1;
        while end < projects.len() && projects[end].weight == weight {
            end += 1;
        }
        if let Some(last) = last
            && let Some(index) = projects[start..end].iter().position(|project| &project.name == last)
        {
            projects[start..end].rotate_left(index + 1);
        }
        start = end;
    }
}
