//! Capacity types: measured host samples, per-run usage and admission decisions.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1–§6.5 and §13.1. The wire shapes are
//! fixed here; the sampler, admission, brakes, targets and advice live in `vowp-core`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{JobId, NodeName, ProjectName, RunId};
use crate::job::JobKind;
use crate::limit::Limit;
use crate::run::RunRole;
use crate::time::Timestamp;

/// Where a [`HostSample`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    /// Read from `/proc` and the cgroup filesystem.
    Proc,
    /// Only the portable reads (`statvfs`, `sysctl`) were available.
    Fallback,
}

/// One measurement of the whole node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostSample {
    /// When the sample was taken.
    pub at: Timestamp,
    /// Sampling interval that produced this row.
    pub interval_secs: u64,
    /// Total physical memory in bytes.
    pub mem_total_bytes: u64,
    /// Memory available to new work in bytes.
    pub mem_available_bytes: u64,
    /// Swap in use in bytes.
    pub swap_used_bytes: u64,
    /// One-minute load average.
    pub load1: f64,
    /// Five-minute load average.
    pub load5: f64,
    /// Fifteen-minute load average.
    pub load15: f64,
    /// Number of logical CPUs.
    pub cpu_count: u32,
    /// Cgroup v2 CPU quota in cores, when present.
    pub cpu_quota_cores: Option<f64>,
    /// Free bytes on the filesystem that holds the home.
    pub disk_free_home_bytes: u64,
    /// Free bytes on the filesystem that holds worktrees, when different.
    pub disk_free_worktrees_bytes: Option<u64>,
    /// `/proc/pressure/cpu` `avg10`, when present.
    pub pressure_cpu_avg10: Option<f64>,
    /// `/proc/pressure/memory` `avg10`, when present.
    pub pressure_mem_avg10: Option<f64>,
    /// `/proc/pressure/io` `avg10`, when present.
    pub pressure_io_avg10: Option<f64>,
    /// Read path that produced the sample.
    pub source: SampleSource,
}

/// Where a [`RunUsage`] row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Measured through a cgroup v2 scope.
    Cgroup,
    /// Measured by walking the process tree under `/proc`.
    ProcTree,
    /// The compiled default because neither measurement was available.
    Default,
}

/// Inference about how warm the job's target directory was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode {
    /// A cold build with no usable target directory.
    Cold,
    /// A warm build with a populated target directory.
    Warm,
    /// An incremental build.
    Incremental,
    /// The build mode could not be told.
    #[default]
    Unknown,
}

/// One finished run's measured resource use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunUsage {
    /// Run the row describes.
    pub run_id: RunId,
    /// Project the run belongs to.
    pub project: ProjectName,
    /// Job the run worked on, when any.
    pub job_id: Option<JobId>,
    /// Kind of the job, when any.
    pub job_kind: Option<JobKind>,
    /// Role the run served.
    pub role: RunRole,
    /// Harness that ran, when any.
    pub harness: Option<crate::executor::HarnessKind>,
    /// Model that ran, when known.
    pub model: Option<String>,
    /// Provider that served the model, when known.
    pub provider: Option<String>,
    /// Executor node.
    pub node: NodeName,
    /// Start of the run.
    pub started_at: Timestamp,
    /// End of the run.
    pub ended_at: Timestamp,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Peak resident memory in bytes.
    pub peak_rss_bytes: u64,
    /// Sampled anonymous memory at the peak, when sampled.
    pub anon_peak_bytes: Option<u64>,
    /// Sampled file-backed pages at the peak, when sampled.
    pub file_peak_bytes: Option<u64>,
    /// Sampled kernel memory at the peak, when sampled.
    pub kernel_peak_bytes: Option<u64>,
    /// CPU time in microseconds.
    pub cpu_usec: u64,
    /// Mean CPU cores used.
    pub mean_cpu_cores: f64,
    /// Maximum CPU cores observed.
    pub max_cpu_cores: f64,
    /// Build-mode inference.
    pub build_mode: BuildMode,
    /// Measurement source.
    pub source: UsageSource,
    /// Whether the kernel OOM killer ended the run.
    pub oom_killed: bool,
    /// Process exit code, when the process exited normally.
    pub exit_code: Option<i32>,
}

