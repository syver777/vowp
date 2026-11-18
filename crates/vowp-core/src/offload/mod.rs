//! Pure offload eligibility: where a job may run and why it is waiting.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.6, §6.7 and §7.1–§7.5. This module
//! decides only; it opens no store, starts no process, reads no clock and touches no
//! filesystem. Callers pass the effective policy and one [`NodeFacts`] per candidate node.

pub mod policy;

pub use policy::{
    Decision, HomeFacts, JobRequest, JobRequirements, LimitFact, NodeCapabilities, NodeFacts, NodePower, PathFacts,
    Target, ThermalState, evaluate,
};
