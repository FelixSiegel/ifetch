use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Single point-in-time resource sample of the target `ifetch --server` process.
#[derive(Debug, Clone)]
pub struct ProcSample {
    pub sampled_at: Instant,
    pub cpu_percent: f64,
    pub total_cpu_ticks: u64,
    pub rss_kb: u64,
    pub hwm_kb: u64,
    pub vsize_kb: u64,
    pub threads: u64,
    pub voluntary_ctxt_switches: u64,
    pub nonvoluntary_ctxt_switches: u64,
}

/// Aggregated resource statistics over a time window or phase.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResourceStats {
    pub phase_name: String,
    pub duration: Duration,
    pub sample_count: usize,
    pub cpu_avg_pct: f64,
    pub cpu_p95_pct: f64,
    pub cpu_max_pct: f64,
    pub cpu_time_ms: f64,
    pub rss_min_kb: u64,
    pub rss_p50_kb: u64,
    pub rss_p95_kb: u64,
    pub rss_avg_kb: u64,
    pub rss_max_kb: u64,
    pub hwm_kb: u64,
    pub vsize_max_kb: u64,
    pub min_threads: u64,
    pub max_threads: u64,
    pub voluntary_switches_delta: u64,
    pub nonvoluntary_switches_delta: u64,
}

/// Continuously samples `/proc/<pid>/stat` and `/proc/<pid>/status` on a background thread.
pub struct ProcessMonitor {
    pid: u32,
    stop_flag: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<ProcSample>>>,
    phase_stats: Arc<Mutex<Vec<ResourceStats>>>,
    handle: Option<JoinHandle<()>>,
}

impl ProcessMonitor {
    pub fn start(pid: u32, sample_interval: Duration) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::with_capacity(4096)));
        let phase_stats = Arc::new(Mutex::new(Vec::new()));

        if let Some(raw) = read_proc_raw(pid) {
            if let Ok(mut guard) = samples.lock() {
                guard.push(ProcSample {
                    sampled_at: Instant::now(),
                    cpu_percent: 0.0,
                    total_cpu_ticks: raw.cpu_ticks,
                    rss_kb: raw.rss_kb,
                    hwm_kb: raw.hwm_kb,
                    vsize_kb: raw.vsize_kb,
                    threads: raw.threads,
                    voluntary_ctxt_switches: raw.voluntary_ctxt_switches,
                    nonvoluntary_ctxt_switches: raw.nonvoluntary_ctxt_switches,
                });
            }
        }

        let stop_clone = Arc::clone(&stop_flag);
        let samples_clone = Arc::clone(&samples);

        let handle = thread::Builder::new()
            .name("testbench-proc-monitor".to_string())
            .spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    thread::sleep(sample_interval);
                    if !record_sample_into(pid, &samples_clone) {
                        break;
                    }
                }
            })
            .expect("Failed to spawn process monitor thread");

        Self {
            pid,
            stop_flag,
            samples,
            phase_stats,
            handle: Some(handle),
        }
    }

    /// Returns the current sample index so a phase can measure its exact slice of samples.
    pub fn mark_phase_start(&self) -> (usize, Instant) {
        let _ = record_sample_into(self.pid, &self.samples);
        let idx = self
            .samples
            .lock()
            .map(|s| s.len().saturating_sub(1))
            .unwrap_or(0);
        (idx, Instant::now())
    }

    /// Forces an immediate sample and computes aggregated `ResourceStats` for the completed phase.
    pub fn record_phase(
        &self,
        name: impl Into<String>,
        start_marker: (usize, Instant),
    ) -> ResourceStats {
        let _ = record_sample_into(self.pid, &self.samples);
        let (start_idx, start_inst) = start_marker;
        let duration = start_inst.elapsed();
        let phase_name = name.into();

        let stats = {
            let guard = self.samples.lock().unwrap();
            let slice = if start_idx < guard.len() {
                &guard[start_idx..]
            } else {
                &guard[..]
            };
            compute_stats(&phase_name, duration, slice)
        };

        if let Ok(mut phases) = self.phase_stats.lock() {
            phases.push(stats.clone());
        }
        stats
    }

    /// Stops the background sampler and returns `(overall_stats, per_phase_stats)`.
    pub fn stop(mut self, total_duration: Duration) -> (ResourceStats, Vec<ResourceStats>) {
        let _ = record_sample_into(self.pid, &self.samples);
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }

        let overall = {
            let guard = self.samples.lock().unwrap();
            compute_stats("TOTAL SERVER RUN", total_duration, &guard)
        };
        let phases = self.phase_stats.lock().unwrap().clone();
        (overall, phases)
    }
}

