//! Per-run resource scopes: a cgroup v2 user scope when available, `/proc` tree walk otherwise.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1. The cgroup path is preferred; the
//! process tree is the fallback when the user manager or cgroup delegation is unavailable.

use std::collections::{BTreeMap, HashSet};
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use vowp_protocol::{BuildMode, ProjectName, RunId, RunUsage, Timestamp, UsageSource};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::{CommandOutput, CommandSpec, Deps};

/// The default peak used when neither cgroup nor `/proc` measurement is available.
pub const DEFAULT_PEAK_RSS_BYTES: u64 = vowp_adapters::proc::DEFAULT_PEAK_RSS_BYTES;

/// The default CPU estimate used when no measurement is available.
pub const DEFAULT_MEAN_CPU_CORES: f64 = vowp_adapters::proc::DEFAULT_MEAN_CPU_CORES;

/// Clock ticks per second assumed by Linux `/proc/<pid>/stat` (USER_HZ).
const CLOCK_TICKS: f64 = 100.0;

/// A unit counter keeps transient systemd scopes unique within one process.
static UNIT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One sampled scope measurement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScopeSample {
    /// Peak resident memory in bytes.
    pub peak_rss_bytes: u64,
    /// Sampled anonymous memory at the peak.
    pub anon_peak_bytes: Option<u64>,
    /// Sampled file-backed pages at the peak.
    pub file_peak_bytes: Option<u64>,
    /// Sampled kernel memory at the peak.
    pub kernel_peak_bytes: Option<u64>,
    /// CPU time in microseconds.
    pub cpu_usec: u64,
    /// Mean CPU cores used.
    pub mean_cpu_cores: f64,
    /// Maximum CPU cores observed.
    pub max_cpu_cores: f64,
    /// Whether the kernel OOM killer ended the run.
    pub oom_killed: bool,
}

/// Returns the compiled default sample used when no measurement was read.
pub(crate) fn default_scope_sample() -> ScopeSample {
    ScopeSample {
        peak_rss_bytes: DEFAULT_PEAK_RSS_BYTES,
        anon_peak_bytes: None,
        file_peak_bytes: None,
        kernel_peak_bytes: None,
        cpu_usec: 0,
        mean_cpu_cores: DEFAULT_MEAN_CPU_CORES,
        max_cpu_cores: DEFAULT_MEAN_CPU_CORES,
        oom_killed: false,
    }
}

/// Replaces an empty measurement with the compiled `source=default` row.
pub(crate) fn effective_sample(sample: ScopeSample, source: UsageSource) -> (ScopeSample, UsageSource) {
    let empty = sample.peak_rss_bytes == 0
        && sample.cpu_usec == 0
        && sample.anon_peak_bytes.is_none()
        && sample.file_peak_bytes.is_none()
        && sample.kernel_peak_bytes.is_none()
        && !sample.oom_killed;
    if empty {
        (default_scope_sample(), UsageSource::Default)
    } else {
        (sample, source)
    }
}

/// The filesystem reads the sampler and scope need.
pub trait ProcFs: Send + Sync {
    /// Reads one pseudo-file as UTF-8.
    fn read(&self, path: &str) -> Result<String, CoreError>;
    /// Returns the direct children of `pid`.
    fn children(&self, pid: u32) -> Result<Vec<u32>, CoreError>;
    /// Returns the free bytes of the filesystem that holds `path`.
    fn disk_free(&self, path: &Path) -> Result<u64, CoreError>;
}

/// The real `/proc` and filesystem implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcFs;

impl ProcFs for SystemProcFs {
    fn read(&self, path: &str) -> Result<String, CoreError> {
        std::fs::read_to_string(path).map_err(|source| CoreError::Io {
            path: path.to_owned(),
            source,
        })
    }

