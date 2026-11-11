//! Host sampling: `/proc/meminfo`, load, cgroup quota, disk free and pressure.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1. Linux reads `/proc`; when those files
//! are unavailable the sample is marked `fallback` and only the portable values are filled.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vowp_protocol::{HostSample, Level, SampleSource, Timestamp};
use vowp_store::{NodePaths, Store};

use crate::CoreError;
use crate::deps::Deps;

use super::scope::{ProcFs, SystemProcFs};

/// The production sampling interval.
pub const SAMPLE_INTERVAL_SECS: u64 = 5;

/// Samples the node and stores one row.
pub fn sample(deps: &Deps, store: &Store, now: Timestamp) -> Result<Option<HostSample>, CoreError> {
    if let Err(error) = super::usage::import_sidecars(deps, store) {
        log(deps, Level::Warn, &format!("could not import adapter usage: {error}"));
    }
    let sample = sample_host(deps)?;
    store.insert_host_sample(&sample)?;
    store.prune_host_samples(
        now.minus(Duration::from_secs(24 * 60 * 60)),
        now.minus(Duration::from_secs(30 * 24 * 60 * 60)),
    )?;
    Ok(Some(sample))
}

/// Reads one host sample without storing it.
pub fn sample_host(deps: &Deps) -> Result<HostSample, CoreError> {
    let worktrees = deps.config.project.worktree_root.as_deref().map(PathBuf::from);
    sample_with(
        &SystemProcFs,
        &deps.paths,
        worktrees.as_deref(),
        deps.clock.now(),
        SAMPLE_INTERVAL_SECS,
    )
}

/// Reads one host sample through `procfs`.
///
/// This is the test seam: a [`FakeProcFs`](super::scope::FakeProcFs) supplies the pseudo-files
/// and the disk numbers, so no test reads the real machine.
pub fn sample_with(
    procfs: &dyn ProcFs,
    paths: &NodePaths,
    worktrees: Option<&Path>,
    now: Timestamp,
    interval_secs: u64,
) -> Result<HostSample, CoreError> {
    let meminfo = procfs.read("/proc/meminfo").ok();
    let loadavg = procfs.read("/proc/loadavg").ok();
    let source = if meminfo.is_some() && loadavg.is_some() {
        SampleSource::Proc
    } else {
        SampleSource::Fallback
    };
    let (mem_total_bytes, mem_available_bytes, swap_used_bytes) =
        meminfo.as_deref().map(parse_meminfo).unwrap_or((0, 0, 0));
    let (load1, load5, load15) = loadavg.as_deref().map(parse_loadavg).unwrap_or((0.0, 0.0, 0.0));
    let cpu_count = parse_cpu_count(procfs).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|count| u32::try_from(count.get()).unwrap_or(u32::MAX))
            .unwrap_or(1)
    });
    let cpu_quota_cores = procfs
        .read("/sys/fs/cgroup/cpu.max")
        .ok()
        .and_then(|text| parse_cpu_max(&text));
    let disk_free_home_bytes = procfs.disk_free(paths.root()).unwrap_or(0);
    let disk_free_worktrees_bytes = worktrees
        .filter(|path| *path != paths.root())
        .and_then(|path| procfs.disk_free(path).ok());
    let pressure_cpu_avg10 = read_pressure(procfs, "cpu");
    let pressure_mem_avg10 = read_pressure(procfs, "memory");
    let pressure_io_avg10 = read_pressure(procfs, "io");
    Ok(HostSample {
        at: now,
        interval_secs,
        mem_total_bytes,
        mem_available_bytes,
        swap_used_bytes,
        load1,
        load5,
        load15,
        cpu_count,
        cpu_quota_cores,
        disk_free_home_bytes,
        disk_free_worktrees_bytes,
        pressure_cpu_avg10,
        pressure_mem_avg10,
        pressure_io_avg10,
        source,
    })
}