fn record_sample_into(pid: u32, samples: &Mutex<Vec<ProcSample>>) -> bool {
    let now = Instant::now();
    let Some(raw) = read_proc_raw(pid) else {
        return false;
    };

    if let Ok(mut guard) = samples.lock() {
        let cpu_percent = if let Some(prev) = guard.last() {
            let dt_secs = now.saturating_duration_since(prev.sampled_at).as_secs_f64();
            if dt_secs >= 0.010 {
                let delta_ticks = raw.cpu_ticks.saturating_sub(prev.total_cpu_ticks);
                let cpu_secs = (delta_ticks as f64) / 100.0;
                (cpu_secs / dt_secs) * 100.0
            } else {
                prev.cpu_percent
            }
        } else {
            0.0
        };

        guard.push(ProcSample {
            sampled_at: now,
            cpu_percent,
            total_cpu_ticks: raw.cpu_ticks,
            rss_kb: raw.rss_kb,
            hwm_kb: raw.hwm_kb,
            vsize_kb: raw.vsize_kb,
            threads: raw.threads,
            voluntary_ctxt_switches: raw.voluntary_ctxt_switches,
            nonvoluntary_ctxt_switches: raw.nonvoluntary_ctxt_switches,
        });
    }
    true
}

struct RawProcData {
    cpu_ticks: u64,
    rss_kb: u64,
    hwm_kb: u64,
    vsize_kb: u64,
    threads: u64,
    voluntary_ctxt_switches: u64,
    nonvoluntary_ctxt_switches: u64,
}

fn read_proc_raw(pid: u32) -> Option<RawProcData> {
    let stat_str = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rparen = stat_str.rfind(')')?;
    let after = stat_str.get(rparen + 2..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    if fields.len() < 22 {
        return None;
    }

    let utime: u64 = fields[11].parse().ok()?;
    let stime: u64 = fields[12].parse().ok()?;
    let threads_stat: u64 = fields[17].parse().unwrap_or(1);
    let vsize_bytes: u64 = fields[20].parse().unwrap_or(0);
    let rss_pages: u64 = fields[21].parse().unwrap_or(0);

    let mut rss_kb = rss_pages * 4;
    let mut hwm_kb = rss_kb;
    let mut vsize_kb = vsize_bytes / 1024;
    let mut threads = threads_stat;
    let mut voluntary = 0;
    let mut nonvoluntary = 0;

    if let Ok(status_str) = fs::read_to_string(format!("/proc/{}/status", pid)) {
        for line in status_str.lines() {
            if let Some(val) = line.strip_prefix("VmRSS:") {
                rss_kb = parse_kb(val).unwrap_or(rss_kb);
            } else if let Some(val) = line.strip_prefix("VmHWM:") {
                hwm_kb = parse_kb(val).unwrap_or(hwm_kb);
            } else if let Some(val) = line.strip_prefix("VmSize:") {
                vsize_kb = parse_kb(val).unwrap_or(vsize_kb);
            } else if let Some(val) = line.strip_prefix("Threads:") {
                threads = val.trim().parse().unwrap_or(threads);
            }
        }
    }

    if let Ok(tasks) = fs::read_dir(format!("/proc/{}/task", pid)) {
        for task in tasks.flatten() {
            if let Ok(t_status) = fs::read_to_string(task.path().join("status")) {
                for line in t_status.lines() {
                    if let Some(val) = line.strip_prefix("voluntary_ctxt_switches:") {
                        voluntary += val.trim().parse::<u64>().unwrap_or(0);
                    } else if let Some(val) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
                        nonvoluntary += val.trim().parse::<u64>().unwrap_or(0);
                    }
                }
            }
        }
    }

    Some(RawProcData {
        cpu_ticks: utime + stime,
        rss_kb,
        hwm_kb: hwm_kb.max(rss_kb),
        vsize_kb,
        threads,
        voluntary_ctxt_switches: voluntary,
        nonvoluntary_ctxt_switches: nonvoluntary,
    })
}