    fn children(&self, pid: u32) -> Result<Vec<u32>, CoreError> {
        let dir = format!("/proc/{pid}/task");
        let tasks = std::fs::read_dir(&dir).map_err(|source| CoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut children = Vec::new();
        for task in tasks {
            let task = task.map_err(|source| CoreError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = task.path().join("children");
            let text = std::fs::read_to_string(&path).map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
            children.extend(text.split_whitespace().filter_map(|value| value.parse::<u32>().ok()));
        }
        Ok(children)
    }

    fn disk_free(&self, path: &Path) -> Result<u64, CoreError> {
        let existing = nearest_existing(path);
        let output = Command::new("df")
            .arg("-Pk")
            .arg(&existing)
            .output()
            .map_err(|source| CoreError::Io {
                path: existing.display().to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(CoreError::Command(format!(
                "df failed for {}: {}",
                existing.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .nth(1)
            .ok_or_else(|| CoreError::Command("df produced no filesystem line".to_owned()))?;
        let available_kib = line
            .split_whitespace()
            .nth(3)
            .ok_or_else(|| CoreError::Command("df produced no available-block column".to_owned()))?
            .parse::<u64>()
            .map_err(|err| CoreError::Command(format!("bad df available blocks: {err}")))?;
        Ok(available_kib.saturating_mul(1024))
    }
}

/// Returns `path` or its nearest existing ancestor.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return current;
        }
        if !current.pop() {
            return PathBuf::from("/");
        }
    }
}

/// A fixture-backed [`ProcFs`] for tests.
#[derive(Debug, Clone, Default)]
pub struct FakeProcFs {
    /// Pseudo-file contents keyed by path.
    pub files: BTreeMap<String, String>,
    /// Process children keyed by parent pid.
    pub children: BTreeMap<u32, Vec<u32>>,
    /// Free disk bytes keyed by filesystem path.
    pub disk: BTreeMap<String, u64>,
}

impl ProcFs for FakeProcFs {
    fn read(&self, path: &str) -> Result<String, CoreError> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(path.to_owned()))
    }

    fn children(&self, pid: u32) -> Result<Vec<u32>, CoreError> {
        Ok(self.children.get(&pid).cloned().unwrap_or_default())
    }

    fn disk_free(&self, path: &Path) -> Result<u64, CoreError> {
        self.disk
            .get(&path.display().to_string())
            .copied()
            .ok_or_else(|| CoreError::NotFound(path.display().to_string()))
    }
}

/// The resource scope of one run.
pub struct RunScope {
    deps: Deps,
    run_id: RunId,
    started_at: Timestamp,
    started: Instant,
    backend: Backend,
}

/// One measurement backend.
enum Backend {
    Cgroup(CgroupScope),
    Proc(ProcTreeScope),
}

impl RunScope {
    /// Starts measuring the process tree rooted at `root_pid`.
    ///
    /// On Linux the preferred cgroup v2 user scope is tried first; when it is unavailable
    /// the `/proc` tree walk is used. Other platforms use the `/proc` path only when the
    /// fixture provides it, otherwise the default.
    pub fn begin(deps: &Deps, run: &RunId, root_pid: u32) -> Result<RunScope, CoreError> {
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = CgroupScope::attach(deps, run, root_pid)? {
            return Ok(Self {
                deps: deps.clone(),
                run_id: run.clone(),
                started_at: deps.clock.now(),
                started: Instant::now(),
                backend: Backend::Cgroup(cgroup),
            });
        }
        Ok(Self::begin_with(deps, Arc::new(SystemProcFs), run, root_pid))
    }

    /// Starts a fixture-backed `/proc` scope. Tests use this with [`FakeProcFs`].
    pub fn begin_with(deps: &Deps, procfs: Arc<dyn ProcFs>, run: &RunId, root_pid: u32) -> RunScope {
        RunScope {
            deps: deps.clone(),
            run_id: run.clone(),
            started_at: deps.clock.now(),
            started: Instant::now(),
            backend: Backend::Proc(ProcTreeScope::new(procfs, root_pid)),
        }
    }

    /// Starts a fixture-backed cgroup scope. Tests use this with [`FakeProcFs`].
    pub fn begin_cgroup_with(deps: &Deps, procfs: Arc<dyn ProcFs>, run: &RunId, cgroup: PathBuf) -> RunScope {
        RunScope {
            deps: deps.clone(),
            run_id: run.clone(),
            started_at: deps.clock.now(),
            started: Instant::now(),
            backend: Backend::Cgroup(CgroupScope::fixture(procfs, cgroup)),
        }
    }

    /// Reads the current measurements.
    pub fn sample(&self) -> Result<ScopeSample, CoreError> {
        match &self.backend {
            Backend::Cgroup(cgroup) => cgroup.sample(self.started),
            Backend::Proc(proc) => proc.sample(self.started),
        }
    }

    /// Stops measuring and writes the `run_usage` row.
    pub fn finish(mut self, store: &Store, build_mode: BuildMode) -> Result<RunUsage, CoreError> {
        let (sample, source) = effective_sample(self.sample()?, self.source());
        self.stop();
        let ended_at = self.deps.clock.now();
        let run = if store.bound_project().is_some() {
            store.run(&self.run_id)?
        } else {
            None
        };
        let project = store
            .bound_project()
            .cloned()
            .or_else(|| ProjectName::parse("_node").ok())
            .unwrap_or_else(|| ProjectName::parse("_node").expect("reserved project name"));
        let (job_id, job_kind, role, harness, model, provider, node, started_at, exit_code) = match &run {
            Some(run) => {
                let job_kind = match &run.job_id {
                    Some(id) => store.job(id)?.map(|job| job.kind),
                    None => None,
                };
                (
                    run.job_id.clone(),
                    job_kind,
                    run.role,
                    run.executor.as_ref().map(|executor| executor.harness),
                    run.executor.as_ref().map(|executor| executor.model.clone()),
                    run.provider.clone(),
                    run.node.clone(),
                    run.started_at,
                    run.exit_code,
                )
            }
            None => (
                None,
                None,
                vowp_protocol::RunRole::Worker,
                None,
                None,
                None,
                self.deps.node.clone(),
                self.started_at,
                None,
            ),
        };
        let elapsed = ended_at.since(started_at);
        let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let usage = RunUsage {
            run_id: self.run_id.clone(),
            project,
            job_id,
            job_kind,
            role,
            harness,
            model,
            provider,
            node,
            started_at,
            ended_at,
            duration_ms,
            peak_rss_bytes: sample.peak_rss_bytes.max(1),
            anon_peak_bytes: sample.anon_peak_bytes,
            file_peak_bytes: sample.file_peak_bytes,
            kernel_peak_bytes: sample.kernel_peak_bytes,
            cpu_usec: sample.cpu_usec,
            mean_cpu_cores: sample.mean_cpu_cores,
            max_cpu_cores: sample.max_cpu_cores,
            build_mode,
            source,
            oom_killed: sample.oom_killed,
            exit_code,
        };
        super::usage::record(store, &usage)?;
        Ok(usage)
    }

    /// Returns the measurement source this scope uses.
    pub fn source(&self) -> UsageSource {
        match &self.backend {
            Backend::Cgroup(_) => UsageSource::Cgroup,
            Backend::Proc(_) => UsageSource::ProcTree,
        }
    }

    /// Stops a cgroup holder process, when one exists.
    fn stop(&mut self) {
        if let Backend::Cgroup(cgroup) = &mut self.backend {
            cgroup.stop();
        }
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A cgroup v2 scope path and its optional systemd holder.
struct CgroupScope {
    procfs: Arc<dyn ProcFs>,
    path: PathBuf,
    holder: Option<Child>,
    hold: Option<PathBuf>,
    state: Mutex<CgroupState>,
}

/// Cumulative state read from one cgroup.
struct CgroupState {
    peak: u64,
    anon: Option<u64>,
    file: Option<u64>,
    kernel: Option<u64>,
    cpu_usec: u64,
    last_cpu_usec: u64,
    last_sample: Instant,
    max_cores: f64,
    oom_killed: bool,
}

impl CgroupScope {
    /// Creates a fixture scope over an existing cgroup directory.
    fn fixture(procfs: Arc<dyn ProcFs>, path: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            procfs,
            path,
            holder: None,
            hold: None,
            state: Mutex::new(CgroupState {
                peak: 0,
                anon: None,
                file: None,
                kernel: None,
                cpu_usec: 0,
                last_cpu_usec: 0,
                last_sample: now,
                max_cores: 0.0,
                oom_killed: false,
            }),
        }
    }

    /// Attaches `root_pid` to a transient `systemd-run --user --scope` cgroup.
    #[cfg(target_os = "linux")]
    fn attach(deps: &Deps, run: &RunId, root_pid: u32) -> Result<Option<CgroupScope>, CoreError> {
        if !systemd_scope_available() {
            return Ok(None);
        }
        let unit = format!(
            "vowp-run-{}-{}",
            run.as_str().to_ascii_lowercase(),
            UNIT_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = deps.paths.tmp_dir();
        fs::create_dir_all(&tmp).map_err(|source| CoreError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
        let hold = tmp.join(format!("{unit}.hold"));
        fs::write(&hold, b"hold").map_err(|source| CoreError::Io {
            path: hold.display().to_string(),
            source,
        })?;
        let hold_arg = hold.display().to_string();
        let mut child = match Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--unit",
                &unit,
                "--",
                "sh",
                "-c",
                "echo $$; while [ -e \"$1\" ]; do sleep 0.05; done",
                "sh",
                &hold_arg,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                let _ = fs::remove_file(&hold);
                return Ok(None);
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&hold);
            return Ok(None);
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let read = reader.read_line(&mut line).map(|_| line);
            let _ = tx.send(read);
        });
        let holder_pid = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(line)) => line.trim().parse::<u32>().ok(),
            _ => None,
        };
        let Some(pid) = holder_pid else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&hold);
            return Ok(None);
        };
        let Ok(proc_cgroup) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&hold);
            return Ok(None);
        };
        let Some(relative) = proc_cgroup.lines().find_map(|line| line.strip_prefix("0::")) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&hold);
            return Ok(None);
        };
        let path = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        if fs::write(path.join("cgroup.procs"), root_pid.to_string()).is_err() {
            let _ = fs::remove_file(&hold);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        let now = Instant::now();
        Ok(Some(Self {
            procfs: Arc::new(SystemProcFs),
            path,
            holder: Some(child),
            hold: Some(hold),
            state: Mutex::new(CgroupState {
                peak: 0,
                anon: None,
                file: None,
                kernel: None,
                cpu_usec: 0,
                last_cpu_usec: 0,
                last_sample: now,
                max_cores: 0.0,
                oom_killed: false,
            }),
        }))
    }

    /// Reads one cgroup stat file, treating absence as zero.
    fn read_u64(&self, name: &str, key: Option<&str>) -> u64 {
        let path = self.path.join(name).display().to_string();
        let Ok(text) = self.procfs.read(&path) else {
            return 0;
        };
        match key {
            None => text.trim().parse().unwrap_or(0),
            Some(key) => text
                .lines()
                .find_map(|line| {
                    let mut parts = line.split_whitespace();
                    (parts.next() == Some(key))
                        .then(|| parts.next()?.parse().ok())
                        .flatten()
                })
                .unwrap_or(0),
        }
    }

    /// Reads one `memory.stat` field.
    fn read_memory_stat(&self, key: &str) -> Option<u64> {
        let path = self.path.join("memory.stat").display().to_string();
        self.procfs.read(&path).ok().and_then(|text| {
            text.lines().find_map(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some(key))
                    .then(|| parts.next()?.parse().ok())
                    .flatten()
            })
        })
    }

    /// Samples the cgroup and updates its peak state.
    fn sample(&self, started: Instant) -> Result<ScopeSample, CoreError> {
        let peak = self.read_u64("memory.peak", None);
        let cpu_usec = self.read_u64("cpu.stat", Some("usage_usec"));
        let oom_kill = self.read_u64("memory.events", Some("oom_kill"));
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if peak > state.peak {
            state.peak = peak;
            state.anon = self.read_memory_stat("anon");
            state.file = self.read_memory_stat("file");
            state.kernel = self.read_memory_stat("kernel");
        }
        if cpu_usec >= state.cpu_usec {
            state.cpu_usec = cpu_usec;
        }
        if oom_kill > 0 {
            state.oom_killed = true;
        }
        let elapsed = now.saturating_duration_since(state.last_sample).as_secs_f64();
        if elapsed > 0.0 {
            let delta = cpu_usec.saturating_sub(state.last_cpu_usec) as f64 / 1_000_000.0;
            state.max_cores = state.max_cores.max(delta / elapsed);
        }
        state.last_cpu_usec = cpu_usec;
        state.last_sample = now;
        let total_secs = now.saturating_duration_since(started).as_secs_f64();
        let mean = if total_secs > 0.0 {
            state.cpu_usec as f64 / 1_000_000.0 / total_secs
        } else {
            0.0
        };
        Ok(ScopeSample {
            peak_rss_bytes: state.peak,
            anon_peak_bytes: state.anon,
            file_peak_bytes: state.file,
            kernel_peak_bytes: state.kernel,
            cpu_usec: state.cpu_usec,
            mean_cpu_cores: mean,
            max_cpu_cores: state.max_cores.max(mean),
            oom_killed: state.oom_killed,
        })
    }

    /// Releases the holder process and its hold file.
    fn stop(&mut self) {
        if let Some(hold) = self.hold.take() {
            let _ = fs::remove_file(hold);
        }
        if let Some(mut child) = self.holder.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
    }
}