/// Parses the three memory fields from `/proc/meminfo` (values are KiB on Linux).
fn parse_meminfo(text: &str) -> (u64, u64, u64) {
    let mut total = 0_u64;
    let mut available = 0_u64;
    let mut swap_total = 0_u64;
    let mut swap_free = 0_u64;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let value = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024);
        match key {
            "MemTotal:" => total = value,
            "MemAvailable:" => available = value,
            "SwapTotal:" => swap_total = value,
            "SwapFree:" => swap_free = value,
            _ => {}
        }
    }
    (total, available, swap_total.saturating_sub(swap_free))
}

/// Parses the three load averages from `/proc/loadavg`.
fn parse_loadavg(text: &str) -> (f64, f64, f64) {
    let mut parts = text.split_whitespace().filter_map(|value| value.parse::<f64>().ok());
    (
        parts.next().unwrap_or(0.0),
        parts.next().unwrap_or(0.0),
        parts.next().unwrap_or(0.0),
    )
}

/// Counts processors in `/proc/cpuinfo`.
fn parse_cpu_count(procfs: &dyn ProcFs) -> Option<u32> {
    let text = procfs.read("/proc/cpuinfo").ok()?;
    let count = text
        .lines()
        .filter(|line| line.split_once(':').is_some_and(|(key, _)| key.trim() == "processor"))
        .count();
    (count > 0).then(|| u32::try_from(count).unwrap_or(u32::MAX))
}

/// Parses cgroup v2 `cpu.max` (`"max 100000"` or `"200000 100000"`).
fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut parts = text.split_whitespace();
    let quota = parts.next()?;
    if quota == "max" {
        return None;
    }
    let quota = quota.parse::<f64>().ok()?;
    let period = parts
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(100_000.0);
    (period > 0.0).then_some(quota / period)
}

/// Reads one pressure file's `avg10` value.
fn read_pressure(procfs: &dyn ProcFs, resource: &str) -> Option<f64> {
    let text = procfs.read(&format!("/proc/pressure/{resource}")).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("some"))
            .then(|| parts.find_map(|field| field.strip_prefix("avg10=")?.parse::<f64>().ok()))?
    })
}

/// A running sampler thread.
pub struct SamplerHandle {
    stop: Option<Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl SamplerHandle {
    /// Starts one sampler thread writing `host_samples` every five seconds until stopped.
    pub fn start(deps: &Deps) -> Result<SamplerHandle, CoreError> {
        Self::start_with_interval(deps, Duration::from_secs(SAMPLE_INTERVAL_SECS))
    }

    /// Starts a sampler thread with an explicit interval; tests use a short one.
    pub fn start_with_interval(deps: &Deps, interval: Duration) -> Result<SamplerHandle, CoreError> {
        let deps = deps.clone();
        let (stop, receiver) = channel();
        let join = thread::Builder::new()
            .name("vowp-capacity-sampler".to_owned())
            .spawn(move || run_sampler(deps, receiver, interval))
            .map_err(|source| CoreError::Io {
                path: "vowp-capacity-sampler".to_owned(),
                source,
            })?;
        Ok(SamplerHandle {
            stop: Some(stop),
            join: Some(join),
        })
    }

    /// Stops the sampler thread and waits for it to finish.
    pub fn stop(mut self) -> Result<(), CoreError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            return Err(CoreError::Command("capacity sampler thread panicked".to_owned()));
        }
        Ok(())
    }
}

impl Drop for SamplerHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The sampler thread body: sample immediately, then on each interval.
fn run_sampler(deps: Deps, stop: Receiver<()>, interval: Duration) {
    loop {
        let now = deps.clock.now();
        match deps.open_store() {
            Ok(store) => {
                if let Err(error) = sample(&deps, &store, now) {
                    log(&deps, Level::Warn, &format!("capacity sample failed: {error}"));
                }
            }
            Err(error) => log(&deps, Level::Warn, &format!("capacity sampler store: {error}")),
        }
        match stop.recv_timeout(interval) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Writes one line to the injected log.
fn log(deps: &Deps, level: Level, message: &str) {
    deps.log.log(level, "capacity", message);
}