fn parse_kb(raw: &str) -> Option<u64> {
    raw.split_whitespace().next()?.parse().ok()
}

fn compute_stats(phase_name: &str, duration: Duration, samples: &[ProcSample]) -> ResourceStats {
    if samples.is_empty() {
        return ResourceStats {
            phase_name: phase_name.to_string(),
            duration,
            sample_count: 0,
            cpu_avg_pct: 0.0,
            cpu_p95_pct: 0.0,
            cpu_max_pct: 0.0,
            cpu_time_ms: 0.0,
            rss_min_kb: 0,
            rss_p50_kb: 0,
            rss_p95_kb: 0,
            rss_avg_kb: 0,
            rss_max_kb: 0,
            hwm_kb: 0,
            vsize_max_kb: 0,
            min_threads: 0,
            max_threads: 0,
            voluntary_switches_delta: 0,
            nonvoluntary_switches_delta: 0,
        };
    }

    let first = &samples[0];
    let last = &samples[samples.len() - 1];
    let delta_ticks = last.total_cpu_ticks.saturating_sub(first.total_cpu_ticks);
    let cpu_time_ms = (delta_ticks as f64) * 10.0; // 100 Hz = 10ms per tick

    let dur_secs = duration.as_secs_f64();
    let cpu_avg_pct = if dur_secs > 0.001 {
        ((delta_ticks as f64 / 100.0) / dur_secs) * 100.0
    } else {
        samples.iter().map(|s| s.cpu_percent).sum::<f64>() / (samples.len() as f64)
    };

    let mut cpu_vals: Vec<f64> = samples.iter().map(|s| s.cpu_percent).collect();
    cpu_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_idx = ((cpu_vals.len() as f64 * 0.95) as usize).min(cpu_vals.len() - 1);
    let cpu_p95_pct = cpu_vals[p95_idx].max(cpu_avg_pct);
    let cpu_max_pct = cpu_vals.last().copied().unwrap_or(0.0).max(cpu_avg_pct);

    let mut rss_vals: Vec<u64> = samples.iter().map(|s| s.rss_kb).collect();
    rss_vals.sort_unstable();
    let rss_min_kb = rss_vals.first().copied().unwrap_or(0);
    let rss_max_kb = rss_vals.last().copied().unwrap_or(0);
    let rss_p50_kb = rss_vals[((rss_vals.len() as f64 * 0.50) as usize).min(rss_vals.len() - 1)];
    let rss_p95_kb = rss_vals[((rss_vals.len() as f64 * 0.95) as usize).min(rss_vals.len() - 1)];
    let rss_avg_kb = samples.iter().map(|s| s.rss_kb).sum::<u64>() / (samples.len() as u64);
    let hwm_kb = samples.iter().map(|s| s.hwm_kb).max().unwrap_or(rss_max_kb);
    let vsize_max_kb = samples.iter().map(|s| s.vsize_kb).max().unwrap_or(0);
    let min_threads = samples.iter().map(|s| s.threads).min().unwrap_or(1);
    let max_threads = samples.iter().map(|s| s.threads).max().unwrap_or(1);

    let voluntary_switches_delta = last
        .voluntary_ctxt_switches
        .saturating_sub(first.voluntary_ctxt_switches);
    let nonvoluntary_switches_delta = last
        .nonvoluntary_ctxt_switches
        .saturating_sub(first.nonvoluntary_ctxt_switches);

    ResourceStats {
        phase_name: phase_name.to_string(),
        duration,
        sample_count: samples.len(),
        cpu_avg_pct,
        cpu_p95_pct,
        cpu_max_pct,
        cpu_time_ms,
        rss_min_kb,
        rss_p50_kb,
        rss_p95_kb,
        rss_avg_kb,
        rss_max_kb,
        hwm_kb,
        vsize_max_kb,
        min_threads,
        max_threads,
        voluntary_switches_delta,
        nonvoluntary_switches_delta,
    }
}