/// What a remote `__exec` measured on the executor node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteUsage {
    /// Peak resident memory in bytes.
    pub peak_rss_bytes: u64,
    /// Sampled anonymous memory at the peak, when sampled.
    pub anon_peak_bytes: Option<u64>,
    /// Sampled file-backed pages at the peak, when sampled.
    pub file_peak_bytes: Option<u64>,
    /// Sampled kernel memory at the peak, when sampled.
    pub kernel_peak_bytes: Option<u64>,
    /// CPU time in microseconds.
    pub cpu_usec: u64,
    /// Mean CPU cores used.
    pub mean_cpu_cores: f64,
    /// Maximum CPU cores observed.
    pub max_cpu_cores: f64,
    /// Build-mode inference.
    pub build_mode: BuildMode,
    /// Whether the kernel OOM killer ended the run.
    pub oom_killed: bool,
    /// Measurement source.
    pub source: UsageSource,
}

/// Where an [`UsageEstimate`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EstimateSource {
    /// Computed from enough exact-group samples.
    Measured,
    /// Pooled across a wider group.
    Pooled,
    /// The compiled default.
    Default,
}

/// Pooled resource estimate for one `(project, job kind, harness, model, build mode)` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageEstimate {
    /// Project key; `None` is the pooled empty key.
    pub project: Option<ProjectName>,
    /// Job-kind key; `None` is the pooled empty key.
    pub job_kind: Option<JobKind>,
    /// Harness key; `None` is the pooled empty key.
    pub harness: Option<crate::executor::HarnessKind>,
    /// Model key; `None` is the pooled empty key.
    pub model: Option<String>,
    /// Build-mode key; `None` is the pooled `unknown` key.
    pub build_mode: Option<BuildMode>,
    /// Number of samples the estimate used.
    pub samples: u32,
    /// Median peak resident memory in bytes.
    pub ram_p50_bytes: u64,
    /// p90 peak resident memory in bytes.
    pub ram_p90_bytes: u64,
    /// Median CPU cores.
    pub cpu_p50_cores: f64,
    /// p90 CPU cores.
    pub cpu_p90_cores: f64,
    /// Median duration in milliseconds.
    pub duration_p50_ms: u64,
    /// p90 duration in milliseconds.
    pub duration_p90_ms: u64,
    /// Estimate source.
    pub source: EstimateSource,
}

/// Everything admission needs to know about one would-be run.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmitRequest {
    /// Project the run belongs to.
    pub project: ProjectName,
    /// Job the run would work on, when any.
    pub job: Option<JobId>,
    /// Kind of the job.
    pub kind: JobKind,
    /// Role the run would serve.
    pub role: RunRole,
    /// Harness that would run.
    pub harness: Option<crate::executor::HarnessKind>,
    /// Model that would run.
    pub model: Option<String>,
    /// Provider that would serve the model.
    pub provider: Option<String>,
    /// Executor node.
    pub node: NodeName,
}

/// The result of one admission decision.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmitDecision {
    /// The run may start.
    Admit,
    /// The run must wait for the named resource.
    Wait(WaitReason),
}

/// One human-readable reason a run is waiting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WaitReason {
    /// Machine-readable category.
    pub code: WaitCode,
    /// One human sentence, for example `RAM 1.2 GB short`.
    pub detail: String,
}

/// Why a run is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitCode {
    /// Not enough free RAM.
    Ram,
    /// Not enough CPU headroom.
    Cpu,
    /// Not enough free disk.
    Disk,
    /// A provider limit is active.
    Provider,
    /// A brake is active.
    Brake,
    /// The target number of workers is already running.
    Target,
}

/// The node's active brake state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrakeState {
    /// Whether the brake is active.
    pub active: bool,
    /// Reasons the brake is active.
    pub reasons: Vec<BrakeReason>,
    /// When the brake first became active.
    pub since: Option<Timestamp>,
}

/// One reason a brake is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrakeReason {
    /// Machine-readable category.
    pub code: BrakeCode,
    /// One human sentence.
    pub detail: String,
}