/// A `/proc` tree walker rooted at one pid.
struct ProcTreeScope {
    procfs: Arc<dyn ProcFs>,
    root_pid: u32,
    state: Mutex<ProcState>,
}

/// Cumulative state of the `/proc` tree walk.
struct ProcState {
    last_cpu: BTreeMap<u32, u64>,
    peak_rss: u64,
    anon: u64,
    file: u64,
    cpu_ticks: u64,
    max_cores: f64,
    last_sample: Instant,
}

impl ProcTreeScope {
    /// Creates a tree walker over `procfs`.
    fn new(procfs: Arc<dyn ProcFs>, root_pid: u32) -> Self {
        Self {
            procfs,
            root_pid,
            state: Mutex::new(ProcState {
                last_cpu: BTreeMap::new(),
                peak_rss: 0,
                anon: 0,
                file: 0,
                cpu_ticks: 0,
                max_cores: 0.0,
                last_sample: Instant::now(),
            }),
        }
    }

    /// Returns the live process tree, root first.
    fn tree(&self) -> Vec<u32> {
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = vec![self.root_pid];
        while let Some(pid) = queue.pop() {
            if !seen.insert(pid) || pid == 0 {
                continue;
            }
            ordered.push(pid);
            if let Ok(children) = self.procfs.children(pid) {
                queue.extend(children);
            }
        }
        ordered
    }

