//! ENT-076: `dowai benchmark` runs a workload several times and reports duration, memory and the
//! host it ran on. The measurements are taken by the host, never by user code.
//!
//! Peak memory is the largest child process (a Python worker) seen by this process, read with
//! `getrusage(RUSAGE_CHILDREN)`. On Linux it is the resident set size; elsewhere it is not
//! measured and reported as `null`.

use crate::{effective_configuration, run_postgres, RunRequest};
use dowai_core::{DowaiError, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Schema of the benchmark report (`dowai benchmark --json`).
pub const BENCHMARK_SCHEMA_VERSION: u32 = 1;

/// Largest number of repetitions accepted by one benchmark.
pub const MAX_REPEAT: u32 = 20;

/// Hardware and software the workload ran on, as the host reports them.
#[derive(Clone, Debug, Serialize)]
pub struct HostInfo {
    pub os: String,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub logical_cpus: Option<usize>,
    pub mem_total_kib: Option<u64>,
}

/// One repetition of the workload.
#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkRun {
    pub run_id: String,
    pub status: String,
    pub duration_ms: u64,
    pub nodes: usize,
}

/// Duration summary over the repetitions, in milliseconds.
#[derive(Clone, Debug, Serialize)]
pub struct DurationSummary {
    pub min: u64,
    pub median: u64,
    pub max: u64,
}

/// Full benchmark report.
#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub engine_version: String,
    pub host: HostInfo,
    pub workload: Vec<String>,
    pub repeat: u32,
    pub runs: Vec<BenchmarkRun>,
    pub duration_ms: DurationSummary,
    /// Largest child process resident set, in KiB, over the whole benchmark (Linux only).
    pub peak_rss_kib: Option<u64>,
    /// `runtime.max_memory_mb` of the effective configuration (RLIMIT_AS per worker, ENT-038).
    pub memory_limit_mb: Option<u64>,
    /// Whether `peak_rss_kib` stayed within `memory_limit_mb`; `null` when either is unknown.
    pub within_memory_limit: Option<bool>,
    pub notes: Vec<String>,
}

/// Runs `targets` `repeat` times in the project and reports the measurements.
pub fn benchmark(project_dir: &Path, targets: Vec<String>, repeat: u32) -> Result<BenchmarkReport> {
    if repeat == 0 || repeat > MAX_REPEAT {
        return Err(DowaiError::Config(format!(
            "benchmark repeat must be between 1 and {MAX_REPEAT}"
        )));
    }
    if targets.is_empty() {
        return Err(DowaiError::Config(
            "benchmark needs at least one target".to_owned(),
        ));
    }
    let configuration = effective_configuration(project_dir)?;
    let memory_limit_mb = configuration["effective"]["runtime"]["max_memory_mb"].as_u64();

    let mut runs = Vec::with_capacity(repeat as usize);
    for _ in 0..repeat {
        let result = run_postgres(
            project_dir,
            RunRequest {
                targets: targets.clone(),
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )?;
        runs.push(BenchmarkRun {
            run_id: result.run_id,
            status: result.status,
            duration_ms: result.duration_ms,
            nodes: result.nodes.len(),
        });
    }

    let peak_rss_kib = children_peak_rss_kib();
    let within_memory_limit = match (peak_rss_kib, memory_limit_mb) {
        (Some(peak), Some(limit)) => Some(peak <= limit.saturating_mul(1024)),
        _ => None,
    };
    let mut durations: Vec<u64> = runs.iter().map(|run| run.duration_ms).collect();
    durations.sort_unstable();
    let summary = DurationSummary {
        min: durations[0],
        median: durations[durations.len() / 2],
        max: durations[durations.len() - 1],
    };
    Ok(BenchmarkReport {
        schema_version: BENCHMARK_SCHEMA_VERSION,
        engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        host: host_info(),
        workload: targets,
        repeat,
        runs,
        duration_ms: summary,
        peak_rss_kib,
        memory_limit_mb,
        within_memory_limit,
        notes: vec![
            "peak_rss_kib is the largest child process (Python worker) of the benchmark process; it is not a per-repetition value".to_owned(),
            "runtime.max_memory_mb is enforced as RLIMIT_AS per worker: virtual memory, not a physical RAM cap".to_owned(),
            "a workload larger than the memory limit only fits if the model streams it (ENT-050, ENT-057)".to_owned(),
        ],
    })
}

/// Largest child resident set of this process, in KiB. `None` when the platform does not report it.
#[cfg(target_os = "linux")]
pub fn children_peak_rss_kib() -> Option<u64> {
    // SAFETY: `getrusage` only writes into the zeroed `rusage` it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    if status != 0 {
        return None;
    }
    // On Linux `ru_maxrss` is reported in kilobytes.
    u64::try_from(usage.ru_maxrss).ok()
}

/// Largest child resident set of this process, in KiB. Not measured on this platform.
#[cfg(not(target_os = "linux"))]
pub fn children_peak_rss_kib() -> Option<u64> {
    None
}

/// Host description read from the operating system; unknown fields are `None`.
pub fn host_info() -> HostInfo {
    HostInfo {
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        kernel: read_first_line("/proc/sys/kernel/osrelease"),
        cpu_model: read_cpu_model(),
        logical_cpus: std::thread::available_parallelism().ok().map(|n| n.get()),
        mem_total_kib: read_mem_total_kib(),
    }
}

fn read_first_line(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.lines().next().map(|line| line.trim().to_owned()))
        .filter(|line| !line.is_empty())
}

fn read_cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_owned())
}

fn read_mem_total_kib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}