/// Why the node brake tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrakeCode {
    /// Memory pressure stayed high.
    MemoryPressure,
    /// Swap use grew quickly.
    SwapGrowth,
    /// Free disk dropped below the reserve.
    DiskLow,
    /// Every candidate provider is limited.
    Provider,
}

/// How the worker target is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    /// The supervisor derives the target from measurements.
    Auto,
    /// The owner fixed the target.
    Fixed,
}

/// The current node-wide worker target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityTarget {
    /// How the target is chosen.
    pub mode: TargetMode,
    /// Number of workers that may run.
    pub target_workers: u32,
    /// The supervisor's advised number, when known.
    pub advised_workers: Option<u32>,
    /// Whether the owner raised the target above the advice.
    pub is_override: bool,
    /// Who set the target: `owner`, `supervisor` or `setup`.
    pub set_by: String,
    /// When the target was set.
    pub set_at: Timestamp,
    /// Free-form reason.
    pub reason: String,
}

impl Default for CapacityTarget {
    fn default() -> Self {
        Self {
            mode: TargetMode::Auto,
            target_workers: 1,
            advised_workers: None,
            is_override: false,
            set_by: "supervisor".to_owned(),
            set_at: Timestamp::from_unix_secs(0),
            reason: String::new(),
        }
    }
}

/// State of one capacity advice row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdviceState {
    /// Waiting for the owner.
    Open,
    /// Applied by the owner.
    Applied,
    /// Declined by the owner.
    Dismissed,
}

/// One proposal to change the worker target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityAdvice {
    /// Advice row id (ULID).
    pub id: String,
    /// When the advice was produced.
    pub created_at: Timestamp,
    /// Target before the proposal.
    pub current_target: u32,
    /// Proposed target.
    pub proposed_target: u32,
    /// Target mode the proposal applies to.
    pub mode: TargetMode,
    /// Human-readable reasons.
    pub reasons: Vec<String>,
    /// Expected effect of applying the advice.
    pub expected_effect: String,
    /// Linked node inbox item, when one was written.
    pub inbox_id: Option<crate::ids::InboxId>,
    /// Current state.
    pub state: AdviceState,
    /// When the owner applied it.
    pub applied_at: Option<Timestamp>,
    /// Who applied it.
    pub applied_by: Option<String>,
}

/// Current running-capacity totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningCapacity {
    /// Number of open runs.
    pub runs: u32,
    /// p90 memory of the open runs in bytes.
    pub ram_p90_bytes: u64,
    /// p90 CPU cores of the open runs.
    pub cpu_p90_cores: f64,
}

/// One job waiting for capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WaitingJob {
    /// Project the job belongs to.
    pub project: ProjectName,
    /// Waiting job.
    pub job: JobId,
    /// Why it is waiting.
    pub reason: WaitReason,
    /// When it started waiting.
    pub since: Timestamp,
}

/// The capacity card embedded in `snapshot.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacitySnapshot {
    /// Latest host sample, when the sampler has run.
    pub measured: Option<HostSample>,
    /// Running-capacity totals.
    pub running: RunningCapacity,
    /// Current worker target.
    pub target: CapacityTarget,
    /// The supervisor's advised target, when known.
    pub advised_target: Option<u32>,
    /// Brake state.
    pub brake: BrakeState,
    /// RAM kept free for the node in bytes.
    pub reserve_ram_bytes: u64,
    /// Disk kept free for the node in bytes.
    pub min_disk_bytes: u64,
}

/// Output of `vowp capacity show` and `vowp capacity report`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityReport {
    /// When the report was generated.
    pub generated_at: Timestamp,
    /// Latest host sample, when one exists.
    pub host: Option<HostSample>,
    /// Current target.
    pub target: CapacityTarget,
    /// Running-capacity totals.
    pub running: RunningCapacity,
    /// Brake state.
    pub brakes: BrakeState,
    /// RAM reserve in bytes.
    pub reserve_ram_bytes: u64,
    /// Disk reserve in bytes.
    pub min_disk_bytes: u64,
    /// Usage estimates included in the report.
    pub estimates: Vec<UsageEstimate>,
    /// Jobs waiting for capacity.
    pub waiting: Vec<WaitingJob>,
    /// Provider limits.
    pub limits: Vec<Limit>,
}