    /// Samples every live process in the tree.
    fn sample(&self, started: Instant) -> Result<ScopeSample, CoreError> {
        let mut rss = 0_u64;
        let mut anon = 0_u64;
        let mut file = 0_u64;
        let mut cpu_by_pid = BTreeMap::new();
        for pid in self.tree() {
            let status_path = format!("/proc/{pid}/status");
            let Ok(status) = self.procfs.read(&status_path) else {
                continue;
            };
            let mut live_rss = 0_u64;
            let mut live_anon = 0_u64;
            let mut live_file = 0_u64;
            for line in status.lines() {
                let mut parts = line.split_whitespace();
                let Some(key) = parts.next() else { continue };
                let value = parts.next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
                match key {
                    "VmRSS:" => live_rss = value.saturating_mul(1024),
                    "RssAnon:" => live_anon = value.saturating_mul(1024),
                    "RssFile:" => live_file = value.saturating_mul(1024),
                    _ => {}
                }
            }
            rss = rss.saturating_add(live_rss);
            anon = anon.saturating_add(live_anon);
            file = file.saturating_add(live_file);
            let stat_path = format!("/proc/{pid}/stat");
            if let Ok(stat) = self.procfs.read(&stat_path)
                && let Some(close) = stat.rfind(')')
            {
                let rest = &stat[close + 1..];
                let mut fields = rest.split_whitespace();
                let utime = fields.nth(11).and_then(|value| value.parse::<u64>().ok());
                let stime = fields.next().and_then(|value| value.parse::<u64>().ok());
                if let (Some(utime), Some(stime)) = (utime, stime) {
                    cpu_by_pid.insert(pid, utime.saturating_add(stime));
                }
            }
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut delta_ticks = 0_u64;
        for (pid, ticks) in &cpu_by_pid {
            let previous = state.last_cpu.get(pid).copied().unwrap_or(0);
            if *ticks >= previous {
                delta_ticks = delta_ticks.saturating_add(*ticks - previous);
            }
        }
        state.cpu_ticks = state.cpu_ticks.saturating_add(delta_ticks);
        state.last_cpu = cpu_by_pid;
        let elapsed = now.saturating_duration_since(state.last_sample).as_secs_f64();
        if elapsed > 0.0 {
            let cores = delta_ticks as f64 / CLOCK_TICKS / elapsed;
            state.max_cores = state.max_cores.max(cores);
        }
        state.last_sample = now;
        state.peak_rss = state.peak_rss.max(rss);
        if rss >= state.peak_rss {
            state.anon = anon;
            state.file = file;
        }
        let total_secs = now.saturating_duration_since(started).as_secs_f64();
        let mean = if total_secs > 0.0 {
            state.cpu_ticks as f64 / CLOCK_TICKS / total_secs
        } else {
            0.0
        };
        Ok(ScopeSample {
            peak_rss_bytes: state.peak_rss,
            anon_peak_bytes: (state.anon > 0).then_some(state.anon),
            file_peak_bytes: (state.file > 0).then_some(state.file),
            kernel_peak_bytes: None,
            cpu_usec: state.cpu_ticks.saturating_mul(10_000),
            mean_cpu_cores: mean,
            max_cpu_cores: state.max_cores.max(mean),
            oom_killed: false,
        })
    }
}

/// Infers `cold`, `warm` or `incremental` from a job directory's Cargo target.
pub fn infer_build_mode(cwd: &Path) -> BuildMode {
    if !cwd.join("Cargo.toml").is_file() {
        return BuildMode::Unknown;
    }
    let target = cwd.join("target");
    if !target.is_dir() {
        return BuildMode::Cold;
    }
    for profile in ["debug", "release", "test"] {
        let incremental = target.join(profile).join("incremental");
        if directory_has_entries(&incremental) {
            return BuildMode::Incremental;
        }
    }
    for profile in ["debug", "release", "test"] {
        let deps = target.join(profile).join("deps");
        if directory_has_entries(&deps) {
            return BuildMode::Warm;
        }
    }
    BuildMode::Cold
}

/// Returns true when `path` is a directory with at least one entry.
fn directory_has_entries(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .is_some_and(|mut entries| entries.next().is_some())
}

/// One command measured through a cgroup scope.
pub(crate) struct ScopedShell {
    /// The command's output.
    pub output: CommandOutput,
    /// The measurement, when the cgroup path worked.
    pub sample: Option<ScopeSample>,
    /// The measurement source.
    pub source: UsageSource,
    /// Start of the command.
    pub started_at: Timestamp,
    /// End of the command.
    pub ended_at: Timestamp,
}

/// Runs one shell command, preferably inside a transient cgroup user scope.
pub(crate) fn run_scoped_shell(
    deps: &Deps,
    run: &str,
    cwd: &Path,
    timeout_secs: u64,
    unit_hint: &str,
) -> Result<ScopedShell, CoreError> {
    let started_at = deps.clock.now();
    let timeout = Duration::from_secs(timeout_secs.max(1));
    if !systemd_scope_available() {
        let output = deps.cmd.run(&CommandSpec {
            shell: Some(run.to_owned()),
            argv: Vec::new(),
            cwd: Some(cwd.to_path_buf()),
            env: Vec::new(),
            timeout,
        })?;
        return Ok(ScopedShell {
            output,
            sample: None,
            source: UsageSource::Default,
            started_at,
            ended_at: deps.clock.now(),
        });
    }

    let tmp = deps.paths.tmp_dir();
    fs::create_dir_all(&tmp).map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    let unit = format!(
        "vowp-{}-{}-{}",
        sanitize_unit(unit_hint),
        std::process::id(),
        UNIT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let script = tmp.join(format!("{unit}.sh"));
    let stats = tmp.join(format!("{unit}.stats"));
    let body = format!(
        "#!/bin/sh\nsh -c {}\nrc=$?\n\
         cg=$(sed -n 's/^0:://p' /proc/self/cgroup)\n\
         root=\"/sys/fs/cgroup$cg\"\n\
         peak=$(cat \"$root/memory.peak\" 2>/dev/null || echo 0)\n\
         usage=$(awk '$1 == \"usage_usec\" {{print $2}}' \"$root/cpu.stat\" 2>/dev/null || echo 0)\n\
         oom=$(awk '$1 == \"oom_kill\" {{print $2}}' \"$root/memory.events\" 2>/dev/null || echo 0)\n\
         printf '%s %s %s\\n' \"$peak\" \"$usage\" \"$oom\" > \"$1\"\n\
         exit $rc\n",
        shell_quote(run)
    );
    fs::write(&script, body).map_err(|source| CoreError::Io {
        path: script.display().to_string(),
        source,
    })?;
    let output_result = deps.cmd.run(&CommandSpec {
        shell: None,
        argv: vec![
            "systemd-run".to_owned(),
            "--user".to_owned(),
            "--scope".to_owned(),
            "--quiet".to_owned(),
            "--unit".to_owned(),
            unit.clone(),
            "--".to_owned(),
            "sh".to_owned(),
            script.display().to_string(),
            stats.display().to_string(),
        ],
        cwd: Some(cwd.to_path_buf()),
        env: Vec::new(),
        timeout,
    });
    let sample = fs::read_to_string(&stats).ok().and_then(|text| {
        output_result
            .as_ref()
            .ok()
            .and_then(|output| parse_gate_stats(&text, output))
    });
    let _ = fs::remove_file(&script);
    let _ = fs::remove_file(&stats);
    let output = output_result?;
    Ok(ScopedShell {
        sample: sample.clone(),
        source: if sample.is_some() {
            UsageSource::Cgroup
        } else {
            UsageSource::Default
        },
        output,
        started_at,
        ended_at: deps.clock.now(),
    })
}

/// Parses the three numbers the wrapper writes after a scoped command.
fn parse_gate_stats(text: &str, output: &CommandOutput) -> Option<ScopeSample> {
    let mut parts = text.split_whitespace();
    let peak = parts.next()?.parse::<u64>().ok()?;
    let cpu_usec = parts.next()?.parse::<u64>().ok()?;
    let oom = parts.next()?.parse::<u64>().ok()?;
    let seconds = output.duration.as_secs_f64();
    let mean = if seconds > 0.0 {
        cpu_usec as f64 / 1_000_000.0 / seconds
    } else {
        0.0
    };
    Some(ScopeSample {
        peak_rss_bytes: peak,
        anon_peak_bytes: None,
        file_peak_bytes: None,
        kernel_peak_bytes: None,
        cpu_usec,
        mean_cpu_cores: mean,
        max_cpu_cores: mean,
        oom_killed: oom > 0,
    })
}

/// Quotes one shell word for `/bin/sh`.
fn shell_quote(text: &str) -> String {
    let mut quoted = String::from("'");
    for character in text.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// Returns a systemd-safe unit fragment.
fn sanitize_unit(text: &str) -> String {
    let mut sanitized = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            sanitized.push(character.to_ascii_lowercase());
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
        if sanitized.len() >= 40 {
            break;
        }
    }
    sanitized.trim_matches('-').to_owned()
}

/// Returns true when a transient `systemd-run --user --scope` can be created and observed.
///
/// The adapter owns the one cached probe so that core scope measurement and adapter sidecars
/// agree on whether the cgroup path is usable.
fn systemd_scope_available() -> bool {
    vowp_adapters::proc::cgroup_scopes_available()
}
